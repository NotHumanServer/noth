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
    /// REST の応答に `member` が無いときの補い。サーバーでのニックネーム
    pub(crate) nicks: HashMap<u64, String>,
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

/// サーバーでのニックネームとグローバル名の両方を示す。Noth が普段使う呼び名(ニックネーム)と、
/// アカウント本来の名前の両方で人を指せるようにするため。同じ、または片方だけなら 1 つ
fn display_name(user: &Value, member: Option<&Value>, nicks: &HashMap<u64, String>) -> String {
    let nick = member
        .and_then(|m| m.get("nick"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| nicks.get(&user.get("id").and_then(id_of)?).cloned());
    let global = user_name(user);
    match nick {
        Some(n) if global.is_empty() || n == global => n,
        Some(n) => format!("{n}（{global}）"),
        None => global,
    }
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

/// embed の title / description / fields を 1 つのテキストにする。Bot の通知は本文が embed にあることが多い
fn embed_text(msg: &Value) -> String {
    let text = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    msg.get("embeds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|e| {
            let mut parts: Vec<String> = ["title", "description"]
                .iter()
                .filter_map(|k| text(e, k))
                .collect();
            parts.extend(
                e.get("fields")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|f| {
                        format!(
                            "{}: {}",
                            text(f, "name").unwrap_or_default(),
                            text(f, "value").unwrap_or_default()
                        )
                    }),
            );
            parts.join("\n")
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 本文を読んでよいのは allowed_users・Noth 自身・他の Bot の発言だけ。引用・転送・添付は読まない。
/// Bot の embed だけは本文として読む(Bot の本文は文脈に入れず、Worker がここで読む設計のため)
pub(crate) fn message_summary(msg: &Value, viewer: &Viewer, content_max: usize) -> Value {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let at = msg
        .get("timestamp")
        .and_then(Value::as_str)
        .map(|t| format_iso(t, viewer.tz));
    let author = msg.get("author").unwrap_or(&Value::Null);
    let author_id = author.get("id").and_then(id_of);
    let readable = author_id.is_some_and(|a| a == viewer.self_id || viewer.allowed.contains(&a));
    // webhook の発言も author.bot が true になるが、誰でも名乗れるので Bot としては扱わず伏せる
    let is_bot = author_id != Some(viewer.self_id)
        && author.get("bot").and_then(Value::as_bool) == Some(true)
        && msg.get("webhook_id").is_none_or(Value::is_null);
    if !readable && !is_bot {
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
                display_name(u, u.get("member"), &viewer.nicks),
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
        "author": display_name(author, msg.get("member"), &viewer.nicks),
        "at": at,
        "content": content,
    });
    let obj = out.as_object_mut().expect("object");
    if is_bot {
        obj.insert("bot".into(), json!(true));
        let embed = embed_text(msg);
        if !embed.is_empty() {
            let (embed, embed_truncated) =
                cap_content(&replace_mentions(&embed, viewer, &names), content_max);
            obj.insert("embed".into(), json!(embed));
            if embed_truncated {
                obj.insert("embed_truncated".into(), json!(true));
            }
        }
    }
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
