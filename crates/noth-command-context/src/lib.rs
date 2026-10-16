//! `/noth context`。各層がトークンをどれだけ使い、圧縮の基準まであとどれだけかを 1 画面にまとめる
use std::sync::Arc;

use async_trait::async_trait;
use noth_core::Result;
use noth_core::command::{Command, CommandEnv, ContextSnapshot, Report, ReportSection};
use noth_core::registry::{ModuleCtx, Registry};

const BAR_WIDTH: usize = 16;

pub fn register(reg: &mut Registry) {
    reg.register_command("command-context", factory);
}

fn factory(_ctx: &ModuleCtx) -> Result<Arc<dyn Command>> {
    Ok(Arc::new(ContextCommand))
}

struct ContextCommand;

#[async_trait]
impl Command for ContextCommand {
    fn name(&self) -> &'static str {
        "context"
    }

    fn description(&self) -> &'static str {
        "main の文脈の使用量を見る"
    }

    async fn run(&self, env: &CommandEnv) -> Result<Report> {
        Ok(render(&env.context().await?))
    }
}

fn render(snap: &ContextSnapshot) -> Report {
    let est_sum: u64 = snap.layers.iter().map(|l| l.est_tokens).sum();
    // 圧縮したあとの実測は古い文脈の大きさなので、合計には使わない
    let measured = snap
        .last_request
        .as_ref()
        .filter(|r| !r.compacted_since)
        .map(|r| r.input_tokens);
    let total = measured.unwrap_or(est_sum);

    // 層ごとの内訳は文字数からの推定でしか出せないので、実測の合計に比例配分して合計を実測に合わせる
    let layer_tokens = |est: u64| match measured {
        Some(m) if est_sum > 0 => (est as u128 * m as u128 / est_sum as u128) as u64,
        _ => est,
    };

    let mut sections = vec![ReportSection {
        heading: "全体".into(),
        body: overall(snap, total),
    }];

    let rows: Vec<String> = snap
        .layers
        .iter()
        .map(|l| {
            let tokens = layer_tokens(l.est_tokens);
            format!(
                "{:<9} {}  {:>7} tok  {:>7}字",
                l.name,
                gauge(tokens, total),
                group(tokens),
                group(l.chars as u64),
            )
        })
        .collect();
    let basis = if measured.is_some() {
        "文字数から推定した比率を、直近の実測の合計に合わせて配分"
    } else {
        "文字数からの推定"
    };
    sections.push(ReportSection {
        heading: "層ごとの使用量".into(),
        body: format!(
            "{}\n_{basis}。Layer 4(今回の発言群)はリクエスト中だけ載るため含みません。_",
            code_block(&rows)
        ),
    });

    sections.push(ReportSection {
        heading: "キャッシュ(直近のリクエスト)".into(),
        body: cache(snap),
    });

    sections.push(ReportSection {
        heading: "補足".into(),
        body: format!(
            "モデル: {}\nLayer 3: {} ターン / {} チャンネル\nLayer 2: {} エピソード\nWorker: 実行中 {} / 待機 {}",
            snap.model,
            snap.turns,
            snap.turn_channels,
            snap.episodes,
            snap.workers_running,
            snap.workers_queued,
        ),
    });

    Report {
        title: "文脈の使用量".into(),
        sections,
    }
}

fn overall(snap: &ContextSnapshot, total: u64) -> String {
    let limit = snap.hard_limit_tokens;
    let source = match &snap.last_request {
        Some(r) if !r.compacted_since => {
            format!("直近のリクエストの実測・{}", ago(r.age_secs))
        }
        Some(_) => "推定。直近の実測は圧縮の前のものなので使っていません".to_string(),
        None => "推定。まだリクエストがありません".to_string(),
    };
    let remaining = if total > limit {
        "基準を超えています。次の応答の前に古いターンを Layer 2 へ圧縮します".to_string()
    } else {
        format!(
            "圧縮の基準まで あと {} トークン(基準 {})",
            group(limit - total),
            group(limit)
        )
    };
    format!(
        "{}\n合計 {} トークン({source})\n{remaining}",
        code_block(&[format!("合計 {}", gauge(total, limit))]),
        group(total),
    )
}

