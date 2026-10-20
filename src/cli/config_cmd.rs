use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;

use noth_core::config::{Config, Paths, lookup, masked_raw_table};

use super::edit;
use super::status::notify_restart_needed;
use super::{ConfigCmd, load, message, paths};

pub async fn run(cfg: Option<&Path>, cmd: ConfigCmd) -> Result<ExitCode> {
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
        ConfigCmd::Set { key, value, string } => {
            let p = paths(cfg)?;
            if edit::is_env_key(&key) {
                let text = std::fs::read_to_string(&p.env).unwrap_or_default();
                edit::write_atomic(&p.env, &edit::env_set(&text, &key, &value)?, Some(0o600))?;
                println!("{} に {key} を設定しました", p.env.display());
            } else {
                let mut doc = edit::parse_doc(&p.config)?;
                let before = doc.to_string();
                let typed = edit::typed_value(&key, &value, string)?;
                if edit::is_literal_secret(&key, &typed) {
                    eprintln!(
                        "警告: {key} に値そのものを書きました。シークレットは .env に置き、`${{VAR}}` で参照してください(例: `noth config set MY_API_KEY <値>` のあと `noth config set {key} '${{MY_API_KEY}}'`)"
                    );
                }
                edit::set_key(&mut doc, &key, typed)?;
                write_config_checked(&p, &before, &doc.to_string())?;
                println!("{} の {key} を設定しました", p.config.display());
                if key == "worker.timeout_secs"
                    && let Some(msg) = super::service::timeout_mismatch_notice(cfg)
                {
                    println!("注意: {msg}");
                }
            }
            notify_restart_needed(cfg).await;
            Ok(ExitCode::SUCCESS)
        }
        ConfigCmd::Unset { key } => {
            let p = paths(cfg)?;
            let removed = if edit::is_env_key(&key) {
                let text = std::fs::read_to_string(&p.env).unwrap_or_default();
                let (out, found) = edit::env_unset(&text, &key);
                if found {
                    edit::write_atomic(&p.env, &out, Some(0o600))?;
                }
                found
            } else {
                let mut doc = edit::parse_doc(&p.config)?;
                let before = doc.to_string();
                let found = edit::unset_key(&mut doc, &key)?;
                if found {
                    write_config_checked(&p, &before, &doc.to_string())?;
                }
                found
            };
            if !removed {
                eprintln!("キーが見つかりません: {key}");
                return Ok(ExitCode::from(1));
            }
            println!("{key} を削除しました");
            notify_restart_needed(cfg).await;
            Ok(ExitCode::SUCCESS)
        }
    }
}

const UNRESOLVED_MARK: &str = "未定義または空";

/// `config check` と同じ検証(ネットワークなし)で見つかった問題の行。
/// `${VAR}` が未解決なだけの行は `soft` に分ける(.env は後から埋められるため)
#[derive(Default)]
struct Problems {
    hard: Vec<String>,
    soft: Vec<String>,
}

fn problems(p: &Paths, text: &str) -> Problems {
    let mut out = Problems::default();
    // Config::load はファイルからしか読めないので、同じディレクトリに検証用の一時ファイルを置く
    let probe = p.home.join(".noth-config-check.toml");
    if let Err(e) = edit::write_atomic(&probe, text, Some(0o600)) {
        out.hard.push(format!("{e:#}"));
        return out;
    }
    let load = |env: &Path| {
        Config::load(Paths {
            home: p.home.clone(),
            config: probe.clone(),
            env: env.to_path_buf(),
        })
    };
    let mut loaded = load(&p.env);
    if let Err(e) = &loaded {
        let msg = message(e);
        let unresolved: Vec<&str> = msg
            .lines()
            .filter(|l| l.contains(UNRESOLVED_MARK))
            .collect();
        if !unresolved.is_empty() {
            out.soft.extend(unresolved.iter().map(|l| l.to_string()));
            // 未解決の変数があると型の検査まで進まないので、仮の値で埋めて再検証する
            let mut env = std::fs::read_to_string(&p.env).unwrap_or_default();
            for var in unresolved.iter().flat_map(|l| referenced_vars(l)) {
                if let Ok(next) = edit::env_set(&env, var, "x") {
                    env = next;
                }
            }
            let probe_env = p.home.join(".noth-config-check.env");
            if edit::write_atomic(&probe_env, &env, Some(0o600)).is_ok() {
                loaded = load(&probe_env);
                let _ = std::fs::remove_file(&probe_env);
            }
        }
    }
    let _ = std::fs::remove_file(&probe);
    match loaded {
        Ok(c) => {
            if let Err(e) = crate::registry().build(&c) {
                out.hard.extend(message(&e).lines().map(String::from));
            }
        }
        Err(e) => {
            out.hard.extend(
                message(&e)
                    .lines()
                    .filter(|l| !l.contains(UNRESOLVED_MARK))
                    .map(String::from),
            );
        }
    }
    out
}

/// `... が参照する ${VAR} が .env で未定義または空` から VAR を取り出す
fn referenced_vars(line: &str) -> impl Iterator<Item = &str> {
    line.split("${")
        .skip(1)
        .filter_map(|rest| rest.split_once('}').map(|(name, _)| name))
}

/// 編集後の config.toml が新たに不正になるなら書かない。変更前から出ていた問題は止めない
/// (壊れた設定を `config set` で直せなくなるため)
fn write_config_checked(p: &Paths, before: &str, after: &str) -> Result<()> {
    let old = problems(p, before);
    let new = problems(p, after);
    let introduced: Vec<&String> = new.hard.iter().filter(|l| !old.hard.contains(l)).collect();
    if !introduced.is_empty() {
        let lines: Vec<&str> = introduced.iter().map(|s| s.as_str()).collect();
        anyhow::bail!(
            "この変更で設定が不正になるため書き込みませんでした:\n{}",
            lines.join("\n")
        );
    }
    edit::write_atomic(&p.config, after, None)?;
    for line in new.soft.iter().filter(|l| !old.soft.contains(l)) {
        eprintln!("警告: {line}");
    }
    Ok(())
}
