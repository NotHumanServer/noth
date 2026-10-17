//! `Report` を Discord に送る JSON にする。serenity 0.12.5 は Components V2 の型を持たないので、生の JSON で組む
use noth_core::command::Report;
use serde_json::{Value, json};

pub(crate) const FLAG_EPHEMERAL: u64 = 1 << 6;
pub(crate) const FLAG_IS_COMPONENTS_V2: u64 = 1 << 15;

/// 公式の Component Reference に書かれた、メッセージ 1 通の部品数の上限(入れ子を含む)
const MAX_COMPONENTS: usize = 40;
/// Text Display の本文を全部足した上限。公式の Component Reference には数値が無く、discord.js ガイドの記述による。
/// 超えると送信が 400 になり、V2 を断念して embed に落ちるので、その前に切り詰める
const MAX_V2_TEXT_CHARS: usize = 4000;
const EMBED_FIELD_VALUE_MAX: usize = 1024;
const EMBED_MAX_FIELDS: usize = 25;
const EMBED_TOTAL_MAX: usize = 6000;
const CONTENT_MAX: usize = 2000;
const ACCENT_COLOR: u32 = 0x5865F2;

/// Container(1) + タイトル + 区切り + 末尾の区切り + Action Row + ボタン。残りを節ごとの Text Display と区切りに使う
const FIXED_COMPONENTS: usize = 6;
const COMPONENTS_PER_SECTION: usize = 2;

pub(crate) fn refresh_custom_id(command: &str) -> String {
    format!("noth:refresh:{command}")
}

pub(crate) fn parse_refresh(custom_id: &str) -> Option<&str> {
    custom_id.strip_prefix("noth:refresh:")
}

/// 切り詰めたことが分かるよう末尾に「…」を付ける。コードブロックの途中で切れたら閉じる
pub(crate) fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    if out.matches("```").count() % 2 == 1 {
        let closing = "\n```";
        let room = max_chars.saturating_sub(closing.chars().count());
        out = out.chars().take(room.saturating_sub(1)).collect::<String>() + "…" + closing;
    }
    out
}

struct Texts {
    title: String,
    sections: Vec<String>,
}

/// 上限に収まるよう、節を減らし、本文の長い節から切り詰める
fn fit_v2(report: &Report) -> Texts {
    let max_sections = (MAX_COMPONENTS - FIXED_COMPONENTS) / COMPONENTS_PER_SECTION;
    let mut sections: Vec<String> = report
        .sections
        .iter()
        .take(max_sections)
        .map(|s| format!("**{}**\n{}", s.heading, s.body))
        .collect();
    let title = truncate(&format!("## {}", report.title), 200);
    let mut total =
        title.chars().count() + sections.iter().map(|s| s.chars().count()).sum::<usize>();
    while total > MAX_V2_TEXT_CHARS {
        let excess = total - MAX_V2_TEXT_CHARS;
        let Some((i, longest)) = sections
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.chars().count()))
            .max_by_key(|(_, n)| *n)
        else {
            break;
        };
        let target = longest.saturating_sub(excess).max(1);
        sections[i] = truncate(&sections[i], target);
        let now: usize = sections.iter().map(|s| s.chars().count()).sum();
        let new_total = title.chars().count() + now;
        if new_total >= total {
            break;
        }
        total = new_total;
    }
    Texts { title, sections }
}

fn text_display(content: &str) -> Value {
    json!({"type": 10, "content": content})
}

fn separator() -> Value {
    json!({"type": 14, "divider": true, "spacing": 1})
}

/// Components V2 のメッセージ。`refresh` に `/noth <command>` の名前を渡すと「更新」ボタンを付ける
pub(crate) fn v2_message(report: &Report, refresh: Option<&str>) -> Value {
    let texts = fit_v2(report);
    let mut inner = vec![text_display(&texts.title), separator()];
    for (i, section) in texts.sections.iter().enumerate() {
        if i > 0 {
            inner.push(separator());
        }
        inner.push(text_display(section));
    }
    if let Some(command) = refresh {
        inner.push(separator());
        inner.push(json!({
            "type": 1,
            "components": [{
                "type": 2,
                "style": 2,
                "label": "更新",
                "custom_id": refresh_custom_id(command),
            }],
        }));
    }
    json!({
        "flags": FLAG_IS_COMPONENTS_V2,
        "components": [{"type": 17, "accent_color": ACCENT_COLOR, "components": inner}],
        "allowed_mentions": {"parse": []},
    })
}

/// V2 を送れなかったときの代わり。ephemeral かどうかは呼び出し側が flags を足して決める
pub(crate) fn embed_message(report: &Report) -> Value {
    let mut budget = EMBED_TOTAL_MAX.saturating_sub(report.title.chars().count());
    let mut fields = Vec::new();
    for section in report.sections.iter().take(EMBED_MAX_FIELDS) {
        let name = truncate(&section.heading, 256);
        let value_max = EMBED_FIELD_VALUE_MAX.min(budget.saturating_sub(name.chars().count()));
        if value_max == 0 {
            break;
        }
        let value = truncate(&section.body, value_max);
        budget = budget.saturating_sub(name.chars().count() + value.chars().count());
        fields.push(json!({"name": name, "value": value, "inline": false}));
    }
    json!({
        "embeds": [{
            "title": truncate(&report.title, 256),
            "color": ACCENT_COLOR,
            "fields": fields,
        }],
        "allowed_mentions": {"parse": []},
    })
}

