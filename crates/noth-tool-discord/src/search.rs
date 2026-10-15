//! search_messages の引数解釈。検索そのものは `MessageHistory` が行う
use jiff::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use noth_core::history::HistoryQuery;
use noth_core::types::GuildId;
use serde_json::Value;

use crate::args::{clamp_limit, text_arg};

const DEFAULT_LIMIT: usize = 30;
/// Discord の snowflake はこれより長い。短い数字列(「2025」など)はチャンネル名として扱う
const MIN_SNOWFLAKE_DIGITS: usize = 15;

pub(crate) fn build_query(
    input: &Value,
    guild_id: GuildId,
    tz: &TimeZone,
) -> Result<HistoryQuery, String> {
    let mut q = HistoryQuery {
        guild_id,
        channel_id: None,
        channel_name: None,
        text: text_arg(input, "query"),
        author: text_arg(input, "author"),
        since: None,
        until: None,
        limit: clamp_limit(input, DEFAULT_LIMIT),
    };
    let channel = match input.get("channel_id") {
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => text_arg(input, "channel_id"),
    };
    if let Some(ch) = channel {
        let name = ch.trim_start_matches('#').trim();
        if name.len() >= MIN_SNOWFLAKE_DIGITS && name.bytes().all(|b| b.is_ascii_digit()) {
            q.channel_id = name.parse().ok();
        } else if !name.is_empty() {
            q.channel_name = Some(name.to_string());
        }
    }
    if let Some(s) = text_arg(input, "since") {
        q.since = Some(parse_time(&s, tz, false).map_err(|e| format!("since: {e}"))?);
    }
    if let Some(s) = text_arg(input, "until") {
        q.until = Some(parse_time(&s, tz, true).map_err(|e| format!("until: {e}"))?);
    }
    Ok(q)
}

/// オフセット付きはそのまま、無ければ `tz` のローカル時刻。日付だけの `until` は翌日 0 時(その日を含める)
fn parse_time(s: &str, tz: &TimeZone, end_of_day: bool) -> Result<Timestamp, String> {
    let bad = || {
        format!(
            "日時を解釈できません: `{s}`(例: 2026-10-01 / 2026-10-01T09:00:00 / 2026-10-01T09:00:00+09:00)"
        )
    };
    if let Ok(ts) = s.parse::<Timestamp>() {
        return Ok(ts);
    }
    // DateTime のパーサは日付だけの文字列も 0 時として受けてしまうので、日付だけかどうかを先に見分ける
    let date_only = !s.contains(['T', 't', ' ', ':']);
    let zoned = if date_only {
        let date: Date = s.parse().map_err(|_| bad())?;
        let date = if end_of_day {
            date.tomorrow().map_err(|_| bad())?
        } else {
            date
        };
        date.to_zoned(tz.clone())
    } else {
        s.parse::<DateTime>()
            .map_err(|_| bad())?
            .to_zoned(tz.clone())
    };
    zoned.map(|z| z.timestamp()).map_err(|_| bad())
}
