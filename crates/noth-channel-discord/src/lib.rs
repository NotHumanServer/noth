use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

use noth_core::async_trait;
use noth_core::bus::{BusEvent, BusHandle};
use noth_core::registry::Registry;
use noth_core::traits::{Channel, ConnectionState};
use noth_core::types::{self, ChannelId, GuildId, IncomingMessage, MessageId, UserId};
use noth_core::{Error, Result};
use serenity::all as sn;
use serenity::http::Http;
use serenity::prelude::{Client, Context, EventHandler, GatewayIntents};

const MESSAGE_LIMIT: usize = 2000;

pub fn register(reg: &mut Registry) {
    reg.register_channel("channel-discord", |ctx| {
        let config = DiscordConfig::parse(&ctx.section)?;
        Ok(Arc::new(DiscordChannel::new(config)))
    });
}

#[derive(Clone, PartialEq)]
struct DiscordConfig {
    token: String,
    guild_id: u64,
    allowed_users: Vec<u64>,
}

// token がログやパニック出力へ漏れないようにする
impl std::fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("token", &"<redacted>")
            .field("guild_id", &self.guild_id)
            .field("allowed_users", &self.allowed_users)
            .finish()
    }
}

impl DiscordConfig {
    fn parse(section: &toml::Value) -> Result<Self> {
        let token = section
            .get("token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| Error::Config("channel.discord.token が必要です".into()))?
            .to_string();
        let guild_id = parse_id(
            section
                .get("guild_id")
                .ok_or_else(|| Error::Config("channel.discord.guild_id が必要です".into()))?,
            "channel.discord.guild_id",
        )?;
        let allowed_users = match section.get("allowed_users") {
            None => Vec::new(),
            Some(toml::Value::Array(items)) => items
                .iter()
                .map(|v| parse_id(v, "channel.discord.allowed_users"))
                .collect::<Result<_>>()?,
            Some(_) => {
                return Err(Error::Config(
                    "channel.discord.allowed_users は文字列の配列で指定してください".into(),
                ));
            }
        };
        Ok(Self {
            token,
            guild_id,
            allowed_users,
        })
    }
}

fn parse_id(value: &toml::Value, key: &str) -> Result<u64> {
    let s = value
        .as_str()
        .ok_or_else(|| Error::Config(format!("{key} は数字の文字列で指定してください")))?;
    s.parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .ok_or_else(|| Error::Config(format!("{key} の ID が不正です: `{s}`")))
}

type Names = Arc<RwLock<HashMap<UserId, String>>>;

/// ConnectionState を EventHandler と Channel の間で共有するための AtomicU8 表現
#[derive(Clone)]
struct SharedState(Arc<AtomicU8>);

impl SharedState {
    fn new() -> Self {
        let s = Self(Arc::new(AtomicU8::new(0)));
        s.set(ConnectionState::Connecting);
        s
    }

    fn set(&self, state: ConnectionState) {
        self.0.store(state as u8, Ordering::Relaxed);
    }

    fn get(&self) -> ConnectionState {
        match self.0.load(Ordering::Relaxed) {
            x if x == ConnectionState::Connected as u8 => ConnectionState::Connected,
            x if x == ConnectionState::Disconnected as u8 => ConnectionState::Disconnected,
            _ => ConnectionState::Connecting,
        }
    }
}

fn from_stage(stage: serenity::gateway::ConnectionStage) -> ConnectionState {
    use serenity::gateway::ConnectionStage as S;
    match stage {
        S::Connected => ConnectionState::Connected,
        S::Disconnected => ConnectionState::Disconnected,
        _ => ConnectionState::Connecting,
    }
}

pub struct DiscordChannel {
    config: DiscordConfig,
    http: Arc<Http>,
    names: Names,
    state: SharedState,
}

impl DiscordChannel {
    fn new(config: DiscordConfig) -> Self {
        let http = Arc::new(Http::new(&config.token));
        Self {
            config,
            http,
            names: Names::default(),
            state: SharedState::new(),
        }
    }

