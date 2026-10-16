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

    /// 除外したチャンネルに残る、Layer 3 に入る前の発言を判定・昇格・state の対象から外す。
    /// 除外を後で外しても戻らない。Layer 3 に入った行は追記だけの方針なので触らない
    pub async fn hide_channel_messages(
        &self,
        guild: GuildId,
        channels: &[ChannelId],
    ) -> Result<u64> {
        // SQLite の変数の上限に余裕を持たせて分ける
        const CHUNK: usize = 500;
        let mut hidden = 0;
        for chunk in channels.chunks(CHUNK) {
            let marks = vec!["?"; chunk.len()].join(", ");
            let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE messages SET hidden = 1
                 WHERE guild_id = ? AND hidden = 0 AND promoted_turn_id IS NULL AND channel_id IN ({marks})"
            )))
            .bind(guild as i64);
            for c in chunk {
                q = q.bind(*c as i64);
            }
            hidden += q.execute(&self.pool).await?.rows_affected();
        }
        Ok(hidden)
    }

    /// 行 ID で指定して外す。キューに残っていた発言が、除外の確定後に届いたとき用
    pub async fn hide_messages(&self, ids: &[i64]) -> Result<()> {
        for id in ids {
            sqlx::query("UPDATE messages SET hidden = 1 WHERE id = ? AND promoted_turn_id IS NULL")
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
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

    #[tokio::test]
    async fn hidden_rows_leave_the_judge_and_promotion_queues() {
        use crate::types::IncomingMessage;
        let s = Store::open_memory().await.unwrap();
        let msg = |message_id: u64, channel_id: u64| IncomingMessage {
            guild_id: 1,
            channel_id,
            channel_name: "c".into(),
            message_id,
            user_id: 5,
            author_name: "a".into(),
            content: "x".into(),
            timestamp: Timestamp::from_millisecond(1_700_000_000_000 + message_id as i64).unwrap(),
            is_self: false,
            mentions_self: false,
            reply_to_self: false,
            reply_to: None,
            bot_marker: None,
        };
        s.insert_message(&msg(1, 10)).await.unwrap();
        let in_other = s.insert_message(&msg(2, 20)).await.unwrap();
        assert_eq!(s.unjudged_messages(1).await.unwrap().len(), 2);

        assert_eq!(s.hide_channel_messages(1, &[10, 30]).await.unwrap(), 1);
        let left: Vec<_> = s.unjudged_messages(1).await.unwrap();
        assert_eq!(
            left.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![in_other]
        );
        assert_eq!(s.recent_messages(1, i64::MAX, 10).await.unwrap().len(), 1);
        assert_eq!(s.hide_channel_messages(1, &[10]).await.unwrap(), 0);

        s.hide_messages(&[in_other]).await.unwrap();
        assert!(s.unjudged_messages(1).await.unwrap().is_empty());
    }
}
