use serde_json::Value;

pub(crate) const MAX_LIMIT: usize = 100;

pub(crate) fn text_arg(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Discord の ID は文字列で渡すよう案内しているが、モデルが整数で渡しても受ける
pub(crate) fn id_arg(input: &Value, key: &str) -> Result<Option<u64>, String> {
    let bad = || format!("{key} は数字の ID で指定してください");
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => s
            .trim()
            .parse()
            .ok()
            .filter(|n| *n != 0)
            .map(Some)
            .ok_or_else(bad),
        Some(v) => v.as_u64().filter(|n| *n != 0).map(Some).ok_or_else(bad),
    }
}

pub(crate) fn require_id(input: &Value, key: &str) -> Result<u64, String> {
    id_arg(input, key)?.ok_or_else(|| format!("{key} は必須です"))
}

/// 範囲外や数値でない指定はエラーにせず丸める。Discord の上限は 1 ページ 100 件
pub(crate) fn clamp_limit(input: &Value, default: usize) -> usize {
    let n = input
        .get("limit")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.trim().parse().ok()));
    match n {
        Some(n) => (n.max(1) as usize).min(MAX_LIMIT),
        None => default,
    }
}
