//! 管理コマンド `context` のための読み取り専用の問い合わせ。Layer の状態は変えない
use std::collections::HashSet;
use std::time::Duration;

use super::{Actor, Job};
use crate::command::{ContextSnapshot, ContextSource, LayerSize, RequestUsage, estimate_tokens};
use crate::layers::HEADER_EPISODE;
use crate::store::RunStatus;
use crate::{Error, Result, async_trait};

/// 長い provider 呼び出しの後ろに並ぶことがあるので余裕を持たせる。Discord の応答トークンは 15 分有効
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// actor の仕事のキューに問い合わせを積む窓口
pub(super) struct JobQueue(pub(super) tokio::sync::mpsc::UnboundedSender<Job>);

#[async_trait]
impl ContextSource for JobQueue {
    async fn context_snapshot(&self) -> Result<ContextSnapshot> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.0
            .send(Job::Snapshot(tx))
            .map_err(|_| Error::Other("actor が停止しています".into()))?;
        tokio::time::timeout(REPLY_TIMEOUT, rx)
            .await
            .map_err(|_| Error::Other("actor が混み合っていて、文脈を取得できませんでした".into()))?
            .map_err(|_| Error::Other("actor が問い合わせに答えずに停止しました".into()))?
    }
}

fn layer(name: &'static str, text: &str) -> LayerSize {
    LayerSize {
        name,
        chars: text.chars().count(),
        est_tokens: estimate_tokens(text),
    }
}

impl Actor {
    pub(super) async fn context_snapshot(&self) -> Result<ContextSnapshot> {
        let turns = self.store.active_turns(self.guild).await?;
        let mut chunk_ids: Vec<i64> = turns.iter().filter_map(|t| t.chunk_id).collect();
        chunk_ids.sort_unstable();
        chunk_ids.dedup();
        let mut channels = HashSet::new();
        for id in chunk_ids {
            if let Some(chunk) = self.store.chunk(id).await? {
                channels.insert(chunk.channel_id);
            }
        }
        let log: String = turns.iter().map(|t| t.text.as_str()).collect();
        // main と Worker に同じ定義を渡しているので、シリアライズした長さをその大きさとみなす
        let tools = serde_json::to_string(&self.tools).unwrap_or_default();

        let last_request = match self.store.last_usage(self.guild).await? {
            Some(usage) => Some(RequestUsage {
                input_tokens: usage.input_tokens,
                cache_read_tokens: usage.cached_tokens,
                cache_write_tokens: self.store.last_prompt_cache_write(self.guild).await?,
            }),
            None => None,
        };
        let runs = self.store.active_runs(self.guild).await?;
        let count = |status| runs.iter().filter(|r| r.status == status).count();

        Ok(ContextSnapshot {
            model: self.provider.label(),
            layers: vec![
                layer("tools", &tools),
                layer("Layer 0", &self.harness_rules),
                layer("Layer 1", &self.profile),
                layer("Layer 1.5", &self.memory),
                layer("Layer 2", &self.digest),
                layer("Layer 3", &log),
            ],
            last_request,
            hard_limit_tokens: self.config.context.prompt_hard_limit_tokens,
            turns: turns.len(),
            turn_channels: channels.len(),
            episodes: self
                .digest
                .lines()
                .filter(|l| l.starts_with(HEADER_EPISODE))
                .count(),
            workers_running: count(RunStatus::Running),
            workers_queued: count(RunStatus::Queued),
        })
    }
}
