use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use jiff::Timestamp;
use noth_core::config::{Config, lookup, masked_raw_table};
use noth_core::store::{CostGroup, CostSummaryRow, Store};

use super::{message, pad, pad_left, paths};

mod format;
mod period;

use format::{display_width, format_cost, format_span, format_units, format_usd, group_digits};
pub use period::By;
use period::{parse_period, today_start_ms};

fn total(rows: &[CostSummaryRow]) -> CostSummaryRow {
    let mut t = CostSummaryRow {
        key: "合計".into(),
        calls: 0,
        input_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 0,
        units: Vec::new(),
        amount_usd: 0.0,
        unpriced_calls: 0,
    };
    let mut units: BTreeMap<String, u64> = BTreeMap::new();
    for r in rows {
        t.calls += r.calls;
        t.input_tokens += r.input_tokens;
        t.cache_read_tokens += r.cache_read_tokens;
        t.cache_write_tokens += r.cache_write_tokens;
        t.output_tokens += r.output_tokens;
        t.amount_usd += r.amount_usd;
        t.unpriced_calls += r.unpriced_calls;
        for (name, n) in &r.units {
            *units.entry(name.clone()).or_default() += n;
        }
    }
    t.units = units.into_iter().collect();
    t
}

fn print_table(by: By, rows: &[CostSummaryRow]) {
    let header = [
        by.header(),
        "回数",
        "入力",
        "cache読",
        "cache書",
        "出力",
        "単位",
        "コスト",
    ];
    let t = total(rows);
    let cells = |r: &CostSummaryRow| {
        [
            r.key.clone(),
            group_digits(r.calls),
            group_digits(r.input_tokens),
            group_digits(r.cache_read_tokens),
            group_digits(r.cache_write_tokens),
            group_digits(r.output_tokens),
            format_units(&r.units),
            format_cost(r.amount_usd, r.unpriced_calls),
        ]
    };
    let mut table: Vec<[String; 8]> = rows.iter().map(cells).collect();
    table.push(cells(&t));
    let mut widths: Vec<usize> = header.iter().map(|h| display_width(h)).collect();
    for row in &table {
        for (w, c) in widths.iter_mut().zip(row) {
            *w = (*w).max(display_width(c));
        }
    }
    let line = |row: &[String]| {
        row.iter()
            .enumerate()
            .map(|(i, c)| {
                // 先頭(キー)と単位は左寄せ、数値は右寄せ
                if i == 0 || i == 6 {
                    pad(c, widths[i])
                } else {
                    pad_left(c, widths[i])
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    println!(
        "{}",
        line(&header.iter().map(|h| h.to_string()).collect::<Vec<_>>()).trim_end()
    );
    let last = table.len() - 1;
    for (i, row) in table.iter().enumerate() {
        if i == last {
            println!(
                "{}",
                "-".repeat(widths.iter().sum::<usize>() + 2 * (widths.len() - 1))
            );
        }
        println!("{}", line(row).trim_end());
    }
}

/// config.toml の guild_id。`${VAR}` が未解決でも読めるよう、生の設定から取る
pub(crate) fn guild_id(cfg: Option<&Path>) -> Result<u64> {
    let raw = masked_raw_table(&paths(cfg)?).map_err(|e| anyhow::anyhow!(message(&e)))?;
    let v = lookup(&raw, "channel.discord.guild_id");
    let id = match v {
        Some(toml::Value::String(s)) => s.parse().ok(),
        Some(toml::Value::Integer(i)) => u64::try_from(*i).ok(),
        _ => None,
    };
    id.ok_or_else(|| anyhow::anyhow!("channel.discord.guild_id が設定されていません"))
}

async fn open_store(cfg: Option<&Path>) -> Result<Option<Store>> {
    let db = paths(cfg)?.database();
    // Store::open は DB を作ってしまうので、未起動の環境では開かない
    if !db.exists() {
        println!("記録がありません(まだ起動していません)");
        return Ok(None);
    }
    Ok(Some(Store::open(&db).await?))
}

pub async fn run(cfg: Option<&Path>, since: &str, by: By, offline: bool) -> Result<ExitCode> {
    let now_ms = Timestamp::now().as_millisecond();
    let since_ms = parse_period(since, now_ms, today_start_ms())?;
    let guild = guild_id(cfg)?;
    let Some(store) = open_store(cfg).await? else {
        return Ok(ExitCode::SUCCESS);
    };

    let rows = store.cost_summary(guild, since_ms, by.group()).await?;
    println!("期間: {since}(guild {guild})");
    if rows.is_empty() {
        println!("この期間の記録はありません");
    } else {
        print_table(by, &rows);
    }

    let limits = store.rate_limits(guild).await?;
    if !limits.is_empty() {
        println!();
        println!("レート制限(直近の記録):");
        let now_s = now_ms / 1000;
        for (l, recorded_ms) in &limits {
            let reset = match l.reset_at {
                Some(at) if at > now_s => format!("リセットまで {}", format_span(at - now_s)),
                Some(_) => "リセット済み".to_string(),
                None => "リセット時刻不明".to_string(),
            };
            println!(
                "  {} {}: 残り {}/{}  {reset}  ({}前に記録)",
                l.source,
                l.bucket,
                group_digits(l.remaining),
                group_digits(l.limit),
                format_span(now_s - recorded_ms / 1000)
            );
        }
    }

    if !offline {
        print_accounts(cfg).await;
    }
    Ok(ExitCode::SUCCESS)
}

/// 課金先アカウントの状態。外部 API を呼ぶので、設定や疎通の失敗は警告にとどめる
async fn print_accounts(cfg: Option<&Path>) {
    let built = super::load(cfg).and_then(|c: Config| {
        crate::registry()
            .build(&c)
            .map_err(|e| anyhow::anyhow!(message(&e)))
    });
    let modules = match built {
        Ok(m) => m,
        Err(e) => {
            eprintln!("警告: アカウント状態を取得できません(--offline で省略できます): {e:#}");
            return;
        }
    };
    let mut lines = Vec::new();
    for (name, tool) in &modules.tools {
        match tool.account_status().await {
            Ok(Some(status)) => lines.push(format!("  {name}: {status}")),
            Ok(None) => {}
            Err(e) => lines.push(format!("  {name}: 取得に失敗しました({})", message(&e))),
        }
    }
    if !lines.is_empty() {
        println!();
        println!("アカウント:");
        for l in lines {
            println!("{l}");
        }
    }
}

/// `noth status` 用の 1 行。例: `今日のコスト: $0.0123(LLM 42 回、TinyFish 5 回)`
pub async fn today_line(store: &Store, guild: u64) -> Result<String> {
    let rows = store
        .cost_summary(guild, today_start_ms(), CostGroup::Source)
        .await?;
    Ok(summary_line(&rows))
}

fn source_label(source: &str) -> &str {
    if source.starts_with("provider-") || source.starts_with("router-") {
        "LLM"
    } else if source.starts_with("tool-web-") {
        "TinyFish"
    } else {
        source
    }
}

fn summary_line(rows: &[CostSummaryRow]) -> String {
    let t = total(rows);
    let mut calls: Vec<(&str, u64)> = Vec::new();
    for r in rows {
        let label = source_label(&r.key);
        match calls.iter_mut().find(|(l, _)| *l == label) {
            Some((_, n)) => *n += r.calls,
            None => calls.push((label, r.calls)),
        }
    }
    if calls.is_empty() {
        return format!("今日のコスト: {}", format_usd(0.0));
    }
    let detail = calls
        .iter()
        .map(|(l, n)| format!("{l} {n} 回"))
        .collect::<Vec<_>>()
        .join("、");
    format!(
        "今日のコスト: {}({detail})",
        format_cost(t.amount_usd, t.unpriced_calls)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;
    const TODAY: i64 = NOW - 5 * 3_600_000;

    #[test]
    fn period_parsing() {
        assert_eq!(parse_period("all", NOW, TODAY).unwrap(), 0);
        assert_eq!(parse_period("today", NOW, TODAY).unwrap(), TODAY);
        assert_eq!(parse_period("24h", NOW, TODAY).unwrap(), NOW - 86_400_000);
        assert_eq!(
            parse_period("7d", NOW, TODAY).unwrap(),
            NOW - 7 * 86_400_000
        );
        assert_eq!(
            parse_period("30d", NOW, TODAY).unwrap(),
            NOW - 30 * 86_400_000
        );
        for bad in ["", "d", "0d", "-1d", "7", "7w", "xd", "1.5d"] {
            assert!(parse_period(bad, NOW, TODAY).is_err(), "{bad}");
        }
    }

    #[test]
    fn number_formatting() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(1_234_567), "1,234,567");
        assert_eq!(format_usd(0.0123), "$0.0123");
        assert_eq!(format_usd(0.0), "$0.0000");
        assert_eq!(format_usd(12.5), "$12.5000");
        assert_eq!(format_cost(0.5, 0), "$0.5000");
        assert_eq!(format_cost(0.5, 3), "$0.5000 +不明 3 件");
        assert_eq!(format_units(&[]), "-");
        assert_eq!(
            format_units(&[("step".into(), 12), ("url".into(), 1500)]),
            "12 step, 1,500 url"
        );
        assert_eq!(format_span(45), "45秒");
        assert_eq!(format_span(180), "3分");
        assert_eq!(format_span(7200), "2時間");
        assert_eq!(format_span(172_800), "2日");
        assert_eq!(format_span(-5), "0秒");
    }

    fn row(key: &str, calls: u64, amount: f64, unpriced: u64) -> CostSummaryRow {
        CostSummaryRow {
            key: key.into(),
            calls,
            input_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            units: Vec::new(),
            amount_usd: amount,
            unpriced_calls: unpriced,
        }
    }

    #[test]
    fn status_line_groups_sources() {
        let rows = [
            row("provider-anthropic", 40, 0.01, 0),
            row("router-systemone", 2, 0.0023, 0),
            row("tool-web-search", 3, 0.0, 0),
            row("tool-web-fetch", 2, 0.0, 0),
        ];
        assert_eq!(
            summary_line(&rows),
            "今日のコスト: $0.0123(LLM 42 回、TinyFish 5 回)"
        );
        assert_eq!(summary_line(&[]), "今日のコスト: $0.0000");
    }
}
