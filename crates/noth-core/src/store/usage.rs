use jiff::Timestamp;
use sqlx::Row;

use super::{Store, ts};
use crate::Result;
use crate::types::GuildId;

/// 課金記録の 1 行。キャッシュの読み書きとの対応が崩れないよう、同じ行から読む
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRequest {
    /// キャッシュの読み書きを含むプロンプト全体
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub at: Timestamp,
}

impl Store {
    /// main か Join の直近のリクエスト。`active_log` の usage は書き込みを区別しないので、課金記録から読む
    pub async fn last_prompt_request(&self, guild: GuildId) -> Result<Option<PromptRequest>> {
        let row = sqlx::query(
            "SELECT input_tokens, cache_read_tokens, cache_write_tokens, created_at FROM costs
             WHERE guild_id = ? AND purpose IN ('main', 'join') ORDER BY id DESC LIMIT 1",
        )
        .bind(guild as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| -> Result<PromptRequest> {
            let input = r.try_get::<i64, _>("input_tokens")? as u64;
            let read = r.try_get::<i64, _>("cache_read_tokens")? as u64;
            let write = r.try_get::<i64, _>("cache_write_tokens")? as u64;
            Ok(PromptRequest {
                input_tokens: input + read + write,
                cache_read_tokens: read,
                cache_write_tokens: write,
                at: ts(r.try_get("created_at")?),
            })
        })
        .transpose()
    }

    /// 最後に Layer 3 を Layer 2 へ圧縮した時刻。要約なしで古いターンを捨てた場合は残らない
    pub async fn last_compaction_at(&self, guild: GuildId) -> Result<Option<Timestamp>> {
        let ms: Option<i64> =
            sqlx::query_scalar("SELECT MAX(created_at) FROM digests WHERE guild_id = ?")
                .bind(guild as i64)
                .fetch_one(&self.pool)
                .await?;
        Ok(ms.map(ts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Billing, CostRecord, Purpose};

    fn cost(purpose: Purpose, input: u64, read: u64, write: u64) -> CostRecord {
        CostRecord {
            source: "provider-x".into(),
            model: "m".into(),
            purpose,
            run_id: None,
            input_tokens: input,
            cache_read_tokens: read,
            cache_write_tokens: write,
            output_tokens: 5,
            unit: None,
            amount_usd: None,
            billing: Billing::Metered,
        }
    }

    #[tokio::test]
    async fn last_request_comes_from_one_main_or_join_row() {
        let store = Store::open_memory().await.unwrap();
        assert_eq!(store.last_prompt_request(1).await.unwrap(), None);
        store
            .record_cost(1, &cost(Purpose::Main, 10, 20, 100))
            .await
            .unwrap();
        store
            .record_cost(1, &cost(Purpose::Join, 1, 2, 300))
            .await
            .unwrap();
        store
            .record_cost(1, &cost(Purpose::Worker, 9, 9, 999))
            .await
            .unwrap();
        store
            .record_cost(2, &cost(Purpose::Main, 5, 5, 777))
            .await
            .unwrap();
        let r = store.last_prompt_request(1).await.unwrap().unwrap();
        assert_eq!(
            (r.input_tokens, r.cache_read_tokens, r.cache_write_tokens),
            (303, 2, 300)
        );
        let other = store.last_prompt_request(2).await.unwrap().unwrap();
        assert_eq!(other.cache_write_tokens, 777);
    }

    #[tokio::test]
    async fn compaction_time_is_the_latest_digest() {
        let store = Store::open_memory().await.unwrap();
        assert_eq!(store.last_compaction_at(1).await.unwrap(), None);
        store.insert_digest(1, "a").await.unwrap();
        let at = store.last_compaction_at(1).await.unwrap().unwrap();
        assert!(Timestamp::now().as_second() - at.as_second() < 5);
        assert_eq!(store.last_compaction_at(2).await.unwrap(), None);
    }
}
