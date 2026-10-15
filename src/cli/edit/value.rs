use anyhow::{Context, Result, bail};
use toml_edit::{Array, Value};

use super::env::is_env_key;

fn string_value(s: &str) -> Value {
    let mut v = Value::from(s);
    v.decor_mut().clear();
    v
}

/// 数値は 10 進のみ。`0x10` が 16 になる、日時や `inf` / `nan` になる、といった意図しない型を避ける
fn scalar_is_plain(v: &Value) -> bool {
    match v {
        Value::Boolean(_) | Value::String(_) => true,
        Value::Integer(_) => {
            let repr = v.to_string();
            let digits = repr.trim().trim_start_matches(['+', '-']);
            !(digits.starts_with("0x") || digits.starts_with("0o") || digits.starts_with("0b"))
        }
        Value::Float(f) => f.value().is_finite(),
        Value::Datetime(_) | Value::Array(_) | Value::InlineTable(_) => false,
    }
}

fn array_is_plain(a: &Array) -> bool {
    a.iter().all(|v| match v {
        Value::Array(inner) => array_is_plain(inner),
        other => scalar_is_plain(other),
    })
}

/// bool / 10 進の整数 / 有限の浮動小数 / 引用符付き文字列 / 配列だけを型付きで読み、残りは文字列にする。
/// `[` で始まる値は配列の意図なので、読めなければエラーにする
fn plain_value(raw: &str) -> Result<Value> {
    let raw = raw.trim();
    if raw.starts_with('[') {
        let mut v = raw
            .parse::<Value>()
            .map_err(|e| anyhow::anyhow!("配列として読めません: {e}"))?;
        match &v {
            Value::Array(a) if array_is_plain(a) => {}
            Value::Array(_) => bail!(
                "配列の要素に使えるのは bool・10 進の数値・文字列だけです(日時・16 進などは `\"…\"` で囲んでください)"
            ),
            _ => bail!("配列として読めません"),
        }
        v.decor_mut().clear();
        return Ok(v);
    }
    Ok(match raw.parse::<Value>() {
        Ok(mut v) if scalar_is_plain(&v) => {
            v.decor_mut().clear();
            v
        }
        _ => string_value(raw),
    })
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// Discord の ID は 18 桁前後で整数にすると桁や型を壊すので、`[..]` でも空白区切りでも文字列の配列にする
fn id_list(raw: &str) -> Result<Value> {
    let raw = raw.trim();
    let inner = match raw.strip_prefix('[') {
        Some(rest) => rest
            .strip_suffix(']')
            .context("配列として読めません(`]` がありません)")?,
        None => raw,
    };
    let mut arr = Array::new();
    for item in inner
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(unquote)
        .filter(|s| !s.is_empty())
    {
        arr.push(item);
    }
    Ok(Value::Array(arr))
}

/// `key` に応じた型の値にする。`force_string` ならそのまま文字列にする
pub fn typed_value(key: &str, raw: &str, force_string: bool) -> Result<Value> {
    if force_string {
        return Ok(string_value(raw));
    }
    let last = key.rsplit('.').next().unwrap_or(key);
    if last == "allowed_users" {
        return id_list(raw);
    }
    let value = plain_value(raw)?;
    if last == "guild_id" || last.ends_with("_id") {
        return Ok(if value.is_str() {
            value
        } else {
            string_value(unquote(raw))
        });
    }
    Ok(value)
}

/// `api_key` / `token` / `*_secret` などに、`${VAR}` ではなく値そのものを書こうとしているか
pub fn is_literal_secret(key: &str, value: &Value) -> bool {
    let last = key.rsplit('.').next().unwrap_or(key);
    let secret_like = matches!(last, "api_key" | "token" | "password")
        || last.ends_with("_secret")
        || last.ends_with("_token")
        || last.ends_with("_api_key");
    let Some(s) = value.as_str() else {
        return false;
    };
    let is_reference = s
        .strip_prefix("${")
        .and_then(|r| r.strip_suffix('}'))
        .is_some_and(is_env_key);
    secret_like && !s.is_empty() && !is_reference
}
