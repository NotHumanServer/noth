use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;

use super::paths;

pub async fn run(cfg: Option<&Path>, follow: bool, lines: usize) -> Result<ExitCode> {
    let dir = paths(cfg)?.logs_dir();
    let Some(mut path) = latest_log(&dir) else {
        eprintln!("ログがありません: {}", dir.display());
        return Ok(ExitCode::from(1));
    };

    let text = String::from_utf8_lossy(&fs::read(&path)?).into_owned();
    let all: Vec<&str> = text.lines().collect();
    for line in &all[all.len().saturating_sub(lines)..] {
        println!("{line}");
    }
    if !follow {
        return Ok(ExitCode::SUCCESS);
    }

    let mut offset = text.len() as u64;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        // 日付が変わると新しいファイルに書かれる
        if let Some(latest) = latest_log(&dir)
            && latest != path
        {
            path = latest;
            offset = 0;
        }
        let Ok(mut f) = fs::File::open(&path) else {
            continue;
        };
        let len = f.metadata()?.len();
        if len < offset {
            offset = 0;
        }
        if len == offset {
            continue;
        }
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        offset += buf.len() as u64;
        let mut out = std::io::stdout();
        out.write_all(&buf)?;
        out.flush()?;
    }
}

fn latest_log(dir: &Path) -> Option<PathBuf> {
    // 日次ローテーションのファイル名は日付サフィックスなので辞書順が時系列順になる
    fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("noth.log"))
        .map(|e| e.path())
        .max()
}
