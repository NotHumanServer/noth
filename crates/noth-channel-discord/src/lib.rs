mod command;
mod config;
mod exclusion;
mod handler;
mod state;
mod text;

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

use noth_core::async_trait;
use noth_core::bus::BusHandle;
use noth_core::command::CommandHost;
use noth_core::exclusion::ChannelExclusions;
use noth_core::registry::Registry;
use noth_core::traits::{Channel, ConnectionState};
use noth_core::types::{self, ChannelId, GuildId, MessageId};
use noth_core::{Error, Result};
use serenity::all as sn;
use serenity::http::Http;
use serenity::prelude::{Client, GatewayIntents};

use crate::command::{Controller, InteractionApi, RestInteractionApi};
use crate::config::DiscordConfig;
use crate::exclusion::{
    ChannelNode, REST_RETRY_DELAYS, Resolver, lookup_rest, node_of, retry, unknown_ids,
};
use crate::handler::Handler;
use crate::state::{Names, SharedState};
use crate::text::{prepare_edit, split_message, wrap_urls};

const MESSAGE_LIMIT: usize = 2000;

pub fn register(reg: &mut Registry) {
    reg.register_channel("channel-discord", |ctx| {
        let config = DiscordConfig::parse(&ctx.section)?;
        Ok(Arc::new(DiscordChannel::new(config)))
    });
}

pub struct DiscordChannel {
    config: DiscordConfig,
    http: Arc<Http>,
    names: Names,
    state: SharedState,
    exclusions: ChannelExclusions,
    resolver: Arc<Resolver>,
    commands: OnceLock<Arc<CommandHost>>,
}

impl DiscordChannel {
    fn new(config: DiscordConfig) -> Self {
        let http = Arc::new(Http::new(&config.token));
        let exclusions = ChannelExclusions::new(config.guild_id);
        exclusions.set_config(config.ignored_channels.iter().copied());
        Self {
            config,
            http,
            names: Names::default(),
            state: SharedState::new(),
            resolver: Arc::new(Resolver::new(exclusions.clone())),
            exclusions,
            commands: OnceLock::new(),
        }
    }

    fn guild(&self) -> sn::GuildId {
        sn::GuildId::new(self.config.guild_id)
    }

    /// 誤記のまま気づかずに運用しないよう、config の ID が存在しない・別の guild のものなら知らせる。起動は止めない。
    /// 一覧に無いのはアーカイブ済みのスレッドかもしれないので、個別に引いて確かめる
    async fn warn_unknown_ignored_channels(&self, known: &[ChannelNode]) {
        for id in unknown_ids(&self.config.ignored_channels, known) {
            match self.http.get_channel(sn::ChannelId::new(id)).await {
                Ok(sn::Channel::Guild(c)) if c.guild_id == self.guild() => {}
                Ok(_) => tracing::warn!(
                    channel_id = id,
                    "channel.discord.ignored_channels の ID が、この guild のチャンネルではありません"
                ),
                Err(e) => tracing::warn!(
                    channel_id = id,
                    error = %transport(e),
                    "channel.discord.ignored_channels の ID のチャンネルを取得できません。存在しないか、ID の誤りかもしれません"
                ),
            }
        }
    }

    /// `set_commands` が呼ばれていない、またはアプリケーション情報を取れないときは None で、スラッシュコマンドなしで動く
    async fn command_controller(&self) -> Option<Arc<Controller>> {
        let host = self.commands.get()?.clone();
        let app = match self.http.get_current_application_info().await {
            Ok(app) => app,
            Err(e) => {
                tracing::warn!(error = %transport(e), "アプリケーション情報を取得できないため、スラッシュコマンドは使えません");
                return None;
            }
        };
        let api = Arc::new(RestInteractionApi::new(
            self.config.token.clone(),
            app.id.get(),
            self.config.guild_id,
        ));
        Some(
            synced_controller(
                api,
                host,
                self.config.master,
                self.config.guild_id,
                self.exclusions.clone(),
            )
            .await,
        )
    }
}

