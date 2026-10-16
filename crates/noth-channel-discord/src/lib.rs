mod command;
mod config;
mod handler;
mod state;
mod text;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use noth_core::async_trait;
use noth_core::bus::BusHandle;
use noth_core::command::CommandHost;
use noth_core::registry::Registry;
use noth_core::traits::{Channel, ConnectionState};
use noth_core::types::{self, ChannelId, GuildId, MessageId};
use noth_core::{Error, Result};
use serenity::all as sn;
use serenity::http::Http;
use serenity::prelude::{Client, GatewayIntents};

use crate::command::{Controller, RestInteractionApi};
use crate::config::DiscordConfig;
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
    commands: OnceLock<Arc<CommandHost>>,
}

impl DiscordChannel {
    fn new(config: DiscordConfig) -> Self {
        let http = Arc::new(Http::new(&config.token));
        Self {
            config,
            http,
            names: Names::default(),
            state: SharedState::new(),
            commands: OnceLock::new(),
        }
    }

    fn guild(&self) -> sn::GuildId {
        sn::GuildId::new(self.config.guild_id)
    }

    /// コマンドが 0 件でも作る(前回登録したコマンドを外すため)。`set_commands` が呼ばれていない、またはアプリケーション情報を取れないときは None で、スラッシュコマンドなしで動く
    async fn command_controller(&self) -> Option<Arc<Controller>> {
        let host = self
            .commands
            .get()
            .filter(|h| !h.commands().is_empty())?
            .clone();
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
        let controller = Controller::new(api, host, self.config.master, self.config.guild_id);
        controller.sync().await;
        Some(Arc::new(controller))
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

    fn set_commands(&self, host: Arc<CommandHost>) {
        let _ = self.commands.set(host);
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
}
