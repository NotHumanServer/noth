//! interaction の応答・webhook・コマンド登録の REST
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::{AppState, BotActionKind, Id, InteractionState, Message};

/// interaction の応答が作るメッセージの type。スラッシュコマンドへの応答は 20(CHAT_INPUT_COMMAND)、
/// それ以外(コンポーネント操作など)は 0
fn response_message_kind(interaction_kind: u8) -> u8 {
    if interaction_kind == crate::state::INTERACTION_COMMAND {
        20
    } else {
        0
    }
}

/// `PUT /api/v10/applications/{application_id}/guilds/{guild_id}/commands`。
/// 本文はコマンド定義の配列。登録結果(id 付き)の配列を返す
pub async fn register_guild_commands(
    State(state): State<AppState>,
    Path((application_id, guild_id)): Path<(Id, Id)>,
    Json(body): Json<Vec<Value>>,
) -> Json<Value> {
    let mut results: Vec<Value> = Vec::new();
    {
        let mut w = state.world();
        // 上書き登録: 既存の同 guild のコマンドを捨てる
        w.commands.retain(|c| c.guild_id != Some(guild_id));
        for def in body {
            let name = def
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let id = w.alloc_id();
            w.commands.push(crate::state::RegisteredCommand {
                id,
                application_id,
                guild_id: Some(guild_id),
                name: name.clone(),
                definition: def.clone(),
            });
            // Discord 形に id / application_id / guild_id を足して返す。
            // serenity の Command は version と type が必須で、description と
            // default_member_permissions もキーが無いと読めないので、欠けていれば補う
            let mut obj = def.clone();
            if let Some(map) = obj.as_object_mut() {
                map.insert("id".into(), json!(id));
                map.insert("application_id".into(), json!(application_id));
                map.insert("guild_id".into(), json!(guild_id));
                map.insert("version".into(), json!(id));
                map.entry("type").or_insert(json!(1));
                map.entry("description").or_insert(json!(""));
                map.entry("default_member_permissions")
                    .or_insert(Value::Null);
            }
            results.push(obj);
        }
    }
    Json(Value::Array(results))
}

/// `POST /api/v10/interactions/{interaction_id}/{token}/callback`。type 4/5/6/7 を扱い、204 を返す
pub async fn interaction_callback(
    State(state): State<AppState>,
    Path((interaction_id, token)): Path<(Id, String)>,
    Json(body): Json<Value>,
) -> Result<StatusCode, (StatusCode, Json<Value>)> {
    // 事前検証
    {
        let w = state.world();
        match w.interactions.get(&interaction_id) {
            Some(i) if i.token == token => {}
            _ => {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(json!({"code": 10062, "message": "Unknown interaction"})),
                ));
            }
        }
        let i = w.interactions.get(&interaction_id).unwrap();
        if i.state != InteractionState::Pending {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"code": 40060, "message": "Interaction has already been acknowledged"}),
                ),
            ));
        }
    }

    let rtype = body.get("type").and_then(Value::as_u64).unwrap_or(0) as u8;
    if !matches!(rtype, 4..=7) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"code": 40001, "message": "Invalid interaction type"})),
        ));
    }
    let data = body.get("data").cloned().unwrap_or_else(|| json!({}));

    let flags = data.get("flags").and_then(Value::as_u64).unwrap_or(0);
    let ephemeral = flags & crate::state::MESSAGE_FLAG_EPHEMERAL != 0;
    let content = data
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let components: Vec<Value> = data
        .get("components")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // interaction の基本情報
    let (channel_id, app_id, clicked_message, response_kind) = {
        let w = state.world();
        let i = w.interactions.get(&interaction_id).unwrap();
        (
            i.channel_id,
            i.application_id,
            i.message_id,
            response_message_kind(i.kind),
        )
    };

    match rtype {
        4 | 5 => {
            let msg_id = {
                let mut w = state.world();
                let id = w.alloc_id();
                let msg = Message {
                    id,
                    channel_id,
                    author_id: app_id,
                    content: content.clone(),
                    timestamp: crate::state::now_rfc3339(),
                    edited_timestamp: None,
                    kind: response_kind,
                    message_reference: None,
                    mentions: Vec::new(),
                    components: components.clone(),
                    flags,
                    reactions: Vec::new(),
                    pinned: false,
                    interaction_id: Some(interaction_id),
                };
                w.messages.insert(id, msg.clone());
                if !ephemeral {
                    let gateway_json = w.message_to_discord_json(&msg, true);
                    state.publish("MESSAGE_CREATE", gateway_json);
                }
                id
            };
            // interaction を更新
            {
                let mut w = state.world();
                if let Some(i) = w.interactions.get_mut(&interaction_id) {
                    i.original_message_id = Some(msg_id);
                    i.state = if rtype == 5 {
                        InteractionState::Deferred
                    } else {
                        InteractionState::Responded
                    };
                }
                w.record_action(BotActionKind::InteractionResponse {
                    interaction_id,
                    response_type: rtype,
                    message_id: Some(msg_id),
                    content: Some(content.clone()),
                });
            }
        }
        6 => {
            let mut w = state.world();
            if let Some(i) = w.interactions.get_mut(&interaction_id) {
                i.state = InteractionState::Deferred;
            }
            w.record_action(BotActionKind::InteractionResponse {
                interaction_id,
                response_type: rtype,
                message_id: None,
                content: None,
            });
        }
        7 => {
            // 押されたメッセージを更新する
            if let Some(mid) = clicked_message {
                {
                    let mut w = state.world();
                    if let Some(msg) = w.messages.get_mut(&mid) {
                        if let Some(c) = data.get("content").and_then(Value::as_str) {
                            msg.content = c.to_string();
                        }
                        if data.get("components").is_some() {
                            msg.components = components.clone();
                        }
                        if let Some(f) = data.get("flags").and_then(Value::as_u64) {
                            msg.flags = f;
                        }
                        msg.edited_timestamp = Some(crate::state::now_rfc3339());
                        let msg_clone = msg.clone();
                        let msg_ephemeral = msg.flags & crate::state::MESSAGE_FLAG_EPHEMERAL != 0;
                        if !msg_ephemeral {
                            let gateway_json = w.message_to_discord_json(&msg_clone, true);
                            state.publish("MESSAGE_UPDATE", gateway_json);
                        }
                    }
                }
                {
                    let mut w = state.world();
                    if let Some(i) = w.interactions.get_mut(&interaction_id) {
                        i.state = InteractionState::Responded;
                    }
                    w.record_action(BotActionKind::InteractionResponse {
                        interaction_id,
                        response_type: rtype,
                        message_id: Some(mid),
                        content: Some(content.clone()),
                    });
                }
            } else {
                let mut w = state.world();
                if let Some(i) = w.interactions.get_mut(&interaction_id) {
                    i.state = InteractionState::Responded;
                }
                w.record_action(BotActionKind::InteractionResponse {
                    interaction_id,
                    response_type: rtype,
                    message_id: None,
                    content: Some(content.clone()),
                });
            }
        }
        _ => {}
    }

    Ok(StatusCode::NO_CONTENT)
}

