//! メッセージ・typing・リアクション・pins の REST。Bot の操作は `World::record_action` で記録する
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::{AppState, BotActionKind, Id, Message, MessageReference, Reaction};

/// `GET /channels/{id}/messages` のクエリ。before / after / around は排他
#[derive(Debug, Default, Deserialize)]
pub struct ListMessagesQuery {
    pub limit: Option<u32>,
    pub before: Option<Id>,
    pub after: Option<Id>,
    pub around: Option<Id>,
}

/// Value から Id を取り出す(文字列・数値どちらも受ける)
fn value_to_id(value: &Value) -> Option<Id> {
    match value {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().map(Id),
        _ => None,
    }
}

fn discord_error(status: StatusCode, code: u32, message: &str) -> Response {
    (status, Json(json!({"code": code, "message": message}))).into_response()
}

/// anchor を考慮して対象チャンネルのメッセージ ID を新しい順で返す
fn collect_list_ids(
    world: &crate::state::World,
    channel_id: Id,
    query: &ListMessagesQuery,
) -> Vec<Id> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100) as usize;
    let mut ids: Vec<Id> = world
        .messages
        .iter()
        .filter(|(_, m)| m.channel_id == channel_id)
        .map(|(id, _)| *id)
        .collect();
    ids.sort();

    let filtered: Vec<Id> = if let Some(before) = query.before {
        ids.into_iter().filter(|id| *id < before).collect()
    } else if let Some(after) = query.after {
        ids.into_iter().filter(|id| *id > after).collect()
    } else if let Some(around) = query.around {
        let pos = match ids.binary_search(&around) {
            Ok(i) => i,
            Err(i) => i,
        };
        let half = limit / 2;
        let mut start = pos.saturating_sub(half);
        let mut end = (start + limit).min(ids.len());
        if end.saturating_sub(start) < limit && start > 0 {
            start = start.saturating_sub(limit - (end - start));
            end = (start + limit).min(ids.len());
        }
        ids[start..end].to_vec()
    } else {
        ids
    };

    let mut desc = filtered;
    desc.sort_by(|a, b| b.cmp(a));
    desc.truncate(limit);
    desc
}

/// `GET /api/v10/channels/{channel_id}/messages`。新しい順で返す
pub async fn list_messages(
    State(state): State<AppState>,
    Path(channel_id): Path<Id>,
    Query(query): Query<ListMessagesQuery>,
) -> Response {
    let world = state.world();
    if !world.channels.contains_key(&channel_id) {
        return discord_error(StatusCode::NOT_FOUND, 10003, "Unknown Channel");
    }
    let ids = collect_list_ids(&world, channel_id, &query);
    let messages: Vec<Value> = ids
        .iter()
        .filter_map(|id| world.messages.get(id))
        .map(|m| world.message_to_discord_json(m, false))
        .collect();
    drop(world);
    (StatusCode::OK, Json(Value::Array(messages))).into_response()
}

/// `POST /api/v10/channels/{channel_id}/messages`。本文は content / message_reference / components / flags など
pub async fn create_message(
    State(state): State<AppState>,
    Path(channel_id): Path<Id>,
    Json(body): Json<Value>,
) -> Response {
    {
        let world = state.world();
        if !world.channels.contains_key(&channel_id) {
            return discord_error(StatusCode::NOT_FOUND, 10003, "Unknown Channel");
        }
    }

    let content = body
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if content.chars().count() > 2000 {
        return discord_error(StatusCode::BAD_REQUEST, 50035, "Invalid Form Body");
    }

    let components: Vec<Value> = body
        .get("components")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let flags = body.get("flags").and_then(|v| v.as_u64()).unwrap_or(0);

    let message_reference = body.get("message_reference").map(|v| MessageReference {
        message_id: v.get("message_id").and_then(value_to_id),
        channel_id: v.get("channel_id").and_then(value_to_id),
        guild_id: v.get("guild_id").and_then(value_to_id),
    });
    let kind = if message_reference
        .as_ref()
        .and_then(|r| r.message_id)
        .is_some()
    {
        19u8
    } else {
        0u8
    };

    let mut world = state.world();
    let bot_user_id = world.bot_user_id;
    let msg_id = world.alloc_id();
    let timestamp = crate::state::now_rfc3339();
    let reply_to = message_reference.as_ref().and_then(|r| r.message_id);

    world.messages.insert(
        msg_id,
        Message {
            id: msg_id,
            channel_id,
            author_id: bot_user_id,
            content: content.clone(),
            timestamp,
            edited_timestamp: None,
            kind,
            message_reference: message_reference.clone(),
            mentions: Vec::new(),
            components: components.clone(),
            flags,
            reactions: Vec::new(),
            pinned: false,
            interaction_id: None,
        },
    );

    world.record_action(BotActionKind::Post {
        channel_id,
        message_id: msg_id,
        content: content.clone(),
        reply_to,
    });

    let rest_json = world.message_to_discord_json(world.messages.get(&msg_id).unwrap(), false);
    let gateway_json = world.message_to_discord_json(world.messages.get(&msg_id).unwrap(), true);
    drop(world);

    state.publish("MESSAGE_CREATE", gateway_json);
    (StatusCode::OK, Json(rest_json)).into_response()
}

