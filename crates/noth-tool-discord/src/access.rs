//! 依頼した人(`Audience`)に見えるチャンネルだけを通す。権限の計算規則は Discord の
//! "Permission Overwrites" と同じ(https://docs.discord.com/developers/topics/permissions)
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use noth_core::types::Audience;
use serde_json::Value;

use crate::DiscordTool;
use crate::api::{ApiError, describe_error};
use crate::format::id_of;

/// 存在しない・別のサーバー・権限なしを区別させない(チャンネルの存在を探る手がかりにしないため)
pub(crate) const DENIED: &str = "依頼した人にはこのチャンネルを見る権限がありません(存在しない・別のサーバーのチャンネルも同じ表示です)";

const CACHE_TTL: Duration = Duration::from_secs(60);
/// ニックネームは権限と違って古くても害が小さいので、404 や失敗も含めて長く覚える
const NICK_TTL: Duration = Duration::from_secs(300);

pub(crate) enum NickLookup {
    Found(String),
    Missing,
    RateLimited,
}
const ADMINISTRATOR: u64 = 1 << 3;
pub(crate) const VIEW_CHANNEL: u64 = 1 << 10;
const READ_MESSAGE_HISTORY: u64 = 1 << 16;
/// 発言を読むのに要る権限
pub(crate) const READ_MESSAGES: u64 = VIEW_CHANNEL | READ_MESSAGE_HISTORY;

const ANNOUNCEMENT_THREAD: i64 = 10;
const PUBLIC_THREAD: i64 = 11;
const PRIVATE_THREAD: i64 = 12;

pub(crate) struct GuildPerms {
    /// 取れなければ None。ユーザー ID 0 の疑似読者がオーナー扱いにならないようにするため
    owner_id: Option<u64>,
    /// ロール ID → 権限ビット。@everyone のロール ID は guild ID と同じ
    roles: HashMap<u64, u64>,
}

impl GuildPerms {
    pub(crate) fn from_guild(guild: &Value) -> Self {
        let roles = guild
            .get("roles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| Some((r.get("id").and_then(id_of)?, bits(r.get("permissions"), 0))))
            .collect();
        let owner_id = guild.get("owner_id").and_then(id_of);
        Self { owner_id, roles }
    }
}

/// 権限は 10 進数の文字列で届く。読めなかったら `on_error`(allow と基本権限は 0、deny は全ビット)にして、
/// 解釈の失敗で権限が広がらないようにする
fn bits(v: Option<&Value>, on_error: u64) -> u64 {
    match v {
        Some(Value::String(s)) => s.parse().unwrap_or(on_error),
        Some(v) => v.as_u64().unwrap_or(on_error),
        None => 0,
    }
}

/// 依頼者 1 人分の権限。ベースは @everyone と所属ロールの和で、オーナーと ADMINISTRATOR は全権限
pub(crate) fn channel_permissions(
    guild_id: u64,
    guild: &GuildPerms,
    user_id: u64,
    member_roles: &[u64],
    channel: &Value,
) -> u64 {
    if Some(user_id) == guild.owner_id {
        return u64::MAX;
    }
    let mut perms = guild.roles.get(&guild_id).copied().unwrap_or(0);
    for r in member_roles {
        perms |= guild.roles.get(r).copied().unwrap_or(0);
    }
    if perms & ADMINISTRATOR != 0 {
        return u64::MAX;
    }

    let overwrites: Vec<&Value> = channel
        .get("permission_overwrites")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    // type 0 がロール、1 がメンバー
    let of = |kind: u64, id: u64| {
        overwrites.iter().find(|o| {
            o.get("id").and_then(id_of) == Some(id)
                && o.get("type")
                    .and_then(|t| t.as_u64().or_else(|| t.as_str()?.parse().ok()))
                    == Some(kind)
        })
    };
    let apply = |perms: u64, deny: u64, allow: u64| (perms & !deny) | allow;

    if let Some(o) = of(0, guild_id) {
        perms = apply(
            perms,
            bits(o.get("deny"), u64::MAX),
            bits(o.get("allow"), 0),
        );
    }
    let (mut deny, mut allow) = (0, 0);
    for r in member_roles {
        if let Some(o) = of(0, *r) {
            deny |= bits(o.get("deny"), u64::MAX);
            allow |= bits(o.get("allow"), 0);
        }
    }
    perms = apply(perms, deny, allow);
    if let Some(o) = of(1, user_id) {
        perms = apply(
            perms,
            bits(o.get("deny"), u64::MAX),
            bits(o.get("allow"), 0),
        );
    }
    perms
}

/// @everyone だけを持つ疑似読者。ユーザー ID 0 はどのロール・overwrite にも一致しない
const EVERYONE_READER: u64 = 0;

/// 読者全員分のロール情報。`None` は権限を確認できない(依頼者がいない・サーバーのメンバーでない)
pub(crate) struct Readers {
    guild_id: u64,
    guild: Arc<GuildPerms>,
    members: Vec<(u64, Vec<u64>)>,
}

impl Readers {
    /// 全員が `need` を満たすときだけ true
    pub(crate) fn can(&self, channel: &Value, need: u64) -> bool {
        debug_assert!(!self.members.is_empty());
        // 空の `all` は true になってしまうので、依頼者がいないときは必ず拒否する
        !self.members.is_empty()
            && self.members.iter().all(|(user, roles)| {
                channel_permissions(self.guild_id, &self.guild, *user, roles, channel) & need
                    == need
            })
    }
}

impl DiscordTool {
    /// 設定した guild のチャンネルだけを返す。DM や別サーバーのチャンネルは `Err(None)`
    pub(crate) async fn channel_fetch(&self, channel_id: u64) -> Result<Value, Option<ApiError>> {
        if let Some((at, v)) = self.channels.lock().expect("lock").get(&channel_id)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(v.clone());
        }
        let ch = self
            .api
            .get(&format!("/channels/{channel_id}"), &[])
            .await
            .map_err(Some)?;
        if ch.get("guild_id").and_then(id_of) != Some(self.guild_id) {
            return Err(None);
        }
        self.channels
            .lock()
            .expect("lock")
            .insert(channel_id, (Instant::now(), ch.clone()));
        Ok(ch)
    }