/// embed も送れなかったときの最後の手段
pub(crate) fn text_message(report: &Report) -> Value {
    let mut text = format!("**{}**", report.title);
    for section in &report.sections {
        text.push_str(&format!("\n\n**{}**\n{}", section.heading, section.body));
    }
    json!({
        "content": truncate(&text, CONTENT_MAX),
        "allowed_mentions": {"parse": []},
    })
}

pub(crate) fn plain(content: &str) -> Value {
    json!({
        "content": truncate(content, CONTENT_MAX),
        "allowed_mentions": {"parse": []},
    })
}

/// テストと診断用。メッセージ全体の部品数(入れ子を含む)
#[cfg(test)]
pub(crate) fn count_components(components: &[Value]) -> usize {
    components
        .iter()
        .map(|c| {
            1 + c
                .get("components")
                .and_then(Value::as_array)
                .map_or(0, |inner| count_components(inner))
        })
        .sum()
}

/// テストと診断用。Text Display の本文の合計文字数
#[cfg(test)]
pub(crate) fn count_text(components: &[Value]) -> usize {
    components
        .iter()
        .map(|c| {
            let own = c
                .get("content")
                .and_then(Value::as_str)
                .map_or(0, |s| s.chars().count());
            own + c
                .get("components")
                .and_then(Value::as_array)
                .map_or(0, |inner| count_text(inner))
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use noth_core::command::ReportSection;

    use super::*;

    fn report(sections: usize, body_chars: usize) -> Report {
        Report {
            title: "文脈の使用量".into(),
            sections: (0..sections)
                .map(|i| ReportSection {
                    heading: format!("節{i}"),
                    body: "あ".repeat(body_chars),
                })
                .collect(),
        }
    }

    fn components(msg: &Value) -> &Vec<Value> {
        msg["components"].as_array().unwrap()
    }

    #[test]
    fn v2_shape_has_flag_container_and_refresh_button() {
        let msg = v2_message(&report(2, 10), Some("context"));
        assert_eq!(msg["flags"], FLAG_IS_COMPONENTS_V2);
        assert!(msg.get("content").is_none() && msg.get("embeds").is_none());
        let container = &components(&msg)[0];
        assert_eq!(container["type"], 17);
        let inner = container["components"].as_array().unwrap();
        assert_eq!(inner[0], json!({"type": 10, "content": "## 文脈の使用量"}));
        assert_eq!(inner[1]["type"], 14);
        assert_eq!(inner[2]["content"], format!("**節0**\n{}", "あ".repeat(10)));
        let row = inner.last().unwrap();
        assert_eq!(row["type"], 1);
        assert_eq!(row["components"][0]["custom_id"], "noth:refresh:context");
        assert_eq!(row["components"][0]["style"], 2);
        assert_eq!(parse_refresh("noth:refresh:context"), Some("context"));
        assert_eq!(parse_refresh("other"), None);
    }

    #[test]
    fn v2_without_refresh_has_no_action_row() {
        let msg = v2_message(&report(1, 5), None);
        let inner = components(&msg)[0]["components"].as_array().unwrap();
        assert!(inner.iter().all(|c| c["type"] != 1));
    }

    #[test]
    fn v2_text_is_trimmed_to_limit() {
        let msg = v2_message(&report(4, 3000), Some("context"));
        assert!(count_text(components(&msg)) <= MAX_V2_TEXT_CHARS);
        assert!(count_text(components(&msg)) > MAX_V2_TEXT_CHARS - 50);
        let inner = components(&msg)[0]["components"].as_array().unwrap();
        assert!(
            inner
                .iter()
                .any(|c| c["content"].as_str().is_some_and(|s| s.ends_with('…')))
        );
    }

    #[test]
    fn v2_component_count_is_capped() {
        let msg = v2_message(&report(60, 5), Some("context"));
        assert!(count_components(components(&msg)) <= MAX_COMPONENTS);
        assert!(count_text(components(&msg)) <= MAX_V2_TEXT_CHARS);
    }

    #[test]
    fn truncate_closes_a_cut_code_block() {
        let text = format!("```\n{}\n```", "x".repeat(100));
        let cut = truncate(&text, 40);
        assert!(cut.chars().count() <= 40, "{cut}");
        assert_eq!(cut.matches("```").count() % 2, 0, "{cut}");
        assert_eq!(truncate("short", 40), "short");
    }

    #[test]
    fn embed_fields_respect_limits() {
        let msg = embed_message(&report(30, 2000));
        let embed = &msg["embeds"][0];
        let fields = embed["fields"].as_array().unwrap();
        assert!(fields.len() <= EMBED_MAX_FIELDS);
        let mut total = embed["title"].as_str().unwrap().chars().count();
        for f in fields {
            let v = f["value"].as_str().unwrap().chars().count();
            assert!(v <= EMBED_FIELD_VALUE_MAX);
            total += v + f["name"].as_str().unwrap().chars().count();
        }
        assert!(total <= EMBED_TOTAL_MAX, "{total}");
        assert!(msg.get("components").is_none());
    }

    #[test]
    fn text_message_fits_content_limit() {
        let msg = text_message(&report(5, 2000));
        assert!(msg["content"].as_str().unwrap().chars().count() <= CONTENT_MAX);
        assert_eq!(msg["allowed_mentions"], json!({"parse": []}));
    }
}
