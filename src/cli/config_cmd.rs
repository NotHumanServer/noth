use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;

use noth_core::config::{Config, lookup, masked_raw_table};

use super::{ConfigCmd, load, message, paths, unimplemented_exit};

pub fn run(cfg: Option<&Path>, cmd: ConfigCmd) -> Result<ExitCode> {
    match cmd {
        ConfigCmd::Show => {
            let p = paths(cfg)?;
            match Config::load(p.clone()) {
                Ok(c) => print!("{}", c.to_masked_string()),
                Err(e) => {
                    eprintln!("警告: 変数が解決できないため、${{…}} を含む値は伏せて表示します");
                    eprintln!("{}", message(&e));
                    let masked = masked_raw_table(&p).map_err(|e| anyhow::anyhow!(message(&e)))?;
                    print!("{}", toml::to_string_pretty(&masked)?);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        ConfigCmd::Path => {
            let p = paths(cfg)?;
            println!("設定:  {}", p.config.display());
            println!(".env:  {}", p.env.display());
            Ok(ExitCode::SUCCESS)
        }
        ConfigCmd::Check => {
            let result = load(cfg).and_then(|c| {
                crate::registry()
                    .build(&c)
                    .map(drop)
                    .map_err(|e| anyhow::anyhow!(message(&e)))
            });
            match result {
                Ok(()) => {
                    println!("問題ありません");
                    Ok(ExitCode::SUCCESS)
                }
                Err(e) => {
                    eprintln!("{e:#}");
                    Ok(ExitCode::from(1))
                }
            }
        }
        ConfigCmd::Get { key, json, reveal } => {
            let p = paths(cfg)?;
            let value = if reveal {
                let config = Config::load(p).map_err(|e| anyhow::anyhow!(message(&e)))?;
                config.get(&key).cloned()
            } else {
                lookup(
                    &masked_raw_table(&p).map_err(|e| anyhow::anyhow!(message(&e)))?,
                    &key,
                )
                .cloned()
            };
            let Some(value) = value else {
                eprintln!("キーが見つかりません: {key}");
                return Ok(ExitCode::from(1));
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else if let Some(s) = value.as_str() {
                println!("{s}");
            } else {
                println!("{value}");
            }
            Ok(ExitCode::SUCCESS)
        }
        ConfigCmd::Set { .. } | ConfigCmd::Unset { .. } => Ok(unimplemented_exit()),
    }
}
