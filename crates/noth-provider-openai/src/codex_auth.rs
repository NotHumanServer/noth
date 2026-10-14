//! ChatGPT サブスクリプションで Codex バックエンドを叩くための OAuth(device code)とトークンの保管・更新
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use noth_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

const ISSUER: &str = "https://auth.openai.com";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const REFRESH_SKEW_SECS: u64 = 120;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MIN_POLL_INTERVAL_SECS: u64 = 3;
const RELOGIN_HINT: &str = "`noth auth codex` を実行して再ログインしてください";

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

/// 外部ライブラリなしで JWT の payload を読むための base64url デコーダ。不正な文字があれば None
pub fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&base64url_decode(payload)?).ok()
}

pub fn jwt_exp(token: &str) -> Option<u64> {
    let exp = jwt_claims(token)?.get("exp")?.clone();
    // 発行元によっては 1700000000.0 のような浮動小数で入っている
    exp.as_u64()
        .or_else(|| exp.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
}

pub fn account_id(token: &str) -> Option<String> {
    jwt_claims(token)?[AUTH_CLAIM]["chatgpt_account_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// OpenAI は第三者のハーネスに自己申告を求めている(hermes-agent の agent/codex_headers.py
/// `codex_cloudflare_headers` の注記)。偽装せず Noth として名乗る。
/// 組織の data residency が強制されたワークスペースは residency ヘッダが無いと 401 になる
pub fn request_headers(access_token: &str) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("originator", "noth".to_string()),
        ("User-Agent", format!("Noth/{}", env!("CARGO_PKG_VERSION"))),
    ];
    if let Some(id) = account_id(access_token) {
        headers.push(("ChatGPT-Account-ID", id));
    }
    let claims = jwt_claims(access_token).unwrap_or_default();
    let auth = &claims[AUTH_CLAIM];
    let residency = ["chatgpt_data_residency", "chatgpt_compute_residency"]
        .iter()
        .find_map(|k| auth[*k].as_str().map(str::trim).filter(|s| !s.is_empty()));
    if let Some(r) = residency {
        headers.push(("x-openai-internal-codex-residency", r.to_string()));
    }
    headers
}

/// exp が読めないトークンは不透明とみなし、毎回更新しに行かない
pub fn needs_refresh(exp: Option<u64>, now: u64) -> bool {
    exp.is_some_and(|exp| exp <= now + REFRESH_SKEW_SECS)
}

pub fn mask_account_id(id: &str) -> String {
    let head: String = id.chars().take(4).collect();
    format!("{head}****")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// UTC の RFC 3339。日時クレートを足さないため、日数から暦日への変換を自前で持つ
fn rfc3339_utc(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
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

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// reqwest の `form` feature を足さずに済ませる
fn form_body(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

async fn post_form(
    client: &reqwest::Client,
    pairs: &[(&str, &str)],
) -> std::result::Result<(u16, Value), Error> {
    let resp = client
        .post(TOKEN_URL)
        // ファイルロックの奪取条件(LOCK_STALE)の前提になる上限
        .timeout(Duration::from_secs(30))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body(pairs))
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or_default()))
}

fn tokens_from_response(body: &Value, fallback_refresh: &str) -> Result<StoredTokens> {
    let access = body["access_token"].as_str().filter(|s| !s.is_empty());
    let Some(access) = access else {
        return Err(Error::Other(
            "Codex のトークン応答に access_token がありません".into(),
        ));
    };
    // refresh_token は使い切りでローテーションされることがあるため、返ってきたら必ず差し替える
    let refresh = body["refresh_token"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(fallback_refresh);
    // 空のまま保存すると以降の更新がすべて失敗するので、取得できた時点で弾く
    if refresh.trim().is_empty() {
        return Err(Error::Other(
            "Codex のトークン応答に refresh_token がありません".into(),
        ));
    }
    Ok(StoredTokens {
        access_token: access.to_string(),
        refresh_token: refresh.trim().to_string(),
        last_refresh: rfc3339_utc(now_secs()),
    })
}

async fn refresh(client: &reqwest::Client, current: &StoredTokens) -> Result<StoredTokens> {
    let (status, body) = post_form(
        client,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &current.refresh_token),
            ("client_id", CLIENT_ID),
        ],
    )
    .await?;
    match status {
        200 => tokens_from_response(&body, &current.refresh_token),
        // 429 と 5xx は refresh_token がまだ有効なので、再ログインを促さない
        429 | 500..=599 => Err(Error::Api {
            status,
            body: "Codex のトークン更新が一時的に受け付けられませんでした。時間をおいて再試行してください".into(),
        }),
        _ => Err(Error::Other(format!(
            "Codex のトークン更新が拒否されました(HTTP {status})。{RELOGIN_HINT}"
        ))),
    }
}

