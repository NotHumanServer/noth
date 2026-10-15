use anyhow::{Context, Result};
use toml_edit::{Array, DocumentMut, Item, Value};

pub fn enabled_list(doc: &DocumentMut) -> Vec<String> {
    doc.get("modules")
        .and_then(|m| m.get("enabled"))
        .and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn enabled_array(doc: &mut DocumentMut) -> Result<&mut Array> {
    doc.get_mut("modules")
        .and_then(|m| m.get_mut("enabled"))
        .and_then(Item::as_array_mut)
        .context("config.toml に [modules] enabled がありません")
}

/// `enabled` 配列の 1 要素。toml_edit は要素間の空白とコメントを前後の要素の decor に持つので、
/// 付け外しのたびに壊れないよう、いったん要素単位のコメントに分解して組み直す
struct ModuleEntry {
    /// 要素の直前にあるコメント行(`# ...`)。その要素に付いた注意書きとして一緒に動かす
    lead: Vec<String>,
    name: String,
    /// カンマの後ろの行末コメント。桁揃えの空白ごと保つ
    trail: Option<String>,
}

fn decor_text(s: Option<&toml_edit::RawString>) -> String {
    s.and_then(|r| r.as_str()).unwrap_or("").to_string()
}

fn comment_lines(text: &str) -> impl Iterator<Item = String> + '_ {
    text.lines()
        .map(str::trim)
        .filter(|l| l.starts_with('#'))
        .map(String::from)
}

/// 1 行に収まる配列(`["a", "b"]`)は None。コメントを持てないので toml_edit の整形に任せる
fn split_entries(arr: &Array) -> Option<(Vec<ModuleEntry>, Vec<String>)> {
    let multiline = arr
        .iter()
        .any(|v| decor_text(v.decor().prefix()).contains('\n'))
        || arr.trailing().as_str().is_some_and(|t| t.contains('\n'));
    if !multiline {
        return None;
    }
    let mut entries: Vec<ModuleEntry> = Vec::new();
    for v in arr.iter() {
        let prefix = decor_text(v.decor().prefix());
        // 先頭行は直前の要素の行末コメント、以降が自分の直前コメント
        let (first, rest) = match prefix.split_once('\n') {
            Some((f, r)) => (f.to_string(), r.to_string()),
            None => (prefix.clone(), String::new()),
        };
        match entries.last_mut() {
            Some(prev) if first.contains('#') => prev.trail = Some(first.trim_end().to_string()),
            None if first.contains('#') => {
                return None;
            }
            _ => {}
        }
        entries.push(ModuleEntry {
            lead: comment_lines(&rest).collect(),
            name: v.as_str()?.to_string(),
            trail: None,
        });
    }
    let tail = arr.trailing().as_str().unwrap_or("").to_string();
    let (first, rest) = tail.split_once('\n').unwrap_or((&tail, ""));
    if first.contains('#')
        && let Some(last) = entries.last_mut()
    {
        last.trail = Some(first.trim_end().to_string());
    }
    Some((entries, comment_lines(rest).collect()))
}

fn write_entries(arr: &mut Array, entries: &[ModuleEntry], dangling: &[String]) {
    let mut new = Array::new();
    let mut prev_trail = "";
    for e in entries {
        let mut v = Value::from(e.name.as_str());
        let leads: String = e.lead.iter().map(|l| format!("  {l}\n")).collect();
        v.decor_mut().set_prefix(format!("{prev_trail}\n{leads}  "));
        new.push_formatted(v);
        prev_trail = e.trail.as_deref().unwrap_or("");
    }
    let dangling: String = dangling.iter().map(|l| format!("  {l}\n")).collect();
    new.set_trailing(format!("{prev_trail}\n{dangling}"));
    new.set_trailing_comma(true);
    *new.decor_mut() = arr.decor().clone();
    *arr = new;
}

/// 追加したら true、既に有効なら false
pub fn enable_module(doc: &mut DocumentMut, name: &str) -> Result<bool> {
    if enabled_list(doc).iter().any(|m| m == name) {
        return Ok(false);
    }
    let arr = enabled_array(doc)?;
    match split_entries(arr) {
        Some((mut entries, dangling)) => {
            // 末尾の宙に浮いたコメントは、直前に外した要素の注意書きなので新しい末尾要素に戻す
            entries.push(ModuleEntry {
                lead: dangling,
                name: name.to_string(),
                trail: None,
            });
            write_entries(arr, &entries, &[]);
        }
        None => arr.push(name),
    }
    Ok(true)
}

/// 並びとコメントを保ったまま `old` を `new` に置き換える。`old` が無ければ false
pub fn replace_module(doc: &mut DocumentMut, old: &str, new: &str) -> Result<bool> {
    let arr = enabled_array(doc)?;
    let Some(idx) = arr.iter().position(|v| v.as_str() == Some(old)) else {
        return Ok(false);
    };
    arr.replace(idx, new);
    Ok(true)
}

/// 外したら true、元から無効なら false
pub fn disable_module(doc: &mut DocumentMut, name: &str) -> Result<bool> {
    let arr = enabled_array(doc)?;
    let Some(idx) = arr.iter().position(|v| v.as_str() == Some(name)) else {
        return Ok(false);
    };
    match split_entries(arr) {
        Some((mut entries, mut dangling)) => {
            let removed = entries.remove(idx);
            // 外した要素の直前コメント(注意書き)は捨てず、次の要素の前、無ければ末尾に残す。行末コメントは要素と一緒に消える
            match entries.get_mut(idx) {
                Some(next) => next.lead.splice(0..0, removed.lead).for_each(drop),
                None => dangling.splice(0..0, removed.lead).for_each(drop),
            }
            write_entries(arr, &entries, &dangling);
        }
        None => {
            arr.remove(idx);
            arr.fmt();
        }
    }
    Ok(true)
}
