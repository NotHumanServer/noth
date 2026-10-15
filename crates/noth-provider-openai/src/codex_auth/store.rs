use std::path::{Path, PathBuf};
use std::time::Duration;

use noth_core::{Error, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::jwt::{jwt_exp, now_secs};
use super::oauth::refresh;
use super::{RELOGIN_HINT, needs_refresh};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub last_refresh: String,
}

// ログやパニック出力に認証情報を出さない
impl std::fmt::Debug for StoredTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredTokens")
            .field("access_token", &"***")
            .field("refresh_token", &"***")
            .field("last_refresh", &self.last_refresh)
            .finish()
    }
}

pub fn load(path: &Path) -> Result<StoredTokens> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        Error::Config(format!(
            "provider.openai: Codex の認証ファイル {} を読めません({e})。{RELOGIN_HINT}",
            path.display()
        ))
    })?;
    serde_json::from_str(&raw).map_err(|e| {
        Error::Config(format!(
            "provider.openai: Codex の認証ファイルが壊れています({e})。{RELOGIN_HINT}"
        ))
    })
}

/// 更新中のクラッシュで refresh_token ごと失うと再ログインになるため、一時ファイル経由で置き換える
pub fn save(path: &Path, tokens: &StoredTokens) -> Result<()> {
    use std::io::Write;
    let io = |e: std::io::Error| Error::Other(format!("Codex の認証ファイルを保存できません: {e}"));
    if let Some(dir) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir).map_err(io)?;
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".tmp{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let body = serde_json::to_vec_pretty(tokens)?;
    let written = opts.open(&tmp).and_then(|mut f| {
        f.write_all(&body)?;
        f.sync_all()
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(e));
    }
    Ok(())
}

/// 別プロセス(常駐サービスと `noth doctor` など)が同時に更新して、使い切りの refresh_token を
/// 二重に使わないための排他。排他作成したファイルをロックとみなす
pub(super) struct FileLock {
    path: PathBuf,
}

/// 更新リクエストのタイムアウト(`post_form`)より長くし、生きているプロセスのロックは奪わない
const LOCK_STALE: Duration = Duration::from_secs(90);
const LOCK_WAIT: Duration = Duration::from_secs(60);

impl FileLock {
    pub(super) async fn acquire(target: &Path) -> Result<Self> {
        let mut name = target.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        let path = target.with_file_name(name);
        let deadline = tokio::time::Instant::now() + LOCK_WAIT;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // 保持側がクラッシュしたロックは mtime で見切って奪う
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                }
                Err(e) => {
                    return Err(Error::Other(format!("Codex の認証ロックを作れません: {e}")));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Transport(
                    "Codex の認証ロックを取得できませんでした(別プロセスが更新中)".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// worker と本線が同じトークンを共有するため、更新はロックして1回にまとめる
pub struct TokenStore {
    path: PathBuf,
    client: reqwest::Client,
    /// 更新に成功したがファイルへ保存できなかったトークン。旧 refresh_token は使用済みで
    /// ファイルに残すと再ログインになるため、保存に成功するまでメモリで持ち続ける
    pub(super) state: Mutex<Option<StoredTokens>>,
}

impl TokenStore {
    pub fn new(path: PathBuf, client: reqwest::Client) -> Self {
        Self {
            path,
            client,
            state: Mutex::new(None),
        }
    }

    /// 期限が近ければ更新して保存し、有効な access_token を返す
    pub async fn access_token(&self) -> Result<String> {
        let mut pending = self.state.lock().await;
        let current = |pending: &Option<StoredTokens>| match pending {
            Some(t) => Ok(t.clone()),
            None => load(&self.path),
        };
        let tokens = current(&pending)?;
        if !needs_refresh(jwt_exp(&tokens.access_token), now_secs()) {
            return Ok(tokens.access_token);
        }
        let _file_lock = FileLock::acquire(&self.path).await?;
        // ロック待ちの間に他プロセスが更新済みのことがあるので、取得後に読み直して判定し直す
        let tokens = current(&pending)?;
        if !needs_refresh(jwt_exp(&tokens.access_token), now_secs()) {
            return Ok(tokens.access_token);
        }
        let fresh = refresh(&self.client, &tokens).await?;
        match save(&self.path, &fresh) {
            Ok(()) => *pending = None,
            Err(e) => {
                tracing::warn!(%e, "更新した Codex トークンを保存できませんでした。メモリ上で使い続けます");
                *pending = Some(fresh.clone());
            }
        }
        Ok(fresh.access_token)
    }
}
