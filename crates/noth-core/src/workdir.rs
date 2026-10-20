//! run ごとの作業ディレクトリ(`$NOTH_HOME/work/<run_id>/`)の片付け。
//! 成果物の置き場所の workspace は対象外で、ここでは触らない
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub empty: usize,
    pub expired: usize,
}

/// 空のディレクトリを消す前の猶予。作った直後で shell がまだ書き込む前のものを、片付けが消してしまわないため
const EMPTY_GRACE: Duration = Duration::from_secs(10 * 60);

/// `next_run_id` が作る `<ミリ秒>-<連番>` の形だけを対象にする。ミリ秒は 13 桁以上で、
/// 人が置いた `1-1` のような名前や別用途のディレクトリを巻き込まない
fn is_run_id(name: &str) -> bool {
    let Some((ms, n)) = name.split_once('-') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    ms.len() >= 13 && digits(ms) && digits(n)
}

/// 空のときだけ消す。`remove_dir` は中身があれば失敗するので、判定と削除の間に Worker が書き込んでも巻き込まない
pub fn remove_if_empty(dir: &Path) -> bool {
    std::fs::remove_dir(dir).is_ok()
}

/// 中身の最終更新で最も新しいもの。シンボリックリンクはたどらず、リンク自身の時刻だけを見る
fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest = std::fs::symlink_metadata(dir).ok()?.modified().ok()?;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if let Ok(m) = meta.modified() {
                newest = newest.max(m);
            }
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Some(newest)
}

/// `root` の直下で、名前が run_id の形の実ディレクトリだけを見る。空なら消し(作って間もないものは除く)、
/// `retention` を過ぎていれば中身ごと消す。消したものは 1 件ずつログに出す。
/// `active` の run は、動いている最中なので触らない。`retention` が None なら期限切れの削除はしない
pub fn sweep(
    root: &Path,
    retention: Option<Duration>,
    active: &HashSet<String>,
    now: SystemTime,
) -> Swept {
    let mut swept = Swept::default();
    let Ok(entries) = std::fs::read_dir(root) else {
        return swept;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_run_id(name) || active.contains(name) {
            continue;
        }
        let path = entry.path();
        // リンク先を消さないよう、リンク自身ではなく実ディレクトリのときだけ扱う
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let age = newest_mtime(&path).and_then(|m| now.duration_since(m).ok());
        if age.is_some_and(|a| a > EMPTY_GRACE) && remove_if_empty(&path) {
            tracing::info!(run_id = name, path = %path.display(), "空の作業ディレクトリを消しました");
            swept.empty += 1;
            continue;
        }
        let Some(retention) = retention else { continue };
        if age.is_some_and(|a| a > retention) {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    tracing::info!(run_id = name, path = %path.display(), "期限を過ぎた作業ディレクトリを消しました");
                    swept.expired += 1;
                }
                Err(e) => {
                    tracing::warn!(run_id = name, path = %path.display(), error = %e, "期限を過ぎた作業ディレクトリを消せませんでした")
                }
            }
        }
    }
    swept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("noth-workdir-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);

    fn later(days: u64) -> SystemTime {
        SystemTime::now() + Duration::from_secs(days * 24 * 3600)
    }

    #[test]
    fn run_id_shape() {
        assert!(is_run_id("1792170173263-2"));
        for bad in [
            "workspace",
            "1792170173263-",
            "-1",
            "1792170173263-2-3",
            "abc-1",
            "",
            "1-1",
            "123456789012-1",
        ] {
            assert!(!is_run_id(bad), "{bad}");
        }
    }

    #[test]
    fn empty_dirs_are_removed_and_used_ones_stay_while_fresh() {
        let root = root("empty");
        std::fs::create_dir(root.join("1792170173263-1")).unwrap();
        std::fs::create_dir(root.join("1792170173263-2")).unwrap();
        std::fs::write(root.join("1792170173263-2/a.txt"), "x").unwrap();
        // 作って間もない空のディレクトリは、shell がこれから書くかもしれないので残す
        assert_eq!(
            sweep(&root, Some(WEEK), &HashSet::new(), SystemTime::now()),
            Swept::default()
        );
        let swept = sweep(&root, Some(WEEK), &HashSet::new(), later(1));
        assert_eq!(
            swept,
            Swept {
                empty: 1,
                expired: 0
            }
        );
        assert!(!root.join("1792170173263-1").exists());
        assert!(root.join("1792170173263-2/a.txt").exists());
    }

    #[test]
    fn old_dirs_are_removed_by_newest_file_not_by_the_directory() {
        let root = root("old");
        std::fs::create_dir_all(root.join("1792170173263-1/sub")).unwrap();
        std::fs::write(root.join("1792170173263-1/sub/a.txt"), "x").unwrap();
        assert_eq!(
            sweep(&root, Some(WEEK), &HashSet::new(), later(6)).expired,
            0
        );
        assert_eq!(
            sweep(&root, Some(WEEK), &HashSet::new(), later(8)).expired,
            1
        );
        assert!(!root.join("1792170173263-1").exists());
    }

    #[test]
    fn retention_none_keeps_old_dirs() {
        let root = root("none");
        std::fs::create_dir(root.join("1792170173263-1")).unwrap();
        std::fs::write(root.join("1792170173263-1/a.txt"), "x").unwrap();
        assert_eq!(
            sweep(&root, None, &HashSet::new(), later(365)),
            Swept::default()
        );
    }

    #[test]
    fn active_runs_and_foreign_names_are_untouched() {
        let root = root("active");
        for name in ["1792170173263-1", "workspace", "notes"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        std::fs::write(root.join("1792170173263-1/a.txt"), "x").unwrap();
        let active = HashSet::from(["1792170173263-1".to_string()]);
        let swept = sweep(&root, Some(WEEK), &active, later(30));
        assert_eq!(swept, Swept::default());
        assert!(root.join("1792170173263-1/a.txt").exists());
        assert!(root.join("workspace").exists() && root.join("notes").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let root = root("link");
        let outside = root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "x").unwrap();
        // run_id の形のリンク自体は対象外で、リンク先の中身も消えない
        std::os::unix::fs::symlink(&outside, root.join("1792170173263-1")).unwrap();
        // 中身にあるリンクは、たどらずリンクだけを消す
        std::fs::create_dir(root.join("1792170173263-2")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("1792170173263-2/link")).unwrap();
        let swept = sweep(&root, Some(WEEK), &HashSet::new(), later(30));
        assert_eq!(swept.expired, 1);
        assert!(root.join("1792170173263-1").exists());
        assert!(!root.join("1792170173263-2").exists());
        assert!(outside.join("keep.txt").exists());
    }
}
