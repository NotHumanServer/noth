use sqlx::Row;

use super::Store;
use crate::Result;
use crate::types::GuildId;

impl Store {
    /// main か Join の直近のリクエストがキャッシュに書いたトークン数。
    /// `active_log` の usage は書き込みを区別しないので、課金記録から読む
    pub async fn last_prompt_cache_write(&self, guild: GuildId) -> Result<Option<u64>> {
        let row = sqlx::query(
            "SELECT cache_write_tokens FROM costs
             WHERE guild_id = ? AND purpose IN ('main', 'join') ORDER BY id DESC LIMIT 1",
        )
        .bind(guild as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| Ok(r.try_get::<i64, _>("cache_write_tokens")? as u64))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Billing, CostRecord, Purpose};

    fn cost(purpose: Purpose, cache_write_tokens: u64) -> CostRecord {
        CostRecord {
            source: "provider-x".into(),
            model: "m".into(),
            purpose,
            run_id: None,
            input_tokens: 10,
            cache_read_tokens: 20,
            cache_write_tokens,
            output_tokens: 5,
            unit: None,
            amount_usd: None,
            billing: Billing::Metered,
        }
    }

    #[tokio::test]
    async fn last_cache_write_is_from_the_latest_main_or_join_request() {
        let store = Store::open_memory().await.unwrap();
        assert_eq!(store.last_prompt_cache_write(1).await.unwrap(), None);
        store
            .record_cost(1, &cost(Purpose::Main, 100))
            .await
            .unwrap();
        store
            .record_cost(1, &cost(Purpose::Join, 300))
            .await
            .unwrap();
        store
            .record_cost(1, &cost(Purpose::Worker, 999))
            .await
            .unwrap();
        store
            .record_cost(2, &cost(Purpose::Main, 777))
            .await
            .unwrap();
        assert_eq!(store.last_prompt_cache_write(1).await.unwrap(), Some(300));
        assert_eq!(store.last_prompt_cache_write(2).await.unwrap(), Some(777));
    }
}