fn cache(snap: &ContextSnapshot) -> String {
    let Some(req) = &snap.last_request else {
        return "リクエストの記録がまだありません".into();
    };
    let total = req.input_tokens;
    let uncached = total.saturating_sub(req.cache_read_tokens + req.cache_write_tokens);
    let mut head = format!("{}のリクエスト", ago(req.age_secs));
    if req.compacted_since {
        head.push_str("(このあとに圧縮したため、いまの文脈とは一致しません)");
    }
    let table = code_block(&[
        cache_row("読み込み", req.cache_read_tokens, total),
        cache_row("書き込み", req.cache_write_tokens, total),
        cache_row("未キャッシュ", uncached, total),
    ]);
    format!("{head}\n{table}")
}

/// 日本語のラベルは全角 2 桁なので、全角の空白で埋めて表の桁をそろえる
fn cache_row(label: &str, tokens: u64, total: u64) -> String {
    const LABEL_WIDTH: usize = 6;
    let pad = "　".repeat(LABEL_WIDTH.saturating_sub(label.chars().count()));
    format!(
        "{label}{pad} {:>7} tok ({:>3}%)",
        group(tokens),
        percent(tokens, total)
    )
}

fn ago(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}秒前"),
        60..3600 => format!("{}分前", secs / 60),
        _ => format!("{}時間前", secs / 3600),
    }
}

fn percent(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        0
    } else {
        (part as u128 * 100 / whole as u128) as u64
    }
}

fn gauge(part: u64, whole: u64) -> String {
    let filled = if whole == 0 {
        0
    } else {
        ((part as u128 * BAR_WIDTH as u128).div_ceil(whole as u128) as usize).min(BAR_WIDTH)
    };
    format!(
        "{}{} {:>3}%",
        "█".repeat(filled),
        "░".repeat(BAR_WIDTH - filled),
        percent(part, whole)
    )
}

fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn code_block(lines: &[String]) -> String {
    format!("```\n{}\n```", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use noth_core::command::{ContextSource, LayerSize, RequestUsage};

    use super::*;

    struct Fixed(ContextSnapshot);

    #[async_trait]
    impl ContextSource for Fixed {
        async fn context_snapshot(&self) -> Result<ContextSnapshot> {
            Ok(self.0.clone())
        }
    }

    fn layer(name: &'static str, est: u64) -> LayerSize {
        LayerSize {
            name,
            chars: est as usize * 2,
            est_tokens: est,
        }
    }

    fn usage(input: u64, read: u64, write: u64) -> RequestUsage {
        RequestUsage {
            input_tokens: input,
            cache_read_tokens: read,
            cache_write_tokens: write,
            age_secs: 300,
            compacted_since: false,
        }
    }

    fn snapshot(last_request: Option<RequestUsage>) -> ContextSnapshot {
        ContextSnapshot {
            model: "model-x".into(),
            layers: vec![
                layer("tools", 100),
                layer("Layer 0", 300),
                layer("Layer 3", 600),
            ],
            last_request,
            hard_limit_tokens: 2_000,
            turns: 12,
            turn_channels: 3,
            episodes: 2,
            workers_running: 1,
            workers_queued: 0,
        }
    }

    fn body<'a>(report: &'a Report, heading: &str) -> &'a str {
        &report
            .sections
            .iter()
            .find(|s| s.heading == heading)
            .unwrap_or_else(|| panic!("{heading}"))
            .body
    }

    #[test]
    fn group_inserts_thousand_separators() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1_000), "1,000");
        assert_eq!(group(1_234_567), "1,234,567");
    }

    #[test]
    fn gauge_clamps_and_shows_nonzero_part() {
        assert_eq!(gauge(0, 100), format!("{} {:>3}%", "░".repeat(16), 0));
        assert!(gauge(1, 1000).starts_with("█░"));
        assert_eq!(
            gauge(50, 100),
            format!("{}{} {:>3}%", "█".repeat(8), "░".repeat(8), 50)
        );
        assert!(gauge(300, 100).starts_with(&"█".repeat(16)));
        assert!(gauge(300, 100).ends_with("300%"));
        assert!(gauge(5, 0).ends_with("0%"));
    }

    #[test]
    fn measured_total_scales_layers_and_reports_remaining() {
        let report = render(&snapshot(Some(usage(1_000, 800, 100))));
        let overall = body(&report, "全体");
        assert!(
            overall.contains("合計 1,000 トークン(直近のリクエストの実測・5分前)"),
            "{overall}"
        );
        assert!(
            overall.contains("あと 1,000 トークン(基準 2,000)"),
            "{overall}"
        );
        assert!(overall.contains(" 50%"), "{overall}");
        let layers = body(&report, "層ごとの使用量");
        // 推定 100:300:600 を実測 1,000 に合わせると同じ値になる
        assert!(layers.contains("Layer 3"));
        assert!(layers.contains("600 tok"), "{layers}");
        assert!(layers.contains(" 60%"), "{layers}");
        let cache = body(&report, "キャッシュ(直近のリクエスト)");
        assert!(cache.contains("800 tok ( 80%)"), "{cache}");
        assert!(cache.contains("100 tok ( 10%)"), "{cache}");
        assert!(cache.contains("未キャッシュ"), "{cache}");
        assert!(cache.starts_with("5分前のリクエスト"), "{cache}");
        let extra = body(&report, "補足");
        for needle in [
            "model-x",
            "12 ターン / 3 チャンネル",
            "2 エピソード",
            "実行中 1 / 待機 0",
        ] {
            assert!(extra.contains(needle), "{extra}");
        }
    }

    #[test]
    fn without_a_request_falls_back_to_estimate() {
        let report = render(&snapshot(None));
        let overall = body(&report, "全体");
        assert!(
            overall.contains("合計 1,000 トークン(推定。まだリクエストがありません)"),
            "{overall}"
        );
        assert!(body(&report, "層ごとの使用量").contains("文字数からの推定"));
        assert!(body(&report, "キャッシュ(直近のリクエスト)").contains("記録がまだありません"));
    }

    #[test]
    fn over_limit_is_called_out() {
        let report = render(&snapshot(Some(usage(2_500, 0, 0))));
        assert!(body(&report, "全体").contains("基準を超えています"));
        assert!(body(&report, "全体").contains("125%"));
    }

    #[test]
    fn measurement_before_a_compaction_is_not_used_for_the_total() {
        let mut stale = usage(9_000, 8_000, 500);
        stale.compacted_since = true;
        let report = render(&snapshot(Some(stale)));
        let overall = body(&report, "全体");
        assert!(overall.contains("合計 1,000 トークン(推定。"), "{overall}");
        assert!(overall.contains("圧縮の前のもの"), "{overall}");
        let cache = body(&report, "キャッシュ(直近のリクエスト)");
        assert!(cache.contains("このあとに圧縮した"), "{cache}");
        assert!(cache.contains("8,000 tok"), "{cache}");
    }

    #[test]
    fn cache_labels_are_padded_to_the_same_width() {
        let report = render(&snapshot(Some(usage(1_000, 800, 100))));
        let cache = body(&report, "キャッシュ(直近のリクエスト)");
        let widths: Vec<usize> = cache
            .lines()
            .filter(|l| l.contains(" tok ("))
            .map(|l| l.split(' ').next().unwrap().chars().count())
            .collect();
        assert_eq!(widths, [6, 6, 6], "{cache}");
    }

    #[test]
    fn ago_picks_a_readable_unit() {
        assert_eq!(ago(5), "5秒前");
        assert_eq!(ago(59), "59秒前");
        assert_eq!(ago(60), "1分前");
        assert_eq!(ago(3599), "59分前");
        assert_eq!(ago(7200), "2時間前");
    }

    #[tokio::test]
    async fn command_reads_snapshot_through_env() {
        let env = CommandEnv::new(Arc::new(Fixed(snapshot(None))));
        let report = ContextCommand.run(&env).await.unwrap();
        assert_eq!(report.title, "文脈の使用量");
        assert_eq!(report.sections.len(), 4);
    }
}