/// `GET /api/v10/channels/{channel_id}/messages/{message_id}`
pub async fn get_message(
    State(state): State<AppState>,
    Path((channel_id, message_id)): Path<(Id, Id)>,
) -> Response {
    let world = state.world();
    if !world.channels.contains_key(&channel_id) {
        return discord_error(StatusCode::NOT_FOUND, 10003, "Unknown Channel");
    }
    let found = world
        .messages
        .get(&message_id)
        .filter(|m| m.channel_id == channel_id)
        .cloned();
    match found {
        Some(m) => {
            let json = world.message_to_discord_json(&m, false);
            drop(world);
            (StatusCode::OK, Json(json)).into_response()
        }
        None => {
            drop(world);
            discord_error(StatusCode::NOT_FOUND, 10008, "Unknown Message")
        }
    }
}

/// `PATCH /api/v10/channels/{channel_id}/messages/{message_id}`
pub async fn edit_message(
    State(state): State<AppState>,
    Path((channel_id, message_id)): Path<(Id, Id)>,
    Json(body): Json<Value>,
) -> Response {
    {
        let world = state.world();
        if !world.channels.contains_key(&channel_id) {
            return discord_error(StatusCode::NOT_FOUND, 10003, "Unknown Channel");
        }
        let ok = world
            .messages
            .get(&message_id)
            .map(|m| m.channel_id == channel_id)
            .unwrap_or(false);
        if !ok {
            return discord_error(StatusCode::NOT_FOUND, 10008, "Unknown Message");
        }
    }

    let content_opt = body
        .get("content")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    if let Some(content) = &content_opt
        && content.chars().count() > 2000
    {
        return discord_error(StatusCode::BAD_REQUEST, 50035, "Invalid Form Body");
    }
    let components_opt = body.get("components").and_then(|v| v.as_array()).cloned();

    let mut world = state.world();
    if let Some(msg) = world.messages.get_mut(&message_id) {
        if let Some(content) = &content_opt {
            msg.content = content.clone();
        }
        if let Some(components) = &components_opt {
            msg.components = components.clone();
        }
        msg.edited_timestamp = Some(crate::state::now_rfc3339());
    }

    world.record_action(BotActionKind::Edit {
        channel_id,
        message_id,
        content: content_opt.clone(),
    });

    let rest_json = world.message_to_discord_json(world.messages.get(&message_id).unwrap(), false);
    let gateway_json =
        world.message_to_discord_json(world.messages.get(&message_id).unwrap(), true);
    drop(world);

    state.publish("MESSAGE_UPDATE", gateway_json);
    (StatusCode::OK, Json(rest_json)).into_response()
}

/// `GET /api/v10/channels/{channel_id}/messages/pins`(新形式)
pub async fn list_pins(State(state): State<AppState>, Path(channel_id): Path<Id>) -> Response {
    let world = state.world();
    if !world.channels.contains_key(&channel_id) {
        return discord_error(StatusCode::NOT_FOUND, 10003, "Unknown Channel");
    }
    let pinned: Vec<Message> = world
        .messages
        .values()
        .filter(|m| m.channel_id == channel_id && m.pinned)
        .cloned()
        .collect();
    let items: Vec<Value> = pinned
        .iter()
        .map(|m| {
            json!({
                "pinned_at": m.timestamp,
                "message": world.message_to_discord_json(m, false),
            })
        })
        .collect();
    drop(world);
    (
        StatusCode::OK,
        Json(json!({"items": items, "has_more": false})),
    )
        .into_response()
}

