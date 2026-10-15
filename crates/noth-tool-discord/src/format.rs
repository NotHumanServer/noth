//! API が返した発言を、Noth の秘匿方針(security.md §2)に沿って Worker に見せる形にする
use std::collections::{HashMap, HashSet};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::{Value, json};

use crate::budget::cap_content;

pub(crate) const MASKED_AUTHOR: &str = "<user>";

pub(crate) struct Viewer<'a> {
    pub(crate) self_id: u64,
    pub(crate) self_name: &'a str,
    pub(crate) allowed: &'a HashSet<u64>,
    pub(crate) tz: &'a TimeZone,
}

pub(crate) fn id_of(v: &Value) -> Option<u64> {
    v.as_str()?.parse().ok()
}

pub(crate) fn user_name(user: &Value) -> String {
    let pick = |k: &str| {
        user.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    pick("global_name")
        .or_else(|| pick("username"))
        .unwrap_or("")
        .to_string()
}

/// channel-discord が author_name に保存する名前と揃える(サーバーでの表示名 → グローバル名 → username)
fn display_name(user: &Value, member: Option<&Value>) -> String {
    member
        .and_then(|m| m.get("nick"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map_or_else(|| user_name(user), str::to_string)
}

pub(crate) fn format_time(ts: Timestamp, tz: &TimeZone) -> String {
    ts.to_zoned(tz.clone())
        .strftime("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

fn format_iso(raw: &str, tz: &TimeZone) -> String {
    raw.parse::<Timestamp>()
        .map_or_else(|_| raw.to_string(), |ts| format_time(ts, tz))
}

/// channel-discord と同じ規則。Noth は `@名前`、allowed_users は表示名、それ以外は `<user>`
pub(crate) fn replace_mentions(
    content: &str,
    viewer: &Viewer,
    names: &HashMap<u64, String>,
) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("<@") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let digits_from = after.strip_prefix('!').unwrap_or(after);
        let digits_len = digits_from.bytes().take_while(u8::is_ascii_digit).count();
        let id = digits_from[..digits_len].parse::<u64>().ok();
        match (id, digits_from[digits_len..].strip_prefix('>')) {
            (Some(id), Some(tail)) => {
                if id == viewer.self_id {
                    out.push('@');
                    out.push_str(viewer.self_name);
                } else if viewer.allowed.contains(&id) {
                    out.push_str(names.get(&id).map_or(MASKED_AUTHOR, String::as_str));
                } else {
                    out.push_str(MASKED_AUTHOR);
                }
                rest = tail;
            }
            _ => {
                out.push_str("<@");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 本文を読んでよいのは allowed_users と Noth 自身の発言だけ。引用・転送・埋め込み・添付は読まない
pub(crate) fn message_summary(msg: &Value, viewer: &Viewer, content_max: usize) -> Value {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let at = msg
        .get("timestamp")
        .and_then(Value::as_str)
        .map(|t| format_iso(t, viewer.tz));
    let author = msg.get("author").unwrap_or(&Value::Null);
    let author_id = author.get("id").and_then(id_of);
    let readable = author_id.is_some_and(|a| a == viewer.self_id || viewer.allowed.contains(&a));
    // bot 判定は id に任せる。webhook は id が Noth や allowed_users と一致しないので伏せられる
    if !readable {
        return json!({"id": id, "author": MASKED_AUTHOR, "at": at});
    }

    let names: HashMap<u64, String> = msg
        .get("mentions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|u| {
            Some((
                u.get("id").and_then(id_of)?,
                display_name(u, u.get("member")),
            ))
        })
        .collect();
    let (content, truncated) = cap_content(
        &replace_mentions(
            msg.get("content").and_then(Value::as_str).unwrap_or(""),
            viewer,
            &names,
        ),
        content_max,
    );
    let mut out = json!({
        "id": id,
        "author": display_name(author, msg.get("member")),
        "at": at,
        "content": content,
    });
    let obj = out.as_object_mut().expect("object");
    if truncated {
        obj.insert("truncated".into(), json!(true));
    }
    if let Some(t) = msg.get("edited_timestamp").and_then(Value::as_str) {
        obj.insert("edited_at".into(), json!(format_iso(t, viewer.tz)));
    }
    if let Some(r) = msg
        .pointer("/message_reference/message_id")
        .filter(|v| v.is_string())
    {
        obj.insert("reply_to".into(), r.clone());
    }
    let attachments = msg
        .get("attachments")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if attachments > 0 {
        obj.insert("attachments".into(), json!(attachments));
    }
    if msg.get("pinned").and_then(Value::as_bool) == Some(true) {
        obj.insert("pinned".into(), json!(true));
    }
    out
}