/// `PATCH /api/v10/webhooks/{application_id}/{token}/messages/@original`
pub async fn edit_original_response(
    State(state): State<AppState>,
    Path((_application_id, token)): Path<(Id, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // interaction を探す
    let interaction_id = {
        let w = state.world();
        let mut found = None;
        for (id, i) in &w.interactions {
            if i.token == token {
                found = Some(*id);
                break;
            }
        }
        found
    };
    let interaction_id = match interaction_id {
        Some(id) => id,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"code": 10062, "message": "Unknown interaction"})),
            ));
        }
    };

    // コンポーネント操作を type 6(遅延更新)で受けた場合は、@original は押されたメッセージを指す
    let original_message_id = {
        let w = state.world();
        w.interactions
            .get(&interaction_id)
            .and_then(|i| i.original_message_id.or(i.message_id))
    };
    let mid = match original_message_id {
        Some(m) => m,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"code": 10062, "message": "Original response not found"})),
            ));
        }
    };

    let (rest_json, _ephemeral) = {
        let mut w = state.world();
        // メッセージを編集
        let msg_clone = {
            let msg = match w.messages.get_mut(&mid) {
                Some(m) => m,
                None => {
                    return Err((
                        StatusCode::NOT_FOUND,
                        Json(json!({"code": 10062, "message": "Original response not found"})),
                    ));
                }
            };
            if let Some(c) = body.get("content").and_then(Value::as_str) {
                msg.content = c.to_string();
            }
            if body.get("components").is_some() {
                let comps: Vec<Value> = body
                    .get("components")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                msg.components = comps;
            }
            if let Some(f) = body.get("flags").and_then(Value::as_u64) {
                msg.flags = f;
            }
            msg.edited_timestamp = Some(crate::state::now_rfc3339());
            msg.clone()
        };
        let content = msg_clone.content.clone();
        let ephemeral = msg_clone.flags & crate::state::MESSAGE_FLAG_EPHEMERAL != 0;
        if let Some(i) = w.interactions.get_mut(&interaction_id) {
            i.state = InteractionState::Responded;
        }
        w.record_action(BotActionKind::InteractionEdit {
            interaction_id,
            message_id: mid,
            content: Some(content),
        });
        let rest_json = w.message_to_discord_json(&msg_clone, false);
        if !ephemeral {
            let gateway_json = w.message_to_discord_json(&msg_clone, true);
            state.publish("MESSAGE_UPDATE", gateway_json);
        }
        (rest_json, ephemeral)
    };

    Ok(Json(rest_json))
}