/// `GET /api/v10/channels/{channel_id}/pins`(旧形式。message の配列)
pub async fn list_pins_legacy(
    State(state): State<AppState>,
    Path(channel_id): Path<Id>,
) -> Response {
    let world = state.world();
    if !world.channels.contains_key(&channel_id) {
        return discord_error(StatusCode::NOT_FOUND, 10003, "Unknown Channel");
    }
    let pinned: Vec<Message> = world
        .messages
        .values()
        .filter(|m| m.channel_id == channel_id && m.pinned)
        .cloned()
        .collect();
    let messages: Vec<Value> = pinned
        .iter()
        .map(|m| world.message_to_discord_json(m, false))
        .collect();
    drop(world);
    (StatusCode::OK, Json(Value::Array(messages))).into_response()
}

/// `PUT /api/v10/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me`
pub async fn add_reaction(
    State(state): State<AppState>,
    Path((channel_id, message_id, emoji)): Path<(Id, Id, String)>,
) -> Response {
    {
        let world = state.world();
        if !world.channels.contains_key(&channel_id) {
            return discord_error(StatusCode::NOT_FOUND, 10003, "Unknown Channel");
        }
        let ok = world
            .messages
            .get(&message_id)
            .map(|m| m.channel_id == channel_id)
            .unwrap_or(false);
        if !ok {
            return discord_error(StatusCode::NOT_FOUND, 10008, "Unknown Message");
        }
    }

    let mut world = state.world();
    let bot_id = world.bot_user_id;
    if let Some(msg) = world.messages.get_mut(&message_id) {
        if let Some(reaction) = msg.reactions.iter_mut().find(|r| r.emoji == emoji) {
            if !reaction.user_ids.contains(&bot_id) {
                reaction.user_ids.push(bot_id);
            }
        } else {
            msg.reactions.push(Reaction {
                emoji: emoji.clone(),
                user_ids: vec![bot_id],
            });
        }
    }
    world.record_action(BotActionKind::Reaction {
        channel_id,
        message_id,
        emoji: emoji.clone(),
    });
    let guild_id = world.channels.get(&channel_id).map(|c| c.guild_id);
    drop(world);

    // 本物の Discord と同じく、Bot 自身のリアクションも Gateway に流れる。形は /_fake/react と同じ
    let (name, emoji_id) = match emoji.split_once(':') {
        Some((name, id)) => (name, Some(id)),
        None => (emoji.as_str(), None),
    };
    state.publish(
        "MESSAGE_REACTION_ADD",
        json!({
            "user_id": bot_id,
            "channel_id": channel_id,
            "message_id": message_id,
            "guild_id": guild_id,
            "emoji": { "id": emoji_id, "name": name },
        }),
    );

    StatusCode::NO_CONTENT.into_response()
}

