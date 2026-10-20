//! fakecord が返す JSON を serenity 0.12.5 の型で deserialize できるかを確かめる互換テスト。
//!
//! 他の担当が未実装のエンドポイントは 501 などを返すため、その分のテストは失敗したままにします。
//! 失敗時には serenity が要求する欠落フィールド名がパニックメッセージに出ます。
//!
//! 必須フィールド一覧(serenity 0.12.5、json.rs 修正材料):
//! - CurrentUser (User): id, name(username), discriminator, global_name, avatar, bot, system,
//!   mfa_enabled, banner, accent_colour(accent_color), locale, verified, email, flags,
//!   premium_type, public_flags, member, primary_guild, avatar_decoration_data, collectibles
//!   (多くは Option だが #[serde(default)] が付いていないものは必須)
//! - CurrentApplicationInfo: id, name, icon, description, rpc_origins, bot_public,
//!   bot_require_code_grant, terms_of_service_url, privacy_policy_url, owner, verify_key, team,
//!   guild_id, primary_sku_id, slug, cover_image, flags, tags, install_params, custom_install_url,
//!   role_connections_verification_url, integration_types_config, approximate_guild_count,
//!   approximate_user_install_count, guild, redirect_uris, interactions_endpoint_url
//! - PartialGuild/Guild: id, name, icon, icon_hash, splash, discovery_splash, owner_id,
//!   afk_metadata, widget_enabled, widget_channel_id, verification_level,
//!   default_message_notifications, explicit_content_filter, roles, emojis, features, mfa_level,
//!   application_id, system_channel_id, system_channel_flags, rules_channel_id, max_presences,
//!   max_members, vanity_url_code, description, banner, premium_tier, premium_subscription_count,
//!   preferred_locale, public_updates_channel_id, max_video_channel_users,
//!   max_stage_video_channel_users, approximate_member_count, approximate_presence_count,
//!   welcome_screen, nsfw_level, stickers, premium_progress_bar_enabled, safety_alerts_channel_id,
//!   incidents_data, joined_at, large, unavailable, member_count, voice_states, members, channels,
//!   threads, presences, stage_instances, scheduled_events
//! - GuildChannel: id, bitrate, parent_id, guild_id(default), kind(type), owner_id, last_message_id,
//!   last_pin_timestamp, name, permission_overwrites(default), position(default), topic, user_limit,
//!   nsfw(default), rate_limit_per_user(default), rtc_region, video_quality_mode, message_count,
//!   member_count, thread_metadata, member, default_auto_archive_duration, permissions, flags(default),
//!   total_message_sent, available_tags(default), applied_tags(default), default_reaction_emoji,
//!   default_thread_rate_limit_per_user, status, default_sort_order
//! - Role: id, guild_id(default), colour(color), colours(colors), hoist, managed, mentionable(default),
//!   name, permissions, position, tags(default), icon, unicode_emoji
//! - Member: user, nick, avatar, banner, roles, joined_at, premium_since, deaf, mute, flags,
//!   pending(default), permissions, guild_id(default), communication_disabled_until,
//!   unusual_dm_activity_until, avatar_decoration_data
//! - Message: id, channel_id, author, content, timestamp, edited_timestamp, tts, mention_everyone,
//!   mentions, mention_roles, mention_channels(default), attachments, embeds, reactions(default),
//!   nonce(default), pinned, webhook_id, kind(type), activity, application, application_id,
//!   message_reference, flags, referenced_message, message_snapshots(default), interaction,
//!   interaction_metadata, thread, components(default+deserialize), sticker_items(default), position,
//!   role_subscription_data, guild_id, member, poll
//! - ThreadsData: threads, members, has_more(default)
//! - Ready (ReadyEvent): version(v), user, guilds, session_id, resume_gateway_url, shard, application
//! - GuildCreateEvent (Guild): 上記 Guild の全フィールド
//! - MessageCreateEvent (Message): 上記 Message の全フィールド
//! - InteractionCreateEvent (Interaction): type, id, application_id, data, guild_id, channel,
//!   channel_id, member, user(default), token, version, app_permissions, locale, guild_locale,
//!   entitlements, authorizing_integration_owners(default), context, attachment_size_limit
//!   (CommandInteraction の場合)。interaction_to_discord_json は attachment_size_limit 等を欠く
//!
//! 注意: json.rs は共有ファイルのため、このテストからは修正しない。

use futures_util::{SinkExt, StreamExt};
use noth_fakecord::{router, state::AppState};
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::net::TcpListener as TokioTcpListener;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

