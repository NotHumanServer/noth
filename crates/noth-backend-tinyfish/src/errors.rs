use std::time::Duration;

use noth_core::{Error, Result};
use reqwest::Url;
use serde_json::Value;

pub(crate) const ERROR_BODY_LIMIT: usize = 200;
const MAX_RETRIES: u32 = 2;
/// これより長い待ちを求められたら、worker の timeout を食い潰すので待たずに失敗させる
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);

/// 日次枠や無料枠の枯渇は待っても回復しない(Retry-After も付かない)ので再試行しない
pub(crate) fn quota_exhausted(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/code")?.as_str().map(str::to_string))
        .is_some_and(|c| {
            matches!(
                c.as_str(),
                "DAILY_LIMIT_EXCEEDED" | "FREE_ALLOWANCE_EXHAUSTED"
            )
        })
}

pub(crate) fn parse_url(s: &str) -> Result<Url> {
    Url::parse(s).map_err(|e| Error::Config(format!("tinyfish: URL `{s}` が不正です: {e}")))
}

/// `{error:{code,message}}` を1行にする。形が違えば本文をそのまま切り詰める
pub(crate) fn error_message(body: &str) -> String {
    let parsed = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        let e = v.get("error")?;
        let code = e.get("code").and_then(Value::as_str).unwrap_or("UNKNOWN");
        let message = e.get("message").and_then(Value::as_str).unwrap_or("");
        Some(format!("TinyFish エラー {code}: {message}"))
    });
    truncate(&parsed.unwrap_or_else(|| format!("TinyFish エラー: {body}")))
}

pub(crate) fn truncate(s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(ERROR_BODY_LIMIT) {
        Some((i, _)) => format!("{}...", &one_line[..i]),
        None => one_line,
    }
}

/// 再試行するなら待ち時間を返す。400 などクライアント側の誤りは繰り返しても枠を消費するだけなので対象外
pub(crate) fn retry_delay(
    status: u16,
    retry_after: Option<Duration>,
    attempt: u32,
) -> Option<Duration> {
    if attempt >= MAX_RETRIES || !matches!(status, 429 | 502 | 503 | 504) {
        return None;
    }
    match retry_after {
        Some(d) if d > MAX_RETRY_AFTER => None,
        Some(d) => Some(d),
        None => Some(Duration::from_millis(500 << attempt)),
    }
}

pub(crate) fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let secs = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}
