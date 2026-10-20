//! tool が記録済みの発言を読み取り専用で引くための窓口。実体は `Store`
use std::fmt::Debug;

use async_trait::async_trait;
use jiff::Timestamp;

use crate::Result;
use crate::store::MessageRow;
use crate::types::{ChannelId, GuildId};

#[derive(Debug, Clone)]
pub struct HistoryQuery {
    pub guild_id: GuildId,
    pub channel_id: Option<ChannelId>,
    /// `messages.channel_name` との完全一致。ASCII のみ大文字小文字を区別しない(非 ASCII は同じ表記で一致)
    pub channel_name: Option<String>,
    /// content の部分一致。ASCII のみ大文字小文字を区別しない(非 ASCII は同じ表記で一致)
    pub text: Option<String>,
    /// author_name の部分一致。ASCII のみ大文字小文字を区別しない(非 ASCII は同じ表記で一致)
    pub author: Option<String>,
    pub since: Option<Timestamp>,
    pub until: Option<Timestamp>,
    /// 除外したチャンネルの ID。カテゴリや親チャンネルの配下は Store から分からないので、呼び出し側が行ごとに確かめる
    pub exclude_channels: Vec<ChannelId>,
    pub limit: usize,
}

#[async_trait]
pub trait MessageHistory: Send + Sync + Debug {
    /// 新しい順。記録されるのは `allowed_users` と Noth の発言だけなので、返る行に追加のマスクは要らない
    async fn search(&self, q: &HistoryQuery) -> Result<Vec<MessageRow>>;
}