    /// 存在しない・見えない・別サーバーは同じ拒否にし、一時的な失敗(429/5xx/通信)だけ区別する
    pub(crate) async fn channel_value(
        &self,
        action: &str,
        channel_id: u64,
    ) -> std::result::Result<Value, String> {
        self.channel_fetch(channel_id).await.map_err(|e| match e {
            Some(e) if !matches!(e.status, 403 | 404) => describe_error(action, &e),
            _ => DENIED.to_string(),
        })
    }

    async fn guild_perms(&self, action: &str) -> std::result::Result<Arc<GuildPerms>, String> {
        if let Some((at, g)) = &*self.guild.lock().expect("lock")
            && at.elapsed() < CACHE_TTL
        {
            return Ok(g.clone());
        }
        let raw = self
            .get(action, &format!("/guilds/{}", self.guild_id), &[])
            .await?;
        let g = Arc::new(GuildPerms::from_guild(&raw));
        *self.guild.lock().expect("lock") = Some((Instant::now(), g.clone()));
        Ok(g)
    }

    /// サーバーのメンバーでなければ None
    async fn member_roles(
        &self,
        action: &str,
        user_id: u64,
    ) -> std::result::Result<Option<Vec<u64>>, String> {
        match self.member_entry(user_id).await {
            Ok(m) => Ok(m.map(|(r, _)| r)),
            Err(e) => Err(crate::api::describe_error(action, &e)),
        }
    }

    /// REST のメッセージ一覧は `member` を付けないので、サーバーでのニックネームはここで補う。
    /// 404 や失敗も覚えて、同じ相手に何度も問い合わせない
    pub(crate) async fn member_nick(&self, user_id: u64) -> NickLookup {
        if let Some((at, n)) = self.nicks.lock().expect("lock").get(&user_id)
            && at.elapsed() < NICK_TTL
        {
            return n.clone().map_or(NickLookup::Missing, NickLookup::Found);
        }
        let nick = match self.member_entry(user_id).await {
            Ok(m) => m.and_then(|(_, nick)| nick),
            Err(e) if e.status == 429 => return NickLookup::RateLimited,
            Err(_) => None,
        };
        self.nicks
            .lock()
            .expect("lock")
            .insert(user_id, (Instant::now(), nick.clone()));
        nick.map_or(NickLookup::Missing, NickLookup::Found)
    }

