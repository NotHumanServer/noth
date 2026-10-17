use anyhow::{Result, bail};
use clap::ValueEnum;
use jiff::Zoned;
use noth_core::store::CostGroup;

#[derive(Clone, Copy, ValueEnum)]
pub enum By {
    Source,
    Model,
    Purpose,
    Day,
    Run,
}

impl By {
    pub(super) fn group(self) -> CostGroup {
        match self {
            By::Source => CostGroup::Source,
            By::Model => CostGroup::Model,
            By::Purpose => CostGroup::Purpose,
            By::Day => CostGroup::Day,
            By::Run => CostGroup::Run,
        }
    }
    pub(super) fn header(self) -> &'static str {
        match self {
            By::Source => "source",
            By::Model => "model",
            By::Purpose => "purpose",
            By::Day => "日付",
            By::Run => "run",
        }
    }
}

/// `today`(ローカル時間の 0 時)/ `<n>h` / `<n>d` / `all` を、集計の開始時刻(UNIX ミリ秒)にする
pub fn parse_period(s: &str, now_ms: i64, today_start_ms: i64) -> Result<i64> {
    const HOUR_MS: i64 = 3_600_000;
    let s = s.trim();
    match s {
        "all" => return Ok(0),
        "today" => return Ok(today_start_ms),
        _ => {}
    }
    let unit_ms = match s.chars().last() {
        Some('h') => HOUR_MS,
        Some('d') => 24 * HOUR_MS,
        _ => bail!("期間は today / 24h / 7d / 30d / all の形式で指定してください: `{s}`"),
    };
    let n: i64 = s[..s.len() - 1]
        .parse()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| anyhow::anyhow!("期間の数値が不正です: `{s}`"))?;
    Ok(now_ms.saturating_sub(n.saturating_mul(unit_ms)))
}

pub(super) fn today_start_ms() -> i64 {
    let now = Zoned::now();
    now.start_of_day()
        .map_or(0, |z| z.timestamp().as_millisecond())
}
