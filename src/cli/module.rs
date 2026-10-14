use std::path::Path;
use std::process::ExitCode;

use anyhow::{Result, bail};
use noth_core::config::enabled_modules;
use noth_core::registry::{KNOWN_MODULES, ModuleKind, kind_of};

use super::status::notify_restart_needed;
use super::{edit, message, paths, service};

pub fn list(cfg: Option<&Path>) -> Result<ExitCode> {
    let reg = crate::registry();
    let enabled = paths(cfg).ok().and_then(|p| enabled_modules(&p).ok());
    if let Some(list) = &enabled
        && let Err(e) = reg.validate_enabled(list)
    {
        eprintln!(
            "警告: 有効な module の構成が不正です: {}",
            super::message(&e)
        );
    }
    println!("{} {} 有効", pad("名前", 20), pad("組み込み", 9));
    for (name, _) in KNOWN_MODULES {
        let built_in = reg.is_built_in(name);
        let enabled = match &enabled {
            Some(list) if list.iter().any(|m| m == name) => "はい",
            Some(_) => "いいえ",
            None => "-",
        };
        println!(
            "{} {} {}",
            pad(name, 20),
            pad(if built_in { "はい" } else { "いいえ" }, 9),
            enabled
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// 全角文字を2桁として数え、日本語の見出しでも列を揃える
fn pad(s: &str, width: usize) -> String {
    let w: usize = s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}

/// 無いと module が動かないセクション。tool-* と router-mention は既定値で動くので対象外
/// (tool-web-* は代わりに backend のセクションを要る)
fn required_section(name: &str) -> Option<String> {
    match kind_of(name)? {
        ModuleKind::Channel | ModuleKind::Provider => name
            .split_once('-')
            .map(|(kind, rest)| format!("{kind}.{rest}")),
        ModuleKind::Router if name == "router-systemone" => Some("router.systemone".into()),
        ModuleKind::Tool if name.starts_with("tool-web-") => Some("backend.tinyfish".into()),
        _ => None,
    }
}

pub async fn set_enabled(
    cfg: Option<&Path>,
    name: &str,
    enable: bool,
    restart: bool,
) -> Result<ExitCode> {
    if kind_of(name).is_none() {
        let known: Vec<&str> = KNOWN_MODULES.iter().map(|(n, _)| *n).collect();
        bail!(
            "未知の module です: `{name}`(指定できるのは {})",
            known.join(", ")
        );
    }
    let reg = crate::registry();
    // 無効化は組み込み外でも許す(組み込まずに残った設定を外せるように)
    if enable && !reg.is_built_in(name) {
        bail!(
            "module `{name}` はこのバイナリに組み込まれていません。`cargo build --release --features {name}` で組み込み直してください"
        );
    }
    let p = paths(cfg)?;
    let mut doc = edit::parse_doc(&p.config)?;
    let changed = if enable {
        edit::enable_module(&mut doc, name)?
    } else {
        edit::disable_module(&mut doc, name)?
    };
    if !changed {
        println!("{name} は既に{}です", if enable { "有効" } else { "無効" });
        return Ok(ExitCode::SUCCESS);
    }
    edit::write_atomic(&p.config, &doc.to_string(), None)?;
    println!(
        "{name} を{}にしました",
        if enable { "有効" } else { "無効" }
    );

    if let Err(e) = reg.validate_enabled(&edit::enabled_list(&doc)) {
        eprintln!("警告: 有効な module の構成が不正です: {}", message(&e));
    }
    if enable
        && let Some(path) = required_section(name)
        && noth_core::config::lookup(&doc.to_string().parse()?, &path).is_none()
    {
        eprintln!(
            "警告: config.toml に [{path}] がありません。必要な項目を `noth config set` で設定してください"
        );
    }

    if restart {
        service::restart_if_installed(cfg).await?;
    } else {
        notify_restart_needed(cfg).await;
    }
    Ok(ExitCode::SUCCESS)
}
