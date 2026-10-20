use std::path::Path;
use std::process::ExitCode;

use noth_core::config::Config;
use noth_core::registry::{ModuleKind, kind_of};
use noth_core::store::Store;

use super::{message, paths};

enum Mark {
    Ok,
    Warn,
    Fail,
    Skip,
}

fn report(mark: Mark, msg: impl std::fmt::Display) -> bool {
    let sym = match mark {
        Mark::Ok => "✓",
        Mark::Warn => "⚠",
        Mark::Fail => "✗",
        Mark::Skip => "-",
    };
    println!("{sym} {msg}");
    matches!(mark, Mark::Fail)
}

const SKIPPED: &str = "前段が失敗したためスキップ";
const MAX_ERROR_CHARS: usize = 200;

pub async fn run(cfg: Option<&Path>) -> anyhow::Result<ExitCode> {
    let mut failed = false;
    let p = paths(cfg)?;

    let config = match Config::load(p) {
        Ok(c) => {
            report(Mark::Ok, "config.toml の構文と ${VAR} の解決");
            Some(c)
        }
        Err(e) => {
            // 未解決の変数は1行ずつ複数返るので、行ごとに出す
            for line in message(&e).lines() {
                failed |= report(Mark::Fail, oneline(line));
            }
            None
        }
    };

    let reg = crate::registry();
    let valid = match &config {
        Some(c) => match reg.validate(c) {
            Ok(()) => {
                report(
                    Mark::Ok,
                    "module 構成(channel / router / provider が1つずつ)",
                );
                true
            }
            Err(e) => {
                failed |= report(
                    Mark::Fail,
                    format!("module 構成: {}", oneline(&message(&e))),
                );
                false
            }
        },
        None => {
            report(Mark::Skip, format!("module 構成({SKIPPED})"));
            false
        }
    };

    if let Some(c) = &config {
        warnings(c);
        match Store::open(&c.paths.database()).await {
            Ok(_) => {
                report(Mark::Ok, "SQLite の読み書き");
            }
            Err(e) => failed |= report(Mark::Fail, format!("SQLite: {}", oneline(&message(&e)))),
        }
    } else {
        report(Mark::Skip, format!("SQLite の読み書き({SKIPPED})"));
    }

    let built = match config.as_ref().filter(|_| valid).map(|c| reg.build(c)) {
        None => {
            report(Mark::Skip, format!("module の初期化({SKIPPED})"));
            None
        }
        Some(Err(e)) => {
            failed |= report(
                Mark::Fail,
                format!("module の初期化: {}", oneline(&message(&e))),
            );
            None
        }
        Some(Ok(m)) => {
            report(Mark::Ok, "module の初期化");
            Some(m)
        }
    };
    match built {
        Some(m) => {
            failed |= check("channel", m.channel.health_check().await);
            failed |= check("router", m.router.health_check().await);
            failed |= check("provider", m.provider.health_check().await);
            for (name, tool) in &m.tools {
                failed |= check(name, tool.health_check().await);
            }
        }
        None => {
            report(Mark::Skip, format!("module の疎通確認({SKIPPED})"));
        }
    }

    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn check(label: &str, result: noth_core::Result<String>) -> bool {
    match result {
        Ok(msg) => report(Mark::Ok, format!("{label}: {msg}")),
        Err(e) => report(Mark::Fail, format!("{label}: {}", oneline(&message(&e)))),
    }
}

/// API の生のエラー本文は長く改行も含むので、1行に潰して切り詰める
fn oneline(msg: &str) -> String {
    let flat = msg.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > MAX_ERROR_CHARS {
        flat.chars().take(MAX_ERROR_CHARS).collect::<String>() + "…"
    } else {
        flat
    }
}

fn warnings(c: &Config) {
    let users_empty = match c.get("channel.discord.allowed_users") {
        Some(v) => v.as_array().is_none_or(|a| a.is_empty()),
        None => true,
    };
    if c.is_enabled("channel-discord") && users_empty {
        report(Mark::Warn, "channel.discord.allowed_users が空です");
    }
    if !c.is_enabled("worker")
        && c.modules
            .enabled
            .iter()
            .any(|m| kind_of(m) == Some(ModuleKind::Tool))
    {
        report(Mark::Warn, "worker が無効なため tool-* は使われません");
    }
    let profile = c.paths.profile(&c.profile.name);
    if !profile.exists() {
        report(
            Mark::Warn,
            format!("profile ファイルがありません: {}", profile.display()),
        );
    }
}
