//! Worker の進捗を Ack の発言に追記して見せる(Hermes 風の進捗表示)
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::traits::{Channel, WorkerProgress};
use crate::types::{ChannelId, MessageId};

/// Discord の 1 発言の上限
pub const MAX_CHARS: usize = 2_000;
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(1_500);
const FOOTER_FINISHED: &str = "-# ✅ 調べ終わりました";
const FOOTER_FAILED: &str = "-# ⚠️ 調査に失敗しました";

fn icon(tool: &str) -> &'static str {
    match tool {
        "web_search" => "🔍",
        "web_fetch" => "📄",
        "web_agent" => "🧭",
        "shell" => "💻",
        _ => "🔧",
    }
}

pub fn progress_line(p: &WorkerProgress) -> String {
    format!("-# {} {} {}", icon(&p.tool), p.tool, p.summary)
        .trim_end()
        .to_string()
}

/// `MAX_CHARS` を超えるときは古い進捗行から省略行に置き換える。Ack 本体と結びの行は削らない
pub fn render(ack: &str, lines: &[String], footer: Option<&str>) -> String {
    let build = |omitted: usize| {
        let mut s = ack.to_string();
        if omitted > 0 {
            s.push_str(&format!("\n-# …({omitted} 件省略)"));
        }
        for l in &lines[omitted..] {
            s.push('\n');
            s.push_str(l);
        }
        if let Some(f) = footer {
            s.push('\n');
            s.push_str(f);
        }
        s
    };
    let mut omitted = 0;
    let mut text = build(0);
    while text.chars().count() > MAX_CHARS && omitted < lines.len() {
        omitted += 1;
        text = build(omitted);
    }
    text
}

/// 進捗表示を動かす。`done` に Worker の成否(完了なら true)を送ると、未反映の進捗と結びの行を最後に 1 回編集して終わる。
/// 編集に失敗したらそこで表示をあきらめる。Worker は progress の送信失敗を無視して続ける
pub fn spawn(
    channel: Arc<dyn Channel>,
    channel_id: ChannelId,
    message_id: MessageId,
    ack: String,
    interval: Duration,
    mut rx: mpsc::UnboundedReceiver<WorkerProgress>,
    mut done: oneshot::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines: Vec<String> = Vec::new();
        let mut pending = false;
        let mut rx_open = true;
        let mut next_edit = Instant::now();
        let edit = |text: String| {
            let channel = channel.clone();
            async move {
                match channel.edit(channel_id, message_id, &text).await {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(error = %e, "Ack を編集できないため、Worker の進捗表示をやめます");
                        false
                    }
                }
            }
        };
        loop {
            tokio::select! {
                biased;
                finished = &mut done => {
                    while let Ok(p) = rx.try_recv() {
                        lines.push(progress_line(&p));
                    }
                    // 送信側が消えたのは Worker 側の task が落ちたとき
                    let footer = if finished.unwrap_or(false) { FOOTER_FINISHED } else { FOOTER_FAILED };
                    edit(render(&ack, &lines, Some(footer))).await;
                    return;
                }
                p = rx.recv(), if rx_open => match p {
                    Some(p) => {
                        lines.push(progress_line(&p));
                        pending = true;
                    }
                    None => rx_open = false,
                },
                _ = tokio::time::sleep_until(next_edit), if pending => {
                    if !edit(render(&ack, &lines, None)).await {
                        return;
                    }
                    pending = false;
                    next_edit = Instant::now() + interval;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(tool: &str, summary: &str) -> WorkerProgress {
        WorkerProgress {
            tool: tool.into(),
            summary: summary.into(),
        }
    }

    #[test]
    fn line_has_icon_per_tool() {
        assert_eq!(
            progress_line(&p("web_search", "東京 天気")),
            "-# 🔍 web_search 東京 天気"
        );
        assert_eq!(progress_line(&p("shell", "ls")), "-# 💻 shell ls");
        assert_eq!(progress_line(&p("other", "x")), "-# 🔧 other x");
    }

    #[test]
    fn render_drops_oldest_lines_but_keeps_ack_and_footer() {
        let ack = "あ".repeat(1_000);
        let lines: Vec<String> = (0..20)
            .map(|i| format!("-# 🔍 web_search {i:0>80}"))
            .collect();
        let text = render(&ack, &lines, Some(FOOTER_FINISHED));
        assert!(text.chars().count() <= MAX_CHARS);
        assert!(text.starts_with(&ack));
        assert!(text.ends_with(FOOTER_FINISHED));
        assert!(text.contains("件省略)"));
        assert!(text.contains(&lines[19]));
        assert!(!text.contains(&lines[0]));

        let short = render("ack", &lines[..2], None);
        assert_eq!(short, format!("ack\n{}\n{}", lines[0], lines[1]));
    }
}