/// コマンドが 0 件でも sync する。前回登録した `/noth` を Discord 側から消すため
async fn synced_controller(
    api: Arc<dyn InteractionApi>,
    host: Arc<CommandHost>,
    master: Option<u64>,
    guild_id: u64,
    exclusions: ChannelExclusions,
) -> Arc<Controller> {
    let controller = Controller::new(api, host, master, guild_id, exclusions);
    controller.sync().await;
    Arc::new(controller)
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

    fn set_commands(&self, host: Arc<CommandHost>) {
        let _ = self.commands.set(host);
    }

    fn exclusions(&self) -> Option<ChannelExclusions> {
        Some(self.exclusions.clone())
    }

    async fn load_exclusions(&self) {
        let guild = self.guild();
        let listed = async {
            let channels = retry(&REST_RETRY_DELAYS, || self.http.get_channels(guild)).await?;
            let threads = retry(&REST_RETRY_DELAYS, || {
                self.http.get_guild_active_threads(guild)
            })
            .await?;
            Ok::<_, serenity::Error>(
                channels
                    .iter()
                    .chain(threads.threads.iter())
                    .map(node_of)
                    .collect::<Vec<_>>(),
            )
        }
        .await;
        let nodes = match listed {
            Ok(nodes) => nodes,
            Err(e) => {
                tracing::warn!(error = %transport(e), "チャンネルとスレッドの一覧を取得できませんでした。接続後に展開できるまで、再起動前の発言の判定と Layer 3 への昇格を止めます");
                self.exclusions.mark_expansion_pending();
                return;
            }
        };
        self.warn_unknown_ignored_channels(&nodes).await;
        self.resolver
            .rebuild(|| async { Some(nodes) }, |id| lookup_rest(&self.http, id))
            .await;
    }

    async fn run(&self, bus: BusHandle) -> Result<()> {
        let me = self.http.get_current_user().await.map_err(transport)?;
        let commands = self.command_controller().await;
        let handler = Handler {
            bus,
            guild_id: self.config.guild_id,
            allowed: self.config.allowed_users.clone(),
            names: self.names.clone(),
            state: self.state.clone(),
            self_id: me.id.get(),
            self_name: me.display_name().to_string(),
            exclusions: self.exclusions.clone(),
            resolver: self.resolver.clone(),
            watching: AtomicBool::new(false),
            commands,
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
        let text = wrap_urls(text);
        for (i, chunk) in split_message(&text, MESSAGE_LIMIT).into_iter().enumerate() {
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

    async fn edit(&self, channel_id: ChannelId, message_id: MessageId, text: &str) -> Result<()> {
        let msg = sn::EditMessage::new()
            .content(prepare_edit(text))
            .allowed_mentions(sn::CreateAllowedMentions::new());
        sn::ChannelId::new(channel_id)
            .edit_message(&self.http, sn::MessageId::new(message_id), msg)
            .await
            .map(|_| ())
            .map_err(transport)
    }

    async fn react(&self, channel_id: ChannelId, message_id: MessageId, emoji: &str) -> Result<()> {
        // `<:name:id>` 形式のカスタム絵文字も設定で使えるよう、Unicode と決め打ちせず解析する
        let reaction = sn::ReactionType::try_from(emoji)
            .map_err(|e| Error::Other(format!("絵文字 {emoji:?} を解釈できません: {e}")))?;
        sn::ChannelId::new(channel_id)
            .create_reaction(&self.http, sn::MessageId::new(message_id), reaction)
            .await
            .map_err(transport)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_body_strips_status_and_truncates() {
        assert_eq!(clean_body(401, "401: Unauthorized"), "Unauthorized");
        assert_eq!(clean_body(404, "Unknown\nChannel"), "Unknown Channel");
        assert_eq!(clean_body(500, &"x".repeat(300)).chars().count(), 201);
    }

    use std::sync::Mutex;

    use crate::command::ApiError;
    use crate::command::{Incoming, Kind};
    use noth_core::command::{CommandEnv, ContextSnapshot, ContextSource};
    use serde_json::Value;

    #[derive(Default)]
    struct RecordingApi {
        syncs: Mutex<Vec<Value>>,
        callbacks: Mutex<Vec<Value>>,
    }

    #[async_trait]
    impl InteractionApi for RecordingApi {
        async fn sync_commands(&self, body: &Value) -> std::result::Result<(), ApiError> {
            self.syncs.lock().unwrap().push(body.clone());
            Ok(())
        }
        async fn callback(
            &self,
            _id: u64,
            _token: &str,
            body: &Value,
        ) -> std::result::Result<(), ApiError> {
            self.callbacks.lock().unwrap().push(body.clone());
            Ok(())
        }
        async fn edit_original(&self, _: &str, _: &Value) -> std::result::Result<(), ApiError> {
            Ok(())
        }
        async fn followup(&self, _: &str, _: &Value) -> std::result::Result<(), ApiError> {
            Ok(())
        }
    }

    struct NoSnapshot;

    #[async_trait]
    impl ContextSource for NoSnapshot {
        async fn context_snapshot(&self) -> Result<ContextSnapshot> {
            Err(Error::Other("unused".into()))
        }
    }

    #[tokio::test]
    async fn controller_is_built_and_registers_only_ignore_when_no_command_is_enabled() {
        let api = Arc::new(RecordingApi::default());
        let host = Arc::new(CommandHost::new(
            Vec::new(),
            CommandEnv::new(Arc::new(NoSnapshot)),
        ));
        let controller =
            synced_controller(api.clone(), host, Some(1), 10, ChannelExclusions::new(10)).await;
        {
            let syncs = api.syncs.lock().unwrap();
            let options = syncs[0][0]["options"].as_array().unwrap();
            assert_eq!(options.len(), 1);
            assert_eq!(options[0]["name"], "ignore");
        }

        controller
            .handle(Incoming {
                id: 5,
                token: "t".into(),
                user_id: 1,
                guild_id: Some(10),
                kind: Kind::Slash {
                    name: "noth".into(),
                    sub: Some("context".into()),
                },
            })
            .await;
        let callbacks = api.callbacks.lock().unwrap();
        assert_eq!(callbacks.len(), 1);
        assert!(callbacks[0].to_string().contains("未知のコマンドです"));
    }
}
