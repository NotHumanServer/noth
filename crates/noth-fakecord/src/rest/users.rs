//! ユーザー・アプリケーション・gateway 情報の REST
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

use crate::state::AppState;

/// Authorization ヘッダーが無ければ 401
fn check_auth(headers: &HeaderMap) -> Result<(), StatusCode> {
    if headers.contains_key("authorization") {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// `GET /api/v10/gateway`。`{"url": "ws://127.0.0.1:{port}/ws"}`
pub async fn get_gateway(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&headers)?;
    let port = state.world().listen_port;
    Ok(Json(json!({ "url": format!("ws://127.0.0.1:{port}/ws") })))
}

/// `GET /api/v10/gateway/bot`。url に shards と session_start_limit を足したもの
pub async fn get_gateway_bot(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&headers)?;
    let port = state.world().listen_port;
    Ok(Json(json!({
        "url": format!("ws://127.0.0.1:{port}/ws"),
        "shards": 1,
        "session_start_limit": {
            "total": 1000,
            "remaining": 1000,
            "reset_after": 0,
            "max_concurrency": 1,
        },
    })))
}

/// `GET /api/v10/users/@me`。bot ユーザー
pub async fn get_me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&headers)?;
    let world = state.world();
    let user = world
        .users
        .get(&world.bot_user_id)
        .map(|u| u.to_discord_json())
        .unwrap_or_else(|| json!(null));
    Ok(Json(user))
}

/// `GET /api/v10/oauth2/applications/@me`
pub async fn get_application_me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&headers)?;
    let world = state.world();
    let app_id = world.application_id;
    let bot_user = world.users.get(&world.bot_user_id);
    let kurazu = world
        .users
        .values()
        .find(|u| u.username.eq_ignore_ascii_case("kurazu"));
    let name = bot_user
        .map(|u| u.username.clone())
        .unwrap_or_else(|| "noth".into());
    let owner = kurazu
        .map(|u| u.to_discord_json())
        .unwrap_or_else(|| json!(null));
    let verify_key = "0".repeat(64);
    Ok(Json(json!({
        "id": app_id,
        "name": name,
        "icon": null,
        "description": "",
        "bot_public": true,
        "bot_require_code_grant": false,
        "owner": owner,
        "verify_key": verify_key,
        // noth の health_check が要求する GATEWAY_MESSAGE_CONTENT_LIMITED
        "flags": crate::gateway::APPLICATION_FLAGS,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::World;
    use axum::http::HeaderValue;
    use serenity::model::application::CurrentApplicationInfo;
    use serenity::model::gateway::{BotGateway, Gateway};
    use serenity::model::user::CurrentUser;

    fn state_with_port(port: u16) -> AppState {
        let mut world = World::seed();
        world.listen_port = port;
        AppState::new(world)
    }

    fn auth_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_str("Bot token").unwrap());
        h
    }

    #[tokio::test]
    async fn gateway_requires_auth() {
        let state = state_with_port(3900);
        let res = get_gateway(State(state), HeaderMap::new()).await;
        assert_eq!(res.unwrap_err(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn gateway_returns_url() {
        let state = state_with_port(3900);
        let Json(v) = get_gateway(State(state), auth_headers()).await.unwrap();
        assert_eq!(v["url"], "ws://127.0.0.1:3900/ws");
        let _: Gateway = serde_json::from_value(v).unwrap();
    }

    #[tokio::test]
    async fn gateway_bot_returns_fields() {
        let state = state_with_port(3900);
        let Json(v) = get_gateway_bot(State(state), auth_headers()).await.unwrap();
        assert_eq!(v["shards"], 1);
        assert_eq!(v["session_start_limit"]["total"], 1000);
        assert_eq!(v["session_start_limit"]["remaining"], 1000);
        assert_eq!(v["session_start_limit"]["reset_after"], 0);
        assert_eq!(v["session_start_limit"]["max_concurrency"], 1);
        let _: BotGateway = serde_json::from_value(v).unwrap();
    }

    #[tokio::test]
    async fn get_me_returns_bot_user() {
        let state = state_with_port(3900);
        let Json(v) = get_me(State(state), auth_headers()).await.unwrap();
        assert_eq!(v["bot"], true);
        assert_eq!(v["discriminator"], "0");
        assert!(v.get("global_name").is_some());
        assert_eq!(v["avatar"], Value::Null);
        let _: CurrentUser = serde_json::from_value(v).unwrap();
    }

    #[tokio::test]
    async fn get_application_me_deserializes() {
        let state = state_with_port(3900);
        let Json(v) = get_application_me(State(state), auth_headers())
            .await
            .unwrap();
        assert_eq!(v["description"], "");
        assert_eq!(v["icon"], Value::Null);
        assert_eq!(v["flags"], crate::gateway::APPLICATION_FLAGS);
        let app: CurrentApplicationInfo = serde_json::from_value(v).unwrap();
        assert!(app.owner.is_some());
    }
}
