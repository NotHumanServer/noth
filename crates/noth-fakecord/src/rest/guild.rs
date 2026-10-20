//! guild・channels・roles・members・threads の REST(すべて読み取り)
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::state::{AppState, Channel, Id, Role, World};

/// Discord 互換のエラー応答
fn discord_error(status: StatusCode, code: u32, message: &str) -> Response {
    (status, Json(json!({"code": code, "message": message}))).into_response()
}

/// serenity の Role が読める形に整えた役 JSON
fn role_json(role: &Role) -> Value {
    let mut value = role.to_discord_json();
    if let Some(obj) = value.as_object_mut() {
        obj.insert("guild_id".into(), json!(role.guild_id));
        obj.insert(
            "colors".into(),
            json!({
                "primary_color": role.color,
                "secondary_color": Value::Null,
                "tertiary_color": Value::Null,
            }),
        );
        obj.insert(
            "tags".into(),
            json!({
                "bot_id": Value::Null,
                "integration_id": Value::Null,
                "subscription_listing_id": Value::Null,
            }),
        );
        obj.insert("icon".into(), Value::Null);
        obj.insert("unicode_emoji".into(), Value::Null);
    }
    value
}

/// `GET /api/v10/guilds/{guild_id}` 用。serenity の PartialGuild が読める形
fn partial_guild_json(world: &World, guild: &crate::state::Guild) -> Value {
    let roles: Vec<Value> = world
        .roles
        .values()
        .filter(|r| r.guild_id == guild.id)
        .map(role_json)
        .collect();
    json!({
        "id": guild.id,
        "name": guild.name,
        "icon": Value::Null,
        "icon_hash": Value::Null,
        "splash": Value::Null,
        "discovery_splash": Value::Null,
        "owner_id": guild.owner_id,
        "verification_level": 0,
        "default_message_notifications": 0,
        "explicit_content_filter": 0,
        "roles": roles,
        "emojis": [],
        "features": [],
        "mfa_level": 0,
        "system_channel_flags": 0,
        "premium_tier": 0,
        "preferred_locale": "en-US",
        "nsfw_level": 0,
        "stickers": [],
        "premium_progress_bar_enabled": false,
        "application_id": Value::Null,
        "system_channel_id": Value::Null,
        "rules_channel_id": Value::Null,
        "max_presences": Value::Null,
        "max_members": Value::Null,
        "vanity_url_code": Value::Null,
        "description": Value::Null,
        "banner": Value::Null,
        "premium_subscription_count": Value::Null,
        "public_updates_channel_id": Value::Null,
        "max_video_channel_users": Value::Null,
        "max_stage_video_channel_users": Value::Null,
        "approximate_member_count": Value::Null,
        "approximate_presence_count": Value::Null,
        "welcome_screen": Value::Null,
        "safety_alerts_channel_id": Value::Null,
        "incidents_data": Value::Null,
        "widget_enabled": Value::Null,
        "widget_channel_id": Value::Null,
    })
}

/// `GET /api/v10/guilds/{guild_id}`
pub async fn get_guild(State(state): State<AppState>, Path(guild_id): Path<Id>) -> Response {
    let world = state.world();
    let guild = world.guilds.get(&guild_id).cloned();
    match guild {
        Some(g) => {
            let json = partial_guild_json(&world, &g);
            drop(world);
            (StatusCode::OK, Json(json)).into_response()
        }
        None => {
            drop(world);
            discord_error(StatusCode::NOT_FOUND, 10004, "Unknown Guild")
        }
    }
}

/// `GET /api/v10/guilds/{guild_id}/channels`。スレッドは除く
pub async fn list_guild_channels(
    State(state): State<AppState>,
    Path(guild_id): Path<Id>,
) -> Response {
    let world = state.world();
    if !world.guilds.contains_key(&guild_id) {
        drop(world);
        return discord_error(StatusCode::NOT_FOUND, 10004, "Unknown Guild");
    }
    let channels: Vec<Value> = world
        .channels
        .values()
        .filter(|c| c.guild_id == guild_id && c.kind != crate::state::CHANNEL_PUBLIC_THREAD)
        .map(Channel::to_discord_json)
        .collect();
    drop(world);
    (StatusCode::OK, Json(Value::Array(channels))).into_response()
}

/// `GET /api/v10/guilds/{guild_id}/threads/active`。`{"threads": [...], "members": []}`
pub async fn list_active_threads(
    State(state): State<AppState>,
    Path(guild_id): Path<Id>,
) -> Response {
    let world = state.world();
    if !world.guilds.contains_key(&guild_id) {
        drop(world);
        return discord_error(StatusCode::NOT_FOUND, 10004, "Unknown Guild");
    }
    let threads: Vec<Value> = world
        .channels
        .values()
        .filter(|c| {
            c.guild_id == guild_id && c.kind == crate::state::CHANNEL_PUBLIC_THREAD && !c.archived
        })
        .map(Channel::to_discord_json)
        .collect();
    drop(world);
    (
        StatusCode::OK,
        Json(json!({"threads": threads, "members": []})),
    )
        .into_response()
}

/// `GET /api/v10/channels/{channel_id}`
pub async fn get_channel(State(state): State<AppState>, Path(channel_id): Path<Id>) -> Response {
    let world = state.world();
    match world.channels.get(&channel_id) {
        Some(ch) => {
            let json = ch.to_discord_json();
            drop(world);
            (StatusCode::OK, Json(json)).into_response()
        }
        None => {
            drop(world);
            discord_error(StatusCode::NOT_FOUND, 10003, "Unknown Channel")
        }
    }
}