/// 別プロセス(常駐サービスと `noth doctor` など)が同時に更新して、使い切りの refresh_token を
/// 二重に使わないための排他。排他作成したファイルをロックとみなす
struct FileLock {
    path: PathBuf,
}

/// 更新リクエストのタイムアウト(`post_form`)より長くし、生きているプロセスのロックは奪わない
const LOCK_STALE: Duration = Duration::from_secs(90);
const LOCK_WAIT: Duration = Duration::from_secs(60);

impl FileLock {
    async fn acquire(target: &Path) -> Result<Self> {
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
    state: Mutex<Option<StoredTokens>>,
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

fn login_err(msg: impl std::fmt::Display) -> Error {
    Error::Other(format!("Codex のログインに失敗しました: {msg}"))
}

async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: &Value,
) -> std::result::Result<(u16, Value), Error> {
    let resp = client
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or_default()))
}

/// device code でログインし、トークンを `store_path` に保存する
pub async fn login(store_path: &Path, print: impl Fn(&str)) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(login_err)?;
    let (status, device) = post_json(
        &client,
        &format!("{ISSUER}/api/accounts/deviceauth/usercode"),
        &json!({"client_id": CLIENT_ID}),
    )
    .await?;
    if status != 200 {
        return Err(login_err(format!(
            "デバイスコードの発行が HTTP {status} でした"
        )));
    }
    let (Some(user_code), Some(device_auth_id)) = (
        device["user_code"].as_str().filter(|s| !s.is_empty()),
        device["device_auth_id"].as_str().filter(|s| !s.is_empty()),
    ) else {
        return Err(login_err("デバイスコードの応答が不完全です"));
    };
    // interval は文字列で返ることがある
    let interval = device["interval"]
        .as_u64()
        .or_else(|| device["interval"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(5)
        .max(MIN_POLL_INTERVAL_SECS);

    print(&format!(
        "1. ブラウザで {ISSUER}/codex/device を開く\n2. コード {user_code} を入力する\nサインインを待っています(最大 15 分)..."
    ));

    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    let code_resp = loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(login_err("15 分以内にサインインが完了しませんでした"));
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let polled = post_json(
            &client,
            &format!("{ISSUER}/api/accounts/deviceauth/token"),
            &json!({"device_auth_id": device_auth_id, "user_code": user_code}),
        )
        .await;
        match polled {
            Ok((200, body)) => break body,
            // 403 / 404 はまだサインインが終わっていないだけ
            Ok((403 | 404, _)) => {}
            Ok((status, _)) => {
                return Err(login_err(format!("ポーリングが HTTP {status} でした")));
            }
            // 一時的な通信断で最初からやり直させない。期限まで続ける
            Err(e) => tracing::warn!(%e, "Codex ログインのポーリングに失敗しました"),
        }
    };

    let (Some(code), Some(verifier)) = (
        code_resp["authorization_code"]
            .as_str()
            .filter(|s| !s.is_empty()),
        code_resp["code_verifier"]
            .as_str()
            .filter(|s| !s.is_empty()),
    ) else {
        return Err(login_err(
            "認可コードまたは code_verifier が応答に含まれていません",
        ));
    };
    let redirect_uri = format!("{ISSUER}/deviceauth/callback");
    let (status, body) = post_form(
        &client,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ],
    )
    .await?;
    if status != 200 {
        return Err(login_err(format!("トークン交換が HTTP {status} でした")));
    }
    let tokens = tokens_from_response(&body, "")?;
    save(store_path, &tokens)?;
    print(&format!("保存しました: {}", store_path.display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64url_encode(data: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |a, (i, b)| a | u32::from(*b) << (16 - 8 * i));
            for i in 0..=chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        out
    }

    pub fn jwt(claims: &Value) -> String {
        format!(
            "{}.{}.sig",
            b64url_encode(b"{\"alg\":\"none\"}"),
            b64url_encode(claims.to_string().as_bytes())
        )
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "noth-codex-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn base64url_roundtrip_and_padding() {
        assert_eq!(base64url_decode("aGVsbG8").unwrap(), b"hello");
        assert_eq!(base64url_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64url_decode("_-8").unwrap(), vec![0xff, 0xef]);
        assert!(base64url_decode("a b").is_none());
        let data: Vec<u8> = (0..=255).collect();
        assert_eq!(base64url_decode(&b64url_encode(&data)).unwrap(), data);
    }

    #[test]
    fn claims_and_headers() {
        let token = jwt(&json!({
            "exp": 2000000000u64,
            AUTH_CLAIM: {"chatgpt_account_id": "acct_12345", "chatgpt_data_residency": " us "},
        }));
        assert_eq!(jwt_exp(&token), Some(2000000000));
        assert_eq!(account_id(&token).as_deref(), Some("acct_12345"));
        let h = request_headers(&token);
        let get = |k: &str| h.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("ChatGPT-Account-ID"), Some("acct_12345"));
        assert_eq!(get("x-openai-internal-codex-residency"), Some("us"));
        assert_eq!(get("originator"), Some("noth"));
        assert!(get("User-Agent").unwrap().starts_with("Noth/"));
    }

    #[test]
    fn malformed_token_drops_account_headers() {
        let h = request_headers("not-a-jwt");
        assert!(
            h.iter()
                .all(|(n, _)| *n == "originator" || *n == "User-Agent")
        );
        assert_eq!(jwt_exp("a.b.c"), None);
    }

    #[test]
    fn refresh_decision() {
        assert!(needs_refresh(Some(1000), 1000));
        assert!(needs_refresh(Some(1119), 1000));
        assert!(!needs_refresh(Some(1121), 1000));
        assert!(!needs_refresh(None, 1000));
    }

    #[test]
    fn masking_and_time_format() {
        assert_eq!(mask_account_id("acct_12345"), "acct****");
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn form_encoding() {
        assert_eq!(
            form_body(&[("a", "x y&z"), ("redirect", "https://h/p")]),
            "a=x%20y%26z&redirect=https%3A%2F%2Fh%2Fp"
        );
    }

    #[test]
    fn tokens_keep_old_refresh_unless_rotated() {
        let t = tokens_from_response(&json!({"access_token": "a"}), "old").unwrap();
        assert_eq!(t.refresh_token, "old");
        let t = tokens_from_response(&json!({"access_token": "a", "refresh_token": "new"}), "old")
            .unwrap();
        assert_eq!(t.refresh_token, "new");
        assert!(tokens_from_response(&json!({}), "old").is_err());
        // ログイン直後は引き継ぐ refresh_token が無いので、応答に無ければエラー
        assert!(tokens_from_response(&json!({"access_token": "a"}), "").is_err());
        assert!(
            tokens_from_response(&json!({"access_token": "a", "refresh_token": " "}), "").is_err()
        );
    }

    #[test]
    fn store_is_atomic_and_private() {
        let dir = temp_dir("store");
        let path = dir.join("data").join("codex-auth.json");
        let t = StoredTokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            last_refresh: "2026-01-01T00:00:00Z".into(),
        };
        save(&path, &t).unwrap();
        let t2 = StoredTokens {
            access_token: "a2".into(),
            ..t.clone()
        };
        save(&path, &t2).unwrap();
        assert_eq!(load(&path).unwrap(), t2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(leftovers.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_store_mentions_login_command() {
        let err = load(Path::new("/nonexistent/noth/codex-auth.json")).unwrap_err();
        assert!(err.to_string().contains("noth auth codex"), "{err}");
    }

    #[tokio::test]
    async fn file_lock_is_exclusive_and_released() {
        let dir = temp_dir("lock");
        let target = dir.join("codex-auth.json");
        let lock = FileLock::acquire(&target).await.unwrap();
        let lock_path = dir.join("codex-auth.json.lock");
        assert!(lock_path.exists());
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
                .is_err()
        );
        drop(lock);
        assert!(!lock_path.exists());
        drop(FileLock::acquire(&target).await.unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn pending_tokens_survive_save_failure() {
        // 保存先を親がファイルのパスにして save を失敗させ、メモリ上のトークンが優先されることを確かめる
        let dir = temp_dir("pending");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let path = blocker.join("codex-auth.json");
        let fresh_token = jwt(&json!({"exp": now_secs() + 3600}));
        let fresh = StoredTokens {
            access_token: fresh_token.clone(),
            refresh_token: "new".into(),
            last_refresh: String::new(),
        };
        assert!(save(&path, &fresh).is_err());
        let store = TokenStore::new(path, reqwest::Client::new());
        *store.state.lock().await = Some(fresh);
        assert_eq!(store.access_token().await.unwrap(), fresh_token);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn float_exp_is_accepted() {
        assert_eq!(
            jwt_exp(&jwt(&json!({"exp": 1700000000.0}))),
            Some(1700000000)
        );
        assert_eq!(jwt_exp(&jwt(&json!({"exp": 1700000000}))), Some(1700000000));
        assert_eq!(jwt_exp(&jwt(&json!({"exp": -1.5}))), None);
    }

    #[test]
    fn debug_masks_tokens() {
        let t = StoredTokens {
            access_token: "secret-a".into(),
            refresh_token: "secret-r".into(),
            last_refresh: "2026-01-01T00:00:00Z".into(),
        };
        let dbg = format!("{t:?}");
        assert!(!dbg.contains("secret"), "{dbg}");
        assert!(dbg.contains("2026-01-01"));
    }

    #[cfg(unix)]
    #[test]
    fn store_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("dirmode");
        let path = dir.join("data").join("codex-auth.json");
        let t = StoredTokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            last_refresh: String::new(),
        };
        save(&path, &t).unwrap();
        let mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
