//! 出力の大きさを tool 側で収める。worker の文字数切り詰めは JSON の途中で切れてしまうため、項目単位で落とす
use serde_json::{Value, json};

/// 1 発言の本文の上限
pub(crate) const CONTENT_MAX_CHARS: usize = 600;
/// worker の `tool_output_max_chars` の既定(20000)に余裕を持たせた、項目の合計上限
pub(crate) const TOTAL_MAX_CHARS: usize = 12_000;

/// 上限を超える本文を切り、`…` を付ける。切ったかどうかも返す
pub(crate) fn cap_content(content: &str, max: usize) -> (String, bool) {
    match content.char_indices().nth(max) {
        Some((i, _)) => (format!("{}…", &content[..i]), true),
        None => (content.to_string(), false),
    }
}

fn size(v: &Value) -> usize {
    v.to_string().chars().count() + 1
}

/// 先頭から収まる分だけ残す(新しい順の並び向け)。落とした件数を返す
pub(crate) fn keep_front(items: Vec<Value>) -> (Vec<Value>, usize) {
    let mut used = 0;
    let total = items.len();
    let kept: Vec<Value> = items
        .into_iter()
        .take_while(|v| {
            used += size(v);
            used <= TOTAL_MAX_CHARS
        })
        .collect();
    let omitted = total - kept.len();
    (kept, omitted)
}

/// 末尾から収まる分だけ残す(古い順の並び向け)。落とした件数を返す
pub(crate) fn keep_back(mut items: Vec<Value>) -> (Vec<Value>, usize) {
    items.reverse();
    let (mut kept, omitted) = keep_front(items);
    kept.reverse();
    (kept, omitted)
}

pub(crate) fn listing(key: &str, items: Vec<Value>, omitted: usize) -> Value {
    let mut out = json!({"count": items.len(), key: items});
    if omitted > 0 {
        out["omitted"] = json!(omitted);
    }
    out
}

/// カテゴリ順に、収まる分のチャンネルだけを残す。(グループ, 残した件数, 落とした件数)
pub(crate) fn fit_groups(groups: Vec<(Value, Vec<Value>)>) -> (Vec<Value>, usize, usize) {
    let mut used = 0;
    let (mut kept, mut omitted) = (0, 0);
    let mut out = Vec::new();
    for (category, channels) in groups {
        let mut fitted = Vec::new();
        for ch in channels {
            used += size(&ch);
            if used <= TOTAL_MAX_CHARS {
                fitted.push(ch);
            } else {
                omitted += 1;
            }
        }
        kept += fitted.len();
        out.push(json!({"category": category, "channels": fitted}));
    }
    (out, kept, omitted)
}