async fn start_server() -> (u16, AppState) {
    let mut world = noth_fakecord::state::World::seed();
    let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    world.listen_port = port;
    let state = AppState::new(world);
    let app = router::build(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (port, state)
}

async fn get_json<T: DeserializeOwned>(port: u16, path: &str, state: &AppState) -> T {
    let _ = state;
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = reqwest::Client::new()
        .get(&url)
        .header("Authorization", "Bot token")
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    serde_json::from_str::<T>(&text)
        .unwrap_or_else(|e| panic!("deserialize {path}: {e}\nstatus: {status}\nbody: {text}"))
}

#[tokio::test]
async fn compat_users_me() {
    let (port, state) = start_server().await;
    let _guild_id = {
        let w = state.world();
        w.primary_guild().unwrap().id
    };
    let _: serenity::model::user::CurrentUser = get_json(port, "/api/v10/users/@me", &state).await;
}

#[tokio::test]
async fn compat_application_me() {
    let (port, state) = start_server().await;
    let _: serenity::model::application::CurrentApplicationInfo =
        get_json(port, "/api/v10/oauth2/applications/@me", &state).await;
}

#[tokio::test]
async fn compat_guild() {
    let (port, state) = start_server().await;
    let guild_id = { state.world().primary_guild().unwrap().id };
    let path = format!("/api/v10/guilds/{guild_id}");
    let _: serenity::model::guild::PartialGuild = get_json(port, &path, &state).await;
}

#[tokio::test]
async fn compat_guild_channels() {
    let (port, state) = start_server().await;
    let guild_id = { state.world().primary_guild().unwrap().id };
    let path = format!("/api/v10/guilds/{guild_id}/channels");
    let _: Vec<serenity::model::channel::GuildChannel> = get_json(port, &path, &state).await;
}

#[tokio::test]
async fn compat_guild_roles() {
    let (port, state) = start_server().await;
    let guild_id = { state.world().primary_guild().unwrap().id };
    let path = format!("/api/v10/guilds/{guild_id}/roles");
    let _: Vec<serenity::model::guild::Role> = get_json(port, &path, &state).await;
}

#[tokio::test]
async fn compat_guild_member() {
    let (port, state) = start_server().await;
    let (guild_id, user_id) = {
        let w = state.world();
        let g = w.primary_guild().unwrap().id;
        let u = w.users.values().find(|u| !u.bot).unwrap().id;
        (g, u)
    };
    let path = format!("/api/v10/guilds/{guild_id}/members/{user_id}");
    let _: serenity::model::guild::Member = get_json(port, &path, &state).await;
}

#[tokio::test]
async fn compat_channel() {
    let (port, state) = start_server().await;
    let channel_id = {
        state
            .world()
            .channels
            .values()
            .find(|c| c.name == "general")
            .unwrap()
            .id
    };
    let path = format!("/api/v10/channels/{channel_id}");
    let _: serenity::model::channel::Channel = get_json(port, &path, &state).await;
}

#[tokio::test]
async fn compat_channel_messages() {
    let (port, state) = start_server().await;
    let channel_id = {
        state
            .world()
            .channels
            .values()
            .find(|c| c.name == "general")
            .unwrap()
            .id
    };
    let path = format!("/api/v10/channels/{channel_id}/messages");
    let _: Vec<serenity::model::channel::Message> = get_json(port, &path, &state).await;
}

#[tokio::test]
async fn compat_active_threads() {
    let (port, state) = start_server().await;
    let guild_id = { state.world().primary_guild().unwrap().id };
    let path = format!("/api/v10/guilds/{guild_id}/threads/active");
    let _: serenity::model::channel::ThreadsData = get_json(port, &path, &state).await;
}

#[tokio::test]
async fn compat_gateway_ready() {
    let (port, _state) = start_server().await;
    let url = format!("ws://127.0.0.1:{port}/ws");
    let (mut ws, _) = connect_async(&url).await.unwrap();
    let hello = ws.next().await.unwrap().unwrap();
    let _ = hello;
    let identify =
        json!({"op": 2, "d": {"token": "x", "intents": 0, "properties": {}, "compress": true}});
    ws.send(WsMessage::Text(identify.to_string().into()))
        .await
        .unwrap();

    // READY
    let ready_msg = ws.next().await.unwrap().unwrap();
    let ready_val: Value = serde_json::from_str(ready_msg.to_text().unwrap()).unwrap();
    assert_eq!(ready_val["t"], "READY");
    let _: serenity::model::event::ReadyEvent = serde_json::from_value(ready_val["d"].clone())
        .unwrap_or_else(|e| panic!("deserialize READY: {e}\nbody: {ready_val}"));
}

#[tokio::test]
async fn compat_gateway_guild_create() {
    let (port, _state) = start_server().await;
    let url = format!("ws://127.0.0.1:{port}/ws");
    let (mut ws, _) = connect_async(&url).await.unwrap();
    let _ = ws.next().await; // hello
    let identify = json!({"op": 2, "d": {"token": "x", "intents": 0, "properties": {}}});
    ws.send(WsMessage::Text(identify.to_string().into()))
        .await
        .unwrap();
    let _ = ws.next().await; // READY
    let gc_msg = ws.next().await.unwrap().unwrap();
    let gc_val: Value = serde_json::from_str(gc_msg.to_text().unwrap()).unwrap();
    assert_eq!(gc_val["t"], "GUILD_CREATE");
    let _: serenity::model::event::GuildCreateEvent = serde_json::from_value(gc_val["d"].clone())
        .unwrap_or_else(|e| panic!("deserialize GUILD_CREATE: {e}\nbody: {gc_val}"));
}

#[tokio::test]
async fn compat_gateway_message_create() {
    let (port, _state) = start_server().await;
    let url = format!("ws://127.0.0.1:{port}/ws");
    let (mut ws, _) = connect_async(&url).await.unwrap();
    let _ = ws.next().await; // hello
    let identify = json!({"op": 2, "d": {"token": "x", "intents": 0, "properties": {}}});
    ws.send(WsMessage::Text(identify.to_string().into()))
        .await
        .unwrap();
    let _ = ws.next().await; // READY
    let _ = ws.next().await; // GUILD_CREATE

    // メッセージを送って MESSAGE_CREATE を発火させる
    let client = Client::new();
    let _ = client
        .post(format!("http://127.0.0.1:{port}/_fake/send"))
        .json(&json!({"channel": "general", "content": "compat msg"}))
        .send()
        .await
        .unwrap();

    let msg = ws.next().await.unwrap().unwrap();
    let val: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(val["t"], "MESSAGE_CREATE");
    let _: serenity::model::event::MessageCreateEvent = serde_json::from_value(val["d"].clone())
        .unwrap_or_else(|e| panic!("deserialize MESSAGE_CREATE: {e}\nbody: {val}"));
}

#[tokio::test]
async fn compat_gateway_interaction_create() {
    let (port, state) = start_server().await;
    let url = format!("ws://127.0.0.1:{port}/ws");
    let (mut ws, _) = connect_async(&url).await.unwrap();
    let _ = ws.next().await; // hello
    let identify = json!({"op": 2, "d": {"token": "x", "intents": 0, "properties": {}}});
    ws.send(WsMessage::Text(identify.to_string().into()))
        .await
        .unwrap();
    let _ = ws.next().await; // READY
    let _ = ws.next().await; // GUILD_CREATE

    // 未登録のコマンドは slash できないので、noth と同じく REST で登録してから呼ぶ
    let (app, guild) = {
        let w = state.world();
        (w.application_id, w.primary_guild().unwrap().id)
    };
    let client = Client::new();
    let registered = client
        .put(format!(
            "http://127.0.0.1:{port}/api/v10/applications/{app}/guilds/{guild}/commands"
        ))
        .header("Authorization", "Bot x")
        .json(&json!([{"name": "noth", "description": "noth", "type": 1}]))
        .send()
        .await
        .unwrap();
    assert!(registered.status().is_success(), "{}", registered.status());
    let res = client
        .post(format!("http://127.0.0.1:{port}/_fake/slash"))
        .json(&json!({"name": "noth", "channel": "general"}))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "slash: {}", res.status());

    // 待ち続けて固まらないよう、届かなければ失敗させる
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("INTERACTION_CREATE が届かない")
        .unwrap()
        .unwrap();
    let val: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(val["t"], "INTERACTION_CREATE");
    let _: serenity::model::event::InteractionCreateEvent =
        serde_json::from_value(val["d"].clone())
            .unwrap_or_else(|e| panic!("deserialize INTERACTION_CREATE: {e}\nbody: {val}"));
}