    fn guild(&self) -> sn::GuildId {
        sn::GuildId::new(self.config.guild_id)
    }
}

fn transport(e: serenity::Error) -> Error {
    match e {
        serenity::Error::Http(http) => match http {
            serenity::http::HttpError::UnsuccessfulRequest(res) => {
                let status = res.status_code.as_u16();
                Error::Api {
                    status,
                    body: clean_body(status, &res.error.message),
                }
            }
            other => Error::Transport(other.to_string()),
        },
        other => Error::Transport(other.to_string()),
    }
}

/// Discord は message に "401: Unauthorized" のように status を含めるため、
/// Error::Api の表示で status が二重にならないよう外す
fn clean_body(status: u16, body: &str) -> String {
    const MAX_CHARS: usize = 200;
    let one_line = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = one_line
        .strip_prefix(&format!("{status}:"))
        .map_or(one_line.as_str(), str::trim_start);
    match trimmed.char_indices().nth(MAX_CHARS) {
        Some((i, _)) => format!("{}…", &trimmed[..i]),
        None => trimmed.to_string(),
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    fn guild_id(&self) -> GuildId {
        self.config.guild_id
    }

    async fn run(&self, bus: BusHandle) -> Result<()> {
        let me = self.http.get_current_user().await.map_err(transport)?;
        let handler = Handler {
            bus,
            guild_id: self.config.guild_id,
            allowed: self.config.allowed_users.clone(),
            names: self.names.clone(),
            state: self.state.clone(),
            self_id: me.id.get(),
            self_name: me.display_name().to_string(),
        };
        // GUILD_MEMBERS は特権 intent なので要求しない。表示名は REST で取る
        let intents = GatewayIntents::GUILDS
            | GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;
        let mut client = Client::builder(&self.config.token, intents)
            .event_handler(handler)
            .await
            .map_err(transport)?;
        client.start().await.map_err(transport)
    }

    async fn send(
        &self,
        channel_id: ChannelId,
        text: &str,
        reply_to: Option<MessageId>,
    ) -> Result<MessageId> {
        let channel = sn::ChannelId::new(channel_id);
        let mut last = None;
        for (i, chunk) in split_message(text, MESSAGE_LIMIT).into_iter().enumerate() {
            // メンションによる意図しない通知・権限昇格を防ぐため、常に空を明示する
            let mut msg = sn::CreateMessage::new()
                .content(chunk)
                .allowed_mentions(sn::CreateAllowedMentions::new());
            if let (0, Some(id)) = (i, reply_to) {
                let reference = sn::MessageReference::from((channel, sn::MessageId::new(id)))
                    .fail_if_not_exists(false);
                msg = msg.reference_message(reference);
            }
            match channel.send_message(&self.http, msg).await {
                Ok(sent) => last = Some(sent.id.get()),
                // 一部でも投稿済みなら Err を返すと呼び出し側が全文を再投稿して重複するため、
                // 部分配信として成功扱いにする
                Err(e) if last.is_some() => {
                    tracing::warn!(error = %transport(e), chunk = i, "後続チャンクの送信に失敗したため、送信済み分のみで完了とします");
                    break;
                }
                Err(e) => return Err(transport(e)),
            }
        }
        last.ok_or_else(|| Error::Other("cannot send an empty message".into()))
    }

    async fn typing(&self, channel_id: ChannelId) -> Result<()> {
        sn::ChannelId::new(channel_id)
            .broadcast_typing(&self.http)
            .await
            .map_err(transport)
    }

    async fn members(&self) -> Result<Vec<types::Member>> {
        let roles: HashMap<sn::RoleId, String> = self
            .http
            .get_guild_roles(self.guild())
            .await
            .map_err(transport)?
            .into_iter()
            .map(|r| (r.id, r.name))
            .collect();

        let mut out = Vec::new();
        for &user_id in &self.config.allowed_users {
            match self
                .http
                .get_member(self.guild(), sn::UserId::new(user_id))
                .await
            {
                Ok(m) => out.push(types::Member {
                    user_id,
                    display_name: m.display_name().to_string(),
                    roles: m
                        .roles
                        .iter()
                        .filter_map(|r| roles.get(r).cloned())
                        .collect(),
                }),
                Err(e) => {
                    tracing::warn!(user_id, error = %e, "failed to fetch member; using id as display name");
                    out.push(types::Member {
                        user_id,
                        display_name: user_id.to_string(),
                        roles: Vec::new(),
                    });
                }
            }
        }
        let mut names = self.names.write().unwrap_or_else(|e| e.into_inner());
        names.clear();
        names.extend(out.iter().map(|m| (m.user_id, m.display_name.clone())));
        Ok(out)
    }

    fn connection_state(&self) -> ConnectionState {
        self.state.get()
    }

    async fn health_check(&self) -> Result<String> {
        let me = self.http.get_current_user().await.map_err(transport)?;
        let guild = self.http.get_guild(self.guild()).await.map_err(|e| {
            Error::Other(format!(
                "guild {} に参加していないか取得できません: {}",
                self.config.guild_id,
                transport(e)
            ))
        })?;
        let app = self
            .http
            .get_current_application_info()
            .await
            .map_err(transport)?;
        let flags = app.flags.unwrap_or_default();
        if !(flags.contains(sn::ApplicationFlags::GATEWAY_MESSAGE_CONTENT)
            || flags.contains(sn::ApplicationFlags::GATEWAY_MESSAGE_CONTENT_LIMITED))
        {
            return Err(Error::Other(
                "MESSAGE_CONTENT intent が無効です。Developer Portal の Bot 設定で有効にしてください".into(),
            ));
        }
        Ok(format!(
            "bot {} / guild {} / message_content ok",
            me.name, guild.name
        ))
    }
}

struct Handler {
    bus: BusHandle,
    guild_id: u64,
    allowed: Vec<UserId>,
    names: Names,
    state: SharedState,
    self_id: UserId,
    self_name: String,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, _ready: sn::Ready) {
        self.state.set(ConnectionState::Connected);
    }

    async fn resume(&self, _ctx: Context, _event: sn::ResumedEvent) {
        self.state.set(ConnectionState::Connected);
    }

    async fn shard_stage_update(&self, _ctx: Context, event: sn::ShardStageUpdateEvent) {
        self.state.set(from_stage(event.new));
    }

    async fn message(&self, ctx: Context, msg: sn::Message) {
        if msg.guild_id.map(|g| g.get()) != Some(self.guild_id) {
            return;
        }
        if !matches!(
            msg.kind,
            sn::MessageType::Regular | sn::MessageType::InlineReply
        ) {
            return;
        }
        let author_id = msg.author.id.get();
        let is_self = author_id == self.self_id;
        if !is_self
            && (msg.author.bot || msg.webhook_id.is_some() || !self.allowed.contains(&author_id))
        {
            return;
        }

        let mut lookup: HashMap<UserId, String> = msg
            .mentions
            .iter()
            .map(|u| (u.id.get(), u.name.clone()))
            .collect();
        lookup.extend(self.names.read().unwrap_or_else(|e| e.into_inner()).clone());

        let content = replace_mentions(
            &msg.content,
            self.self_id,
            &self.self_name,
            &self.allowed,
            &lookup,
        );
        if content.trim().is_empty() {
            return;
        }
        let author_name = msg
            .member
            .as_ref()
            .and_then(|m| m.nick.clone())
            .unwrap_or_else(|| msg.author.display_name().to_string());
        let channel_name = ctx
            .cache
            .guild(self.guild_id)
            .and_then(|g| {
                // スレッドは channels ではなく threads に入っている
                g.channels
                    .get(&msg.channel_id)
                    .or_else(|| g.threads.iter().find(|t| t.id == msg.channel_id))
                    .map(|c| c.name.clone())
            })
            .unwrap_or_else(|| msg.channel_id.get().to_string());

        let incoming = IncomingMessage {
            guild_id: self.guild_id,
            channel_id: msg.channel_id.get(),
            channel_name,
            message_id: msg.id.get(),
            user_id: author_id,
            author_name,
            content,
            timestamp: jiff::Timestamp::from_nanosecond(msg.timestamp.unix_timestamp_nanos())
                .unwrap_or_else(|_| jiff::Timestamp::now()),
            is_self,
            mentions_self: msg.mentions.iter().any(|u| u.id.get() == self.self_id),
            reply_to_self: msg
                .referenced_message
                .as_ref()
                .is_some_and(|r| r.author.id.get() == self.self_id),
        };
        if let Err(e) = self.bus.publish(BusEvent::MessageReceived(incoming)).await {
            tracing::error!(error = %e, "failed to publish message");
        }
    }
}

fn replace_mentions(
    content: &str,
    self_id: UserId,
    self_name: &str,
    allowed: &[UserId],
    names: &HashMap<UserId, String>,
) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("<@") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let digits_from = after.strip_prefix('!').unwrap_or(after);
        let digits_len = digits_from.bytes().take_while(u8::is_ascii_digit).count();
        let id = digits_from[..digits_len].parse::<u64>().ok();
        match (id, digits_from[digits_len..].strip_prefix('>')) {
            (Some(id), Some(tail)) => {
                if id == self_id {
                    out.push('@');
                    out.push_str(self_name);
                } else if allowed.contains(&id) {
                    out.push_str(names.get(&id).map_or("<user>", String::as_str));
                } else {
                    out.push_str("<user>");
                }
                rest = tail;
            }
            _ => {
                out.push_str("<@");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn split_message(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.chars().count() <= limit {
            chunks.push(rest.to_string());
            break;
        }
        let window_end = rest
            .char_indices()
            .nth(limit)
            .map_or(rest.len(), |(i, _)| i);
        let window = &rest[..window_end];
        match window.rfind('\n').filter(|&i| i > 0) {
            Some(i) => {
                chunks.push(rest[..i].to_string());
                rest = &rest[i + 1..];
            }
            None => {
                chunks.push(window.to_string());
                rest = &rest[window_end..];
            }
        }
    }
    chunks.retain(|c| !c.trim().is_empty());
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> HashMap<UserId, String> {
        HashMap::from([(1, "alice".to_string()), (3, "carol".to_string())])
    }

    fn replace(s: &str) -> String {
        replace_mentions(s, 99, "Noth", &[1, 2], &names())
    }

    #[test]
    fn mention_allowed_user_uses_display_name() {
        assert_eq!(replace("hi <@1> and <@!1>"), "hi alice and alice");
    }

    #[test]
    fn mention_allowed_user_without_name_is_masked() {
        assert_eq!(replace("<@2>"), "<user>");
    }

    #[test]
    fn mention_unknown_user_is_masked_even_if_name_known() {
        assert_eq!(replace("<@3>"), "<user>");
    }

    #[test]
    fn mention_self_becomes_at_name() {
        assert_eq!(replace("<@99> help"), "@Noth help");
    }

    #[test]
    fn role_everyone_and_malformed_are_kept() {
        assert_eq!(
            replace("<@&5> @everyone <@abc> <@1 <@"),
            "<@&5> @everyone <@abc> <@1 <@"
        );
    }

    #[test]
    fn split_short_is_single() {
        assert_eq!(split_message("abc", 2000), vec!["abc"]);
        assert!(split_message("", 2000).is_empty());
    }

    #[test]
    fn split_prefers_newline() {
        let text = format!("{}\n{}", "a".repeat(10), "b".repeat(10));
        assert_eq!(
            split_message(&text, 15),
            vec!["a".repeat(10), "b".repeat(10)]
        );
    }

    #[test]
    fn split_drops_whitespace_only_chunks() {
        let text = format!("{}\n{}", "a".repeat(10), " ".repeat(10));
        assert_eq!(split_message(&text, 15), vec!["a".repeat(10)]);
        assert!(split_message(" \n\t", 2000).is_empty());
    }

    #[test]
    fn debug_redacts_token() {
        let c = DiscordConfig {
            token: "secret".into(),
            guild_id: 1,
            allowed_users: vec![],
        };
        assert!(!format!("{c:?}").contains("secret"));
    }

    #[test]
    fn clean_body_strips_status_and_truncates() {
        assert_eq!(clean_body(401, "401: Unauthorized"), "Unauthorized");
        assert_eq!(clean_body(404, "Unknown\nChannel"), "Unknown Channel");
        assert_eq!(clean_body(500, &"x".repeat(300)).chars().count(), 201);
    }

    #[test]
    fn split_hard_cuts_multibyte_by_char() {
        let text = "あ".repeat(4500);
        let chunks = split_message(&text, MESSAGE_LIMIT);
        assert_eq!(
            chunks.iter().map(|c| c.chars().count()).collect::<Vec<_>>(),
            vec![2000, 2000, 500]
        );
        assert_eq!(chunks.concat(), text);
    }

    fn section(s: &str) -> toml::Value {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn config_ok() {
        let c = DiscordConfig::parse(&section(
            r#"token = "t"
guild_id = "123"
allowed_users = ["1", "2"]"#,
        ))
        .unwrap();
        assert_eq!(
            c,
            DiscordConfig {
                token: "t".into(),
                guild_id: 123,
                allowed_users: vec![1, 2]
            }
        );
    }

    #[test]
    fn config_errors() {
        for bad in [
            r#"guild_id = "1""#,
            r#"token = "" 
guild_id = "1""#,
            r#"token = "t""#,
            r#"token = "t"
guild_id = 1"#,
            r#"token = "t"
guild_id = "x""#,
            r#"token = "t"
guild_id = "1"
allowed_users = ["1", 2]"#,
            r#"token = "t"
guild_id = "1"
allowed_users = "1""#,
        ] {
            assert!(
                matches!(DiscordConfig::parse(&section(bad)), Err(Error::Config(_))),
                "{bad}"
            );
        }
    }
}