/// `POST /api/v10/webhooks/{application_id}/{token}` のクエリ
#[derive(Debug, Default, Deserialize)]
pub struct FollowupQuery {
    /// true ならメッセージ本体を返す
    pub wait: Option<bool>,
}

/// `POST /api/v10/webhooks/{application_id}/{token}`(followup メッセージ)
pub async fn create_followup(
    State(state): State<AppState>,
    Path((_application_id, token)): Path<(Id, String)>,
    Query(_query): Query<FollowupQuery>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // interaction を探す
    let interaction_id = {
        let w = state.world();
        let mut found = None;
        for (id, i) in &w.interactions {
            if i.token == token {
                found = Some(*id);
                break;
            }
        }
        found
    };
    let interaction_id = match interaction_id {
        Some(id) => id,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"code": 10062, "message": "Unknown interaction"})),
            ));
        }
    };

    let (channel_id, app_id, response_kind) = {
        let w = state.world();
        let i = w.interactions.get(&interaction_id).unwrap();
        (
            i.channel_id,
            i.application_id,
            response_message_kind(i.kind),
        )
    };

    let flags = body.get("flags").and_then(Value::as_u64).unwrap_or(0);
    let ephemeral = flags & crate::state::MESSAGE_FLAG_EPHEMERAL != 0;
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let components: Vec<Value> = body
        .get("components")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let rest_json = {
        let mut w = state.world();
        let id = w.alloc_id();
        let msg = Message {
            id,
            channel_id,
            author_id: app_id,
            content: content.clone(),
            timestamp: crate::state::now_rfc3339(),
            edited_timestamp: None,
            kind: response_kind,
            message_reference: None,
            mentions: Vec::new(),
            components,
            flags,
            reactions: Vec::new(),
            pinned: false,
            interaction_id: Some(interaction_id),
        };
        w.messages.insert(id, msg.clone());
        if !ephemeral {
            let gateway_json = w.message_to_discord_json(&msg, true);
            state.publish("MESSAGE_CREATE", gateway_json);
        }
        w.record_action(BotActionKind::InteractionFollowup {
            interaction_id,
            message_id: id,
            content: Some(content),
        });
        w.message_to_discord_json(&msg, false)
    };

    // serenity の create_followup_message は応答を Message として読むので、wait に関わらず本体を返す
    Ok(Json(rest_json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, Interaction, World};
    use axum::extract::{Path, Query, State};

    fn setup_interaction() -> (AppState, Id, Id, String) {
        let mut world = World::seed();
        let guild_id = world.primary_guild().unwrap().id;
        let app_id = world.application_id;
        let channel_id = world.channels.values().next().unwrap().id;
        let user_id = world.users.values().find(|u| !u.bot).unwrap().id;
        let interaction_id = world.alloc_id();
        let token = "tok123".to_string();
        world.interactions.insert(
            interaction_id,
            Interaction {
                id: interaction_id,
                token: token.clone(),
                application_id: app_id,
                guild_id,
                channel_id,
                user_id,
                kind: 3,
                data: json!({}),
                message_id: None,
                state: InteractionState::Pending,
                original_message_id: None,
                created_at: crate::state::now_rfc3339(),
            },
        );
        (AppState::new(world), interaction_id, app_id, token)
    }

    /// スラッシュコマンドへの応答は type 20、コンポーネント操作への応答は type 0
    #[tokio::test]
    async fn response_message_kind_follows_interaction_kind() {
        for (interaction_kind, want) in [(2u8, 20u8), (3, 0)] {
            let (state, interaction_id, _app_id, token) = setup_interaction();
            state
                .world()
                .interactions
                .get_mut(&interaction_id)
                .unwrap()
                .kind = interaction_kind;
            let body = json!({"type": 4, "data": {"content": "hello"}});
            interaction_callback(
                State(state.clone()),
                Path((interaction_id, token.clone())),
                Json(body),
            )
            .await
            .unwrap();
            let followup = create_followup(
                State(state.clone()),
                Path((Id(0), token)),
                Query(FollowupQuery::default()),
                Json(json!({"content": "again"})),
            )
            .await
            .unwrap()
            .0;
            assert_eq!(followup["type"], want);
            let w = state.world();
            assert!(w.messages.values().all(|m| m.kind == want));
            assert_eq!(w.messages.len(), 2);
        }
    }

    #[tokio::test]
    async fn callback_type4_creates_message() {
        let (state, interaction_id, _app_id, token) = setup_interaction();
        let msg_count_before = state.world().messages.len();
        let body = json!({"type": 4, "data": {"content": "hello", "flags": 0}});
        let res = interaction_callback(
            State(state.clone()),
            Path((interaction_id, token.clone())),
            Json(body),
        )
        .await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), StatusCode::NO_CONTENT);
        let w = state.world();
        assert_eq!(w.messages.len(), msg_count_before + 1);
        let ix = w.interactions.get(&interaction_id).unwrap();
        assert!(ix.original_message_id.is_some());
        assert_eq!(ix.state, InteractionState::Responded);
    }

    #[tokio::test]
    async fn callback_double_rejected() {
        let (state, interaction_id, _app_id, token) = setup_interaction();
        let body = json!({"type": 4, "data": {"content": "hi", "flags": 0}});
        let _ = interaction_callback(
            State(state.clone()),
            Path((interaction_id, token.clone())),
            Json(body.clone()),
        )
        .await;
        let res = interaction_callback(
            State(state.clone()),
            Path((interaction_id, token)),
            Json(body),
        )
        .await;
        assert!(res.is_err());
        let (status, json_body) = res.unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json_body.0["code"], 40060);
    }

    #[tokio::test]
    async fn callback_bad_token_404() {
        let (state, interaction_id, _app_id, _token) = setup_interaction();
        let body = json!({"type": 4, "data": {"content": "hi", "flags": 0}});
        let res = interaction_callback(
            State(state.clone()),
            Path((interaction_id, "wrong".to_string())),
            Json(body),
        )
        .await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn edit_and_followup_flow() {
        let (state, interaction_id, app_id, token) = setup_interaction();
        // callback type 5 (deferred)
        let body = json!({"type": 5, "data": {"flags": 64}});
        let _ = interaction_callback(
            State(state.clone()),
            Path((interaction_id, token.clone())),
            Json(body),
        )
        .await
        .unwrap();
        // edit original
        let edit_body = json!({"content": "edited", "flags": 32768});
        let res = edit_original_response(
            State(state.clone()),
            Path((app_id, token.clone())),
            Json(edit_body),
        )
        .await;
        assert!(res.is_ok());
        // followup
        let fu_body = json!({"content": "followup", "flags": 0});
        let q = FollowupQuery { wait: Some(true) };
        let res = create_followup(
            State(state.clone()),
            Path((app_id, token.clone())),
            Query(q),
            Json(fu_body),
        )
        .await;
        // serenity が Message として読めるよう、本体を返す
        let followup = res.unwrap().0;
        assert_eq!(followup["content"], "followup");
        let w = state.world();
        let ix = w.interactions.get(&interaction_id).unwrap();
        assert_eq!(ix.state, InteractionState::Responded);
    }

    /// type 6(遅延更新)で受けたコンポーネント操作の @original は、押されたメッセージを指す
    #[tokio::test]
    async fn edit_original_after_deferred_update_targets_clicked_message() {
        let (state, interaction_id, app_id, token) = setup_interaction();
        let clicked = {
            let mut w = state.world();
            let id = w.alloc_id();
            let channel_id = w.channels.values().next().unwrap().id;
            let author_id = w.bot_user_id;
            w.messages.insert(
                id,
                Message {
                    id,
                    channel_id,
                    author_id,
                    content: "before".into(),
                    timestamp: crate::state::now_rfc3339(),
                    edited_timestamp: None,
                    kind: 0,
                    message_reference: None,
                    mentions: Vec::new(),
                    components: Vec::new(),
                    flags: 0,
                    reactions: Vec::new(),
                    pinned: false,
                    interaction_id: None,
                },
            );
            w.interactions.get_mut(&interaction_id).unwrap().message_id = Some(id);
            id
        };
        let _ = interaction_callback(
            State(state.clone()),
            Path((interaction_id, token.clone())),
            Json(json!({"type": 6})),
        )
        .await
        .unwrap();
        let res = edit_original_response(
            State(state.clone()),
            Path((app_id, token)),
            Json(json!({"content": "after"})),
        )
        .await
        .unwrap();
        assert_eq!(res.0["content"], "after");
        assert_eq!(state.world().messages[&clicked].content, "after");
    }

    #[tokio::test]
    async fn register_commands_assigns_id() {
        let world = World::seed();
        let app_id = world.application_id;
        let guild_id = world.primary_guild().unwrap().id;
        let state = AppState::new(world);
        let body = vec![json!({"name": "noth", "description": "test"})];
        let res =
            register_guild_commands(State(state.clone()), Path((app_id, guild_id)), Json(body))
                .await;
        let arr = res.0;
        assert!(arr.is_array());
        assert_eq!(arr.as_array().unwrap().len(), 1);
        assert!(arr[0].get("id").is_some());
    }
}