/// `POST /api/v10/channels/{channel_id}/typing`(204)
pub async fn trigger_typing(State(state): State<AppState>, Path(channel_id): Path<Id>) -> Response {
    let world = state.world();
    if !world.channels.contains_key(&channel_id) {
        return discord_error(StatusCode::NOT_FOUND, 10003, "Unknown Channel");
    }
    drop(world);

    let mut world = state.world();
    world.record_action(BotActionKind::Typing { channel_id });
    drop(world);

    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, BotActionKind, World};
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use serde_json::json;
    use tokio;

    fn test_state() -> AppState {
        AppState::new(World::seed())
    }

    fn general_channel(state: &AppState) -> Id {
        state
            .world()
            .channels
            .values()
            .find(|c| c.name == "general")
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn create_message_records_and_publishes() {
        let state = test_state();
        let channel_id = general_channel(&state);
        let mut rx = state.events.subscribe();

        let resp = create_message(
            State(state.clone()),
            Path(channel_id),
            Json(json!({"content": "hello"})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let world = state.world();
        assert_eq!(world.bot_actions.len(), 1);
        assert!(matches!(
            world.bot_actions[0].kind,
            BotActionKind::Post { .. }
        ));
        drop(world);

        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "MESSAGE_CREATE");
    }

    #[tokio::test]
    async fn edit_message_updates_and_publishes() {
        let state = test_state();
        let channel_id = general_channel(&state);

        create_message(
            State(state.clone()),
            Path(channel_id),
            Json(json!({"content": "before"})),
        )
        .await;

        let msg_id = state.world().messages.values().last().unwrap().id;

        let mut rx = state.events.subscribe();
        let resp = edit_message(
            State(state.clone()),
            Path((channel_id, msg_id)),
            Json(json!({"content": "after", "components": []})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let world = state.world();
        let msg = world.messages.get(&msg_id).unwrap();
        assert_eq!(msg.content, "after");
        assert!(msg.edited_timestamp.is_some());
        assert!(
            world
                .bot_actions
                .iter()
                .any(|a| matches!(a.kind, BotActionKind::Edit { .. }))
        );
        drop(world);

        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "MESSAGE_UPDATE");
    }

    #[tokio::test]
    async fn add_reaction_records() {
        let state = test_state();
        let channel_id = general_channel(&state);

        create_message(
            State(state.clone()),
            Path(channel_id),
            Json(json!({"content": "hi"})),
        )
        .await;
        let msg_id = state.world().messages.values().last().unwrap().id;
        let mut rx = state.events.subscribe();

        let resp = add_reaction(
            State(state.clone()),
            Path((channel_id, msg_id, "👍".to_string())),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "MESSAGE_REACTION_ADD");
        assert_eq!(ev.data["emoji"]["name"], "👍");

        let world = state.world();
        let msg = world.messages.get(&msg_id).unwrap();
        assert!(!msg.reactions.is_empty());
        assert!(
            world
                .bot_actions
                .iter()
                .any(|a| matches!(a.kind, BotActionKind::Reaction { .. }))
        );
    }

    #[tokio::test]
    async fn trigger_typing_records() {
        let state = test_state();
        let channel_id = general_channel(&state);

        let resp = trigger_typing(State(state.clone()), Path(channel_id)).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let world = state.world();
        assert!(
            world
                .bot_actions
                .iter()
                .any(|a| matches!(a.kind, BotActionKind::Typing { .. }))
        );
    }

    #[tokio::test]
    async fn paging_before_after_around() {
        let state = test_state();
        let channel_id = general_channel(&state);

        for i in 0..5 {
            create_message(
                State(state.clone()),
                Path(channel_id),
                Json(json!({"content": format!("m{i}")})),
            )
            .await;
        }

        let world = state.world();
        let ids_sorted: Vec<Id> = world
            .messages
            .values()
            .filter(|m| m.channel_id == channel_id)
            .map(|m| m.id)
            .collect();
        // ensure at least 5
        assert!(ids_sorted.len() >= 5);
        let anchor = ids_sorted[2];
        drop(world);

        // before
        let world = state.world();
        let q = ListMessagesQuery {
            limit: Some(2),
            before: Some(anchor),
            ..Default::default()
        };
        let got = collect_list_ids(&world, channel_id, &q);
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|id| *id < anchor));
        // newer first
        assert!(got[0] > got[1]);
        drop(world);

        // after
        let world = state.world();
        let q = ListMessagesQuery {
            limit: Some(2),
            after: Some(anchor),
            ..Default::default()
        };
        let got = collect_list_ids(&world, channel_id, &q);
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|id| *id > anchor));
        assert!(got[0] > got[1]);
        drop(world);

        // around
        let world = state.world();
        let q = ListMessagesQuery {
            limit: Some(3),
            around: Some(anchor),
            ..Default::default()
        };
        let got = collect_list_ids(&world, channel_id, &q);
        assert_eq!(got.len(), 3);
        assert!(got.contains(&anchor));
        assert!(got[0] > got[1] && got[1] > got[2]);
        drop(world);
    }

    #[tokio::test]
    async fn content_too_long_rejected() {
        let state = test_state();
        let channel_id = general_channel(&state);

        let long = "a".repeat(2001);
        let resp = create_message(
            State(state.clone()),
            Path(channel_id),
            Json(json!({"content": long})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let ok_len = "a".repeat(2000);
        let resp = create_message(
            State(state.clone()),
            Path(channel_id),
            Json(json!({"content": ok_len})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_channel_or_message_returns_404() {
        let state = test_state();
        let channel_id = general_channel(&state);

        let resp = create_message(
            State(state.clone()),
            Path(Id(999_999_999)),
            Json(json!({"content": "x"})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = get_message(State(state.clone()), Path((channel_id, Id(999_999_999)))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = edit_message(
            State(state.clone()),
            Path((channel_id, Id(999_999_999))),
            Json(json!({"content": "x"})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
