use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

/// 保持している間だけ `noth run` の排他ロックが続く。flock なのでプロセスが落ちれば OS が解放する
#[derive(Debug)]
pub struct RunLock {
    _file: File,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LockState {
    Held { pid: Option<u32> },
    Free,
}

pub fn acquire(path: &Path) -> Result<RunLock> {
    // truncate すると起動中のプロセスが書いた pid を消してしまうので、ロックを取るまで中身に触れない
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("{} を開けません", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            let pid = read_pid(&mut file)
                .map(|p| format!("pid {p}"))
                .unwrap_or_else(|| "pid 不明".into());
            bail!(
                "noth は既に起動しています({pid}、{})。2重に起動すると同じ発言に2回返信してしまいます",
                path.display()
            );
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("{} をロックできません", path.display()));
        }
    }
    file.set_len(0)?;
    file.rewind()?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(RunLock { _file: file })
}

pub fn probe(path: &Path) -> io::Result<LockState> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LockState::Free),
        Err(e) => return Err(e),
    };
    match file.try_lock() {
        Ok(()) => Ok(LockState::Free),
        Err(std::fs::TryLockError::WouldBlock) => Ok(LockState::Held {
            pid: read_pid(&mut file),
        }),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

fn read_pid(file: &mut File) -> Option<u32> {
    let mut s = String::new();
    file.rewind().ok()?;
    file.read_to_string(&mut s).ok()?;
    s.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_while_first_is_held() {
        let dir = std::env::temp_dir().join(format!("noth-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("noth.lock");
        let _ = std::fs::remove_file(&path);
        assert_eq!(probe(&path).unwrap(), LockState::Free);

        let held = acquire(&path).unwrap();
        let err = acquire(&path).unwrap_err().to_string();
        assert!(err.contains(&std::process::id().to_string()), "{err}");
        assert_eq!(
            probe(&path).unwrap(),
            LockState::Held {
                pid: Some(std::process::id())
            }
        );

        drop(held);
        assert_eq!(probe(&path).unwrap(), LockState::Free);
        acquire(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
