use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use noth_core::config::enabled_modules;
use noth_core::registry::KNOWN_MODULES;

use super::paths;

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
