//! 受信発言の即時保存とチャンネル単位のデバウンス。context-layers.md §2
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::actor::Job;
use crate::bus::BusEvent;
use crate::config::Config;
use crate::judge::{JudgeBatch, PendingMessage};
use crate::store::Store;
use crate::traits::Channel;
use crate::types::ChannelId;

const DEFAULT_DEBOUNCE_SECS: f64 = 2.0;
/// 発言が debounce 間隔より速く続くと期限が延び続けて判定されないので、最初の発言からこの時間で打ち切る
pub const MAX_WAIT: Duration = Duration::from_secs(10);

pub fn debounce_from_config(config: &Config) -> Duration {
    let section = config.module_section("channel-discord");
    let secs = section
        .get("debounce_secs")
        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
        .filter(|s| s.is_finite() && *s >= 0.0)
        .unwrap_or(DEFAULT_DEBOUNCE_SECS);
    Duration::from_secs_f64(secs)
}

struct Pending {
    first_at: Instant,
    deadline: Instant,
    batch: JudgeBatch,
}

/// Bus が閉じるまで動く。閉じたときは溜まっている分を流してから戻る
pub async fn run(
    store: Store,
    channel: Arc<dyn Channel>,
    mut rx: mpsc::Receiver<BusEvent>,
    debounce: Duration,
    jobs: mpsc::UnboundedSender<Job>,
) {
    let mut pending: HashMap<ChannelId, Pending> = HashMap::new();
    loop {
        let next_deadline = pending.values().map(|p| p.deadline).min();
        tokio::select! {
            event = rx.recv() => match event {
                Some(BusEvent::MessageReceived(msg)) => {
                    let row_id = match store.insert_message(&msg).await {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::warn!(error = %e, message_id = msg.message_id, "発言を保存できませんでした");
                            continue;
                        }
                    };
                    // Noth 自身の発言は router が「直前に答えた」ことを知るためだけに保存する
                    if msg.is_self {
                        continue;
                    }
                    // 必ず応答する発言は、デバウンスと router を待たずに入力中を見せる。
                    // ingest のループを止めないよう送信は待たない
                    if msg.mentions_self || msg.reply_to_self {
                        let channel = channel.clone();
                        let channel_id = msg.channel_id;
                        tokio::spawn(async move {
                            if let Err(e) = channel.typing(channel_id).await {
                                tracing::debug!(error = %e, "typing を送れませんでした");
                            }
                        });
                    }
                    let now = Instant::now();
                    let entry = pending.entry(msg.channel_id).or_insert_with(|| Pending {
                        first_at: now,
                        deadline: now,
                        batch: JudgeBatch { channel_id: msg.channel_id, channel_name: msg.channel_name.clone(), messages: Vec::new() },
                    });
                    entry.deadline = extended_deadline(entry.first_at, now, debounce);
                    entry.batch.messages.push(PendingMessage {
                        row_id,
                        author_name: msg.author_name,
                        content: msg.content,
                        at: msg.timestamp,
                        mentions_self: msg.mentions_self,
                        reply_to_self: msg.reply_to_self,
                    });
                }
                Some(BusEvent::WorkerFinished { .. } | BusEvent::IdleDetected) => {}
                None => {
                    flush(&mut pending, &jobs, true);
                    return;
                }
            },
            _ = tokio::time::sleep_until(next_deadline.unwrap_or_else(Instant::now)), if next_deadline.is_some() => {
                flush(&mut pending, &jobs, false);
            }
        }
    }
}

fn extended_deadline(first_at: Instant, now: Instant, debounce: Duration) -> Instant {
    (now + debounce).min(first_at + MAX_WAIT)
}

/// 期限の早い順に送る。キューに積む順がそのまま Layer 3 への追記順になる
fn flush(pending: &mut HashMap<ChannelId, Pending>, jobs: &mpsc::UnboundedSender<Job>, all: bool) {
    let now = Instant::now();
    let mut due: Vec<ChannelId> = pending
        .iter()
        .filter(|(_, p)| all || p.deadline <= now)
        .map(|(c, _)| *c)
        .collect();
    due.sort_by_key(|c| pending[c].deadline);
    for channel in due {
        if let Some(p) = pending.remove(&channel) {
            let _ = jobs.send(Job::Judge(p.batch));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_is_capped_by_max_wait() {
        let first = Instant::now();
        let debounce = Duration::from_secs(2);

        let early = first + Duration::from_secs(1);
        assert_eq!(extended_deadline(first, early, debounce), early + debounce);

        // 1 秒おきに発言が続いても、最初の発言から MAX_WAIT で期限が止まる
        let late = first + MAX_WAIT - Duration::from_secs(1);
        assert_eq!(extended_deadline(first, late, debounce), first + MAX_WAIT);
        let past = first + MAX_WAIT + Duration::from_secs(5);
        assert_eq!(extended_deadline(first, past, debounce), first + MAX_WAIT);
    }
}
