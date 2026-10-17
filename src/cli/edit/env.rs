use anyhow::{Result, bail};

/// 大文字スネークケースのキーは `.env` に書く(cli.md §2 `noth config`)
pub fn is_env_key(key: &str) -> bool {
    key.starts_with(|c: char| c.is_ascii_uppercase())
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn env_line_key(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line);
    line.split_once('=').map(|(k, _)| k.trim())
}

/// core の `.env` パーサはエスケープを解釈しないので、囲む引用符で空白や `#` を守る。
/// `"` と `'` の両方を含む値はどちらでも囲めず、書けない
pub fn env_value(value: &str) -> Result<String> {
    if value.contains('\n') || value.contains('\r') {
        bail!("改行を含む値は .env に書けません");
    }
    if !value
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '#' | '"' | '\''))
    {
        return Ok(value.to_string());
    }
    if !value.contains('"') {
        Ok(format!("\"{value}\""))
    } else if !value.contains('\'') {
        Ok(format!("'{value}'"))
    } else {
        bail!("`\"` と `'` の両方を含む値は .env に書けません")
    }
}

pub fn env_set(text: &str, key: &str, value: &str) -> Result<String> {
    let new_line = format!("{key}={}", env_value(value)?);
    let mut out = String::new();
    let mut replaced = false;
    for line in text.split_inclusive('\n') {
        if env_line_key(line) == Some(key) {
            if !replaced {
                out.push_str(&new_line);
                out.push('\n');
                replaced = true;
            }
        } else {
            out.push_str(line);
        }
    }
    if !replaced {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&new_line);
        out.push('\n');
    }
    Ok(out)
}

/// 消したら true、元から無ければ false
pub fn env_unset(text: &str, key: &str) -> (String, bool) {
    let mut found = false;
    let out = text
        .split_inclusive('\n')
        .filter(|line| {
            let hit = env_line_key(line) == Some(key);
            found |= hit;
            !hit
        })
        .collect();
    (out, found)
}
