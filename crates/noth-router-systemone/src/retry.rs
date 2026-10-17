use std::time::Duration;

const MAX_RETRIES: u32 = 2;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(5);

/// 再試行するなら待ち時間を返す。Retry-After が長すぎる場合は待たずに失敗させる
pub(crate) fn retry_delay(
    status: u16,
    retry_after: Option<Duration>,
    attempt: u32,
) -> Option<Duration> {
    if attempt >= MAX_RETRIES || !matches!(status, 429 | 500..=599) {
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
