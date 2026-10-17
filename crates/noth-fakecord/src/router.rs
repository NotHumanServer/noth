//! axum の Router。全ルートをここで登録する(ハンドラの本体は各モジュール)
use axum::Router;
use axum::routing::{get, patch, post, put};

use crate::state::AppState;
use crate::{control, gateway, rest};

/// Discord REST 互換のルート。`/api/v10` の下に置く
fn discord_api() -> Router<AppState> {
    Router::new()
        // ユーザー・アプリ・gateway
        .route("/gateway", get(rest::users::get_gateway))
        .route("/gateway/bot", get(rest::users::get_gateway_bot))
        .route("/users/@me", get(rest::users::get_me))
        .route(
            "/oauth2/applications/@me",
            get(rest::users::get_application_me),
        )
        // guild・channels・roles・members・threads
        .route("/guilds/{guild_id}", get(rest::guild::get_guild))
        .route(
            "/guilds/{guild_id}/channels",
            get(rest::guild::list_guild_channels),
        )
        .route(
            "/guilds/{guild_id}/threads/active",
            get(rest::guild::list_active_threads),
        )
        .route("/guilds/{guild_id}/roles", get(rest::guild::list_roles))
        .route(
            "/guilds/{guild_id}/members/{user_id}",
            get(rest::guild::get_member),
        )
        .route("/channels/{channel_id}", get(rest::guild::get_channel))
        // メッセージ・typing・リアクション・pins。
        // `messages/pins` は `messages/{message_id}` より静的セグメントが優先される
        .route(
            "/channels/{channel_id}/messages",
            get(rest::messages::list_messages).post(rest::messages::create_message),
        )
        .route(
            "/channels/{channel_id}/messages/pins",
            get(rest::messages::list_pins),
        )
        .route(
            "/channels/{channel_id}/pins",
            get(rest::messages::list_pins_legacy),
        )
        .route(
            "/channels/{channel_id}/messages/{message_id}",
            get(rest::messages::get_message).patch(rest::messages::edit_message),
        )
        .route(
            "/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me",
            put(rest::messages::add_reaction),
        )
        .route(
            "/channels/{channel_id}/typing",
            post(rest::messages::trigger_typing),
        )
        // interaction・webhook・コマンド登録
        .route(
            "/applications/{application_id}/guilds/{guild_id}/commands",
            put(rest::interactions::register_guild_commands),
        )
        .route(
            "/interactions/{interaction_id}/{token}/callback",
            post(rest::interactions::interaction_callback),
        )
        .route(
            "/webhooks/{application_id}/{token}",
            post(rest::interactions::create_followup),
        )
        .route(
            "/webhooks/{application_id}/{token}/messages/@original",
            patch(rest::interactions::edit_original_response),
        )
}

/// 操作用 API。CLI が叩く
fn fake_api() -> Router<AppState> {
    Router::new()
        .route("/send", post(control::send))
        .route(
            "/channels",
            get(control::list_channels).post(control::create_channel),
        )
        .route(
            "/channels/{channel}",
            patch(control::rename_channel).delete(control::delete_channel),
        )
        .route("/categories", post(control::create_category))
        .route("/threads", post(control::create_thread))
        .route("/thread-list-sync", post(control::thread_list_sync))
        .route("/slash", post(control::slash))
        .route("/click", post(control::click))
        .route("/react", post(control::react))
        .route("/log", get(control::log))
        .route("/wait-reply", post(control::wait_reply))
        .route("/users", get(control::list_users).post(control::add_user))
        .route("/snapshot", post(control::save_snapshot))
        .route("/status", get(control::status))
}

pub fn build(state: AppState) -> Router {
    Router::new()
        .nest("/api/v10", discord_api())
        .nest("/_fake", fake_api())
        // `GET /api/v10/gateway` が返す url。serenity は `?v=10&encoding=json` を付けてくる
        .route("/ws", get(gateway::ws_handler))
        .with_state(state)
}
