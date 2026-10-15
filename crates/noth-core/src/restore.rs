//! 再起動時の復元。context-layers.md §6.1
use crate::Result;
use crate::judge::{JudgeBatch, PendingMessage};
use crate::store::Store;
use crate::types::{ChannelId, GuildId};

/// Layer 3 と未昇格バッファは Store が正なので、ここでは Worker の中断と未判定発言の Judge 化だけを行う
pub async fn restore(store: &Store, guild: GuildId) -> Result<Vec<JudgeBatch>> {
    let aborted = store.abort_pending_runs(guild).await?;
    if aborted > 0 {
        tracing::info!(aborted, "再起動前の Worker run を aborted にしました");
    }

    let mut batches: Vec<JudgeBatch> = Vec::new();
    let mut index: std::collections::HashMap<ChannelId, usize> = std::collections::HashMap::new();
    for row in store.unjudged_messages(guild).await? {
        let i = *index.entry(row.channel_id).or_insert_with(|| {
            batches.push(JudgeBatch {
                channel_id: row.channel_id,
                channel_name: row.channel_name.clone(),
                messages: Vec::new(),
            });
            batches.len() - 1
        });
        batches[i].messages.push(PendingMessage {
            row_id: row.id,
            message_id: row.message_id,
            author_name: row.author_name,
            content: row.content,
            at: row.created_at,
            mentions_self: row.mentions_self,
            reply_to_self: row.reply_to_self,
        });
    }
    Ok(batches)
}