    async fn member_entry(
        &self,
        user_id: u64,
    ) -> std::result::Result<Option<(Vec<u64>, Option<String>)>, ApiError> {
        if let Some((at, r, n)) = self.members.lock().expect("lock").get(&user_id)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(Some((r.clone(), n.clone())));
        }
        let m = match self
            .api
            .get(&format!("/guilds/{}/members/{user_id}", self.guild_id), &[])
            .await
        {
            Ok(m) => m,
            Err(e) if e.status == 404 => return Ok(None),
            Err(e) => return Err(e),
        };
        let roles: Vec<u64> = m
            .get("roles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(id_of)
            .collect();
        let nick = m
            .get("nick")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        self.members
            .lock()
            .expect("lock")
            .insert(user_id, (Instant::now(), roles.clone(), nick.clone()));
        Ok(Some((roles, nick)))
    }

    /// 出力が届く先は、依頼した人と、トリガーのチャンネルを見られる全員(@everyone が見られるなら @everyone も)。
    /// 出力はトリガーのチャンネルに投稿され Layer 3 にも入るので、その全員が読めるチャンネルだけを許す。
    /// `None` は誰かの権限を確認できない状態で、トリガーのチャンネル以外は読ませない
    pub(crate) async fn readers(
        &self,
        action: &str,
        audience: &Audience,
    ) -> std::result::Result<Option<Readers>, String> {
        // 出力先が分からなければ閲覧者を数えられないので、依頼者だけで判断せずトリガー以外は読ませない
        let Some(dest) = audience.channel_id else {
            return Ok(None);
        };
        if audience.requesters.is_empty() {
            return Ok(None);
        }
        let guild = self.guild_perms(action).await?;
        let mut members: Vec<(u64, Vec<u64>)> = Vec::new();
        for user in &audience.requesters {
            match self.member_roles(action, *user).await? {
                Some(roles) => members.push((*user, roles)),
                None => return Ok(None),
            }
        }
        {
            let ch = self.channel_value(action, dest).await?;
            // 非公開スレッドは閲覧者を調べられないので、そのスレッド以外は読ませない
            let Some(source) = self.permission_source(action, &ch).await? else {
                return Ok(None);
            };
            let mut viewers: Vec<u64> = self.allowed.iter().copied().collect();
            viewers.sort_unstable();
            for user in viewers {
                if members.iter().any(|(u, _)| *u == user) {
                    continue;
                }
                let Some(roles) = self.member_roles(action, user).await? else {
                    continue;
                };
                let perms = channel_permissions(self.guild_id, &guild, user, &roles, &source);
                if perms & VIEW_CHANNEL != 0 {
                    members.push((user, roles));
                }
            }
            let everyone =
                channel_permissions(self.guild_id, &guild, EVERYONE_READER, &[], &source);
            if everyone & VIEW_CHANNEL != 0 {
                members.push((EVERYONE_READER, Vec::new()));
            }
        }
        Ok(Some(Readers {
            guild_id: self.guild_id,
            guild,
            members,
        }))
    }

    /// 権限の計算に使うチャンネル。公開スレッドは親チャンネルの権限に従い、非公開スレッドは対象外
    async fn permission_source(
        &self,
        action: &str,
        channel: &Value,
    ) -> std::result::Result<Option<Value>, String> {
        match channel.get("type").and_then(Value::as_i64) {
            Some(PRIVATE_THREAD) => Ok(None),
            Some(ANNOUNCEMENT_THREAD | PUBLIC_THREAD) => {
                let Some(parent) = channel.get("parent_id").and_then(id_of) else {
                    return Ok(None);
                };
                self.channel_value(action, parent).await.map(Some)
            }
            _ => Ok(Some(channel.clone())),
        }
    }

    /// サーバー内のチャンネルで、依頼した人全員が発言を読めるときだけ返す
    pub(crate) async fn readable_channel(
        &self,
        action: &str,
        channel_id: u64,
        audience: &Audience,
    ) -> std::result::Result<Value, String> {
        let ch = self.channel_value(action, channel_id).await?;
        if audience.channel_id == Some(channel_id) {
            return Ok(ch);
        }
        if self.can_read(action, &ch, audience).await? {
            Ok(ch)
        } else {
            Err(DENIED.into())
        }
    }

    pub(crate) async fn can_read(
        &self,
        action: &str,
        channel: &Value,
        audience: &Audience,
    ) -> std::result::Result<bool, String> {
        if channel.get("id").and_then(id_of) == audience.channel_id && audience.channel_id.is_some()
        {
            return Ok(true);
        }
        let Some(readers) = self.readers(action, audience).await? else {
            return Ok(false);
        };
        let Some(source) = self.permission_source(action, channel).await? else {
            return Ok(false);
        };
        Ok(readers.can(&source, READ_MESSAGES))
    }
}