/// ボタン操作(type 3)の INTERACTION_CREATE。message と component の data を serenity が読めること
#[tokio::test]
async fn compat_gateway_component_interaction_create() {
    let (port, state) = start_server().await;
    let url = format!("ws://127.0.0.1:{port}/ws");
    let (mut ws, _) = connect_async(&url).await.unwrap();
    let _ = ws.next().await; // hello
    let identify = json!({"op": 2, "d": {"token": "x", "intents": 0, "properties": {}}});
    ws.send(WsMessage::Text(identify.to_string().into()))
        .await
        .unwrap();
    let _ = ws.next().await; // READY
    let _ = ws.next().await; // GUILD_CREATE

    // Bot が REST で投稿した、ボタン付きメッセージを押す
    let channel_id = {
        state
            .world()
            .channels
            .values()
            .find(|c| c.name == "general")
            .unwrap()
            .id
    };
    let client = Client::new();
    let _ = client
        .post(format!("http://127.0.0.1:{port}/api/v10/channels/{channel_id}/messages"))
        .json(&json!({
            "content": "btn",
            "components": [{"type": 1, "components": [{"type": 2, "style": 1, "custom_id": "noth:refresh:context", "label": "x"}]}],
        }))
        .send()
        .await
        .unwrap();
    let _ = ws.next().await; // Bot 自身の MESSAGE_CREATE
    let _ = client
        .post(format!("http://127.0.0.1:{port}/_fake/click"))
        .json(&json!({"custom_id": "noth:refresh:context"}))
        .send()
        .await
        .unwrap();

    let msg = ws.next().await.unwrap().unwrap();
    let val: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(val["t"], "INTERACTION_CREATE");
    let ev: serenity::model::event::InteractionCreateEvent =
        serde_json::from_value(val["d"].clone()).unwrap_or_else(|e| {
            panic!("deserialize INTERACTION_CREATE(component): {e}\nbody: {val}")
        });
    assert!(matches!(
        ev.interaction,
        serenity::model::application::Interaction::Component(_)
    ));
}