/// `GET /api/v10/guilds/{guild_id}/roles`
pub async fn list_roles(State(state): State<AppState>, Path(guild_id): Path<Id>) -> Response {
    let world = state.world();
    if !world.guilds.contains_key(&guild_id) {
        drop(world);
        return discord_error(StatusCode::NOT_FOUND, 10004, "Unknown Guild");
    }
    let roles: Vec<Value> = world
        .roles
        .values()
        .filter(|r| r.guild_id == guild_id)
        .map(role_json)
        .collect();
    drop(world);
    (StatusCode::OK, Json(Value::Array(roles))).into_response()
}

/// `GET /api/v10/guilds/{guild_id}/members/{user_id}`
pub async fn get_member(
    State(state): State<AppState>,
    Path((guild_id, user_id)): Path<(Id, Id)>,
) -> Response {
    let world = state.world();
    if !world.guilds.contains_key(&guild_id) {
        drop(world);
        return discord_error(StatusCode::NOT_FOUND, 10004, "Unknown Guild");
    }
    let member = world.member(guild_id, user_id).cloned();
    match member {
        Some(m) => {
            let mut json = world.member_to_discord_json(&m);
            if let Some(obj) = json.as_object_mut() {
                obj.insert("guild_id".into(), json!(guild_id));
            }
            drop(world);
            (StatusCode::OK, Json(json)).into_response()
        }
        None => {
            drop(world);
            discord_error(StatusCode::NOT_FOUND, 10007, "Unknown Member")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, World};
    use axum::extract::{Path, State};
    use serenity::model::channel::GuildChannel;
    use serenity::model::guild::{Member, PartialGuild, Role};
    use tokio;

    fn test_state() -> AppState {
        AppState::new(World::seed())
    }

    fn guild_id(state: &AppState) -> Id {
        state.world().guilds.keys().next().copied().unwrap()
    }

    fn general_channel_id(state: &AppState) -> Id {
        state
            .world()
            .channels
            .values()
            .find(|c| c.name == "general")
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn get_guild_deserializes() {
        let state = test_state();
        let gid = guild_id(&state);
        let resp = get_guild(State(state.clone()), Path(gid)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let _: PartialGuild = serde_json::from_value(v.clone()).unwrap();
        // owner is kurazu, not equal to guild id necessarily; just parseable
        let _ = v["owner_id"].as_str().unwrap().parse::<u64>().unwrap();
    }

    #[tokio::test]
    async fn list_channels_excludes_threads() {
        let state = test_state();
        let gid = guild_id(&state);
        let resp = list_guild_channels(State(state.clone()), Path(gid)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v.as_array().unwrap();
        for c in arr {
            let _: GuildChannel = serde_json::from_value(c.clone()).unwrap();
        }
        // no threads
        assert!(arr.iter().all(|c| c["type"].as_u64().unwrap() != 11));
    }

    #[tokio::test]
    async fn active_threads_shape() {
        let state = test_state();
        let gid = guild_id(&state);
        let resp = list_active_threads(State(state.clone()), Path(gid)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v.get("threads").unwrap().is_array());
        assert!(v.get("members").unwrap().is_array());
        assert_eq!(v["members"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn roles_deserialize() {
        let state = test_state();
        let gid = guild_id(&state);
        let resp = list_roles(State(state.clone()), Path(gid)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v.as_array().unwrap();
        assert!(!arr.is_empty());
        for r in arr {
            let _: Role = serde_json::from_value(r.clone()).unwrap();
        }
    }

    #[tokio::test]
    async fn get_channel_thread_fields() {
        let state = test_state();
        // create a thread manually
        let gid = guild_id(&state);
        let parent = general_channel_id(&state);
        let tid = {
            let mut world = state.world();
            let owner = world.users.keys().next().copied().unwrap();
            let tid = world.alloc_id();
            world.channels.insert(
                tid,
                crate::state::Channel {
                    id: tid,
                    guild_id: gid,
                    kind: crate::state::CHANNEL_PUBLIC_THREAD,
                    name: "t".into(),
                    parent_id: Some(parent),
                    position: 0,
                    topic: None,
                    owner_id: Some(owner),
                    archived: false,
                },
            );
            tid
        };

        let resp = get_channel(State(state.clone()), Path(tid)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let ch: GuildChannel = serde_json::from_value(v).unwrap();
        assert!(ch.thread_metadata.is_some());
        assert!(ch.owner_id.is_some());
        assert!(ch.parent_id.is_some());
    }

    #[tokio::test]
    async fn get_member_deserializes() {
        let state = test_state();
        let gid = guild_id(&state);
        let uid = state.world().users.keys().nth(1).copied().unwrap(); // kurazu
        let resp = get_member(State(state.clone()), Path((gid, uid))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let m: Member = serde_json::from_value(v).unwrap();
        assert!(!m.deaf);
        assert!(!m.mute);
        assert!(!m.user.bot);
    }

    #[tokio::test]
    async fn missing_ids_return_404() {
        let state = test_state();
        let gid = guild_id(&state);
        let resp = get_guild(State(state.clone()), Path(Id(999_999))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], 10004);

        let resp = get_channel(State(state.clone()), Path(Id(999_999))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], 10003);

        let uid = Id(999_999);
        let resp = get_member(State(state.clone()), Path((gid, uid))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], 10007);
    }
}
