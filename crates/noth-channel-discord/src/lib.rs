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
use noth_core::config::url_endpoint;
use noth_core::exclusion::ChannelExclusions;
use noth_core::registry::Registry;
use noth_core::traits::{Channel, ConnectionState};
use noth_core::types::{self, ChannelId, GuildId, MessageId};
use noth_core::{Error, Result};
use serenity::all as sn;
use serenity::client::ClientBuilder;
use serenity::http::{Http, HttpBuilder};
use serenity::prelude::GatewayIntents;

use crate::command::{Controller, InteractionApi, RestInteractionApi, rest_base};
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

/// REST と Gateway の両方で使う Http を作る。`api_base` があるときだけ proxy に向ける。
/// ratelimiter_disabled(true) は必須で、外すと serenity は proxy を無視して本物の Discord に向かう。
/// 環境変数の HTTP_PROXY 経由で loopback への通信が横取りされないよう、reqwest は no_proxy にする
fn build_http(config: &DiscordConfig) -> Http {
    let builder = HttpBuilder::new(&config.token);
    match &config.api_base {
        Some(api_base) => {
            // workspace の reqwest が serenity の解決する版とずれると、HttpBuilder::client に渡せずコンパイルできない
            let client = reqwest::Client::builder()
                .no_proxy()
                .use_rustls_tls()
                .build()
                .expect("Cannot build reqwest::Client");
            builder
                .client(client)
                .proxy(api_base.clone())
                .ratelimiter_disabled(true)
                .build()
        }
        None => builder.build(),
    }
}

/// proxy は REST にしか効かず、Gateway の接続先は `get_gateway()` の応答で決まる。
/// その取得に失敗すると serenity は黙って `wss://gateway.discord.gg` に切り替え、IDENTIFY でトークンが本物の Discord へ送られる。
/// api_base があるときは、Gateway URL が api_base と同じ host と port の ws(s):// でなければ起動を止める
fn ensure_gateway_is_local(api_base: Option<&str>, ws_url: &str) -> Result<()> {
    let Some(api_base) = api_base else {
        return Ok(());
    };
    let local = url_endpoint(api_base);
    let is_ws = ws_url.starts_with("ws://") || ws_url.starts_with("wss://");
    if is_ws && local.is_some() && url_endpoint(ws_url) == local {
        return Ok(());
    }
    tracing::error!(
        api_base,
        ws_url,
        "Gateway の接続先が api_base と一致しません。Bot トークンが別のホストへ送られるため起動を止めます(api_base 側の /api/v10/gateway が失敗している可能性があります)"
    );
    Err(Error::Config(format!(
        "Gateway の接続先 {ws_url} が channel.discord.api_base {api_base} と一致しないため、起動を止めました"
    )))
}

impl DiscordChannel {
    fn new(config: DiscordConfig) -> Self {
        if let Some(api_base) = &config.api_base {
            tracing::warn!(
                "Discord の接続先を {api_base} に置き換えます。Bot トークンはこのホストに送られます"
            );
        }
        let http = Arc::new(build_http(&config));
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
        let api = Arc::new(RestInteractionApi::with_base(
            self.config.token.clone(),
            app.id.get(),
            self.config.guild_id,
            rest_base(self.config.api_base.as_deref()),
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
        // api_base が無いときは `Client::builder(token, intents)` と同じ Http になる
        let mut client = ClientBuilder::new_with_http(build_http(&self.config), intents)
            .event_handler(handler)
            .await
            .map_err(transport)?;
        let ws_url = client.ws_url.lock().await.clone();
        ensure_gateway_is_local(self.config.api_base.as_deref(), &ws_url)?;
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

    fn config_with(api_base: Option<&str>) -> DiscordConfig {
        DiscordConfig {
            token: "secret-token".into(),
            guild_id: 10,
            allowed_users: vec![],
            master: None,
            ignored_channels: vec![],
            api_base: api_base.map(Into::into),
        }
    }

    #[test]
    fn build_http_keeps_the_production_setup_without_api_base() {
        let http = build_http(&config_with(None));
        assert_eq!(http.proxy, None);
        assert!(http.ratelimiter.is_some());
    }

    #[test]
    fn build_http_points_at_api_base_and_disables_the_ratelimiter() {
        let http = build_http(&config_with(Some("http://127.0.0.1:3900")));
        assert_eq!(http.proxy.as_deref(), Some("http://127.0.0.1:3900"));
        assert!(http.ratelimiter.is_none());
    }

    #[test]
    fn gateway_check_passes_without_api_base_or_for_the_same_host_and_port() {
        assert!(ensure_gateway_is_local(None, "wss://gateway.discord.gg").is_ok());
        let base = Some("http://127.0.0.1:3900");
        assert!(ensure_gateway_is_local(base, "ws://127.0.0.1:3900").is_ok());
        assert!(ensure_gateway_is_local(base, "ws://127.0.0.1:3900/gateway").is_ok());
        assert!(
            ensure_gateway_is_local(Some("http://localhost:3900"), "ws://localhost:3900").is_ok()
        );
    }

    #[test]
    fn gateway_check_rejects_any_other_destination() {
        let base = Some("http://127.0.0.1:3900");
        for ws in [
            "wss://gateway.discord.gg",
            "ws://127.0.0.1:3901",
            "ws://localhost:3900",
            "http://127.0.0.1:3900",
            "gateway.discord.gg",
            "",
        ] {
            let r = ensure_gateway_is_local(base, ws);
            assert!(matches!(r, Err(Error::Config(_))), "{ws}");
        }
    }

    /// `/api/v10/gateway` だけ 500 を返し、他は最小限の応答をする偽サーバー。(base, 受けたリクエスト行)
    async fn serve_without_gateway() -> (String, Arc<Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let log = log.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let line = req.lines().next().unwrap_or_default().to_string();
                    log.lock().unwrap().push(line.clone());
                    let (status, body) = if line.contains("/api/v10/gateway") {
                        ("500 Internal Server Error", r#"{"message":"boom"}"#)
                    } else if line.contains("/api/v10/users/@me") {
                        (
                            "200 OK",
                            r#"{"id":"1","username":"bot","discriminator":"0","avatar":null,"bot":true}"#,
                        )
                    } else {
                        ("404 Not Found", r#"{"message":"Unknown"}"#)
                    };
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(reply.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (base, seen)
    }

    #[tokio::test]
    async fn run_refuses_to_start_when_the_gateway_lookup_fails() {
        let (base, seen) = serve_without_gateway().await;
        let channel = DiscordChannel::new(config_with(Some(&base)));
        let (bus, _rx) = noth_core::bus::channel(8);
        // serenity はここで wss://gateway.discord.gg に切り替えるので、IDENTIFY の前に止まらなければならない
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), channel.run(bus))
            .await
            .expect("run() が止まらず、接続しに行った可能性がある");
        let Err(Error::Config(message)) = result else {
            panic!("起動が止まらなかった");
        };
        assert!(message.contains("gateway.discord.gg"), "{message}");
        assert!(!message.contains("secret-token"));
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().any(|l| l.contains("/api/v10/gateway")),
            "{seen:?}"
        );
    }
}
