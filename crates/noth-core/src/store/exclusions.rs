use jiff::Timestamp;
use sqlx::Row;

use super::{Store, ts};
use crate::Result;
use crate::types::{ChannelId, GuildId, UserId};

/// コマンドで除外したチャンネルの 1 行
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredChannelRow {
    pub channel_id: ChannelId,
    pub added_by: UserId,
    pub created_at: Timestamp,
}

impl Store {
    pub async fn ignored_channels(&self, guild: GuildId) -> Result<Vec<IgnoredChannelRow>> {
        let rows = sqlx::query(
            "SELECT channel_id, added_by, created_at FROM ignored_channels
             WHERE guild_id = ? ORDER BY created_at, channel_id",
        )
        .bind(guild as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(IgnoredChannelRow {
                    channel_id: r.try_get::<i64, _>("channel_id")? as ChannelId,
                    added_by: r.try_get::<i64, _>("added_by")? as UserId,
                    created_at: ts(r.try_get("created_at")?),
                })
            })
            .collect()
    }

    /// 追加した(まだ無かった)ときに true
    pub async fn add_ignored_channel(
        &self,
        guild: GuildId,
        channel: ChannelId,
        added_by: UserId,
    ) -> Result<bool> {
        let res = sqlx::query(
            "INSERT OR IGNORE INTO ignored_channels (guild_id, channel_id, added_by, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(guild as i64)
        .bind(channel as i64)
        .bind(added_by as i64)
        .bind(Timestamp::now().as_millisecond())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// 外した(あった)ときに true
    pub async fn remove_ignored_channel(&self, guild: GuildId, channel: ChannelId) -> Result<bool> {
        let res = sqlx::query("DELETE FROM ignored_channels WHERE guild_id = ? AND channel_id = ?")
            .bind(guild as i64)
            .bind(channel as i64)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// 保存の失敗を再現するためのテスト専用
    #[cfg(test)]
    pub(crate) async fn drop_ignored_channels_for_test(&self) {
        sqlx::query("DROP TABLE ignored_channels")
            .execute(&self.pool)
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_list_remove_round_trip_per_guild() {
        let s = Store::open_memory().await.unwrap();
        assert!(s.add_ignored_channel(1, 100, 9).await.unwrap());
        assert!(!s.add_ignored_channel(1, 100, 8).await.unwrap());
        assert!(s.add_ignored_channel(2, 100, 9).await.unwrap());
        let rows = s.ignored_channels(1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].channel_id, rows[0].added_by), (100, 9));
        assert!(s.remove_ignored_channel(1, 100).await.unwrap());
        assert!(!s.remove_ignored_channel(1, 100).await.unwrap());
        assert!(s.ignored_channels(1).await.unwrap().is_empty());
        assert_eq!(s.ignored_channels(2).await.unwrap().len(), 1);
    }
}
