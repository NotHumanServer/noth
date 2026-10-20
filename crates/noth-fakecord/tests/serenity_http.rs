//! serenity の Http を実際に fakecord へ向けて、noth が使う REST が通ることを確かめる結合テスト。
//! noth 本体は起動しない。noth の `api_base` と同じ組み立て(proxy + ratelimiter_disabled)を使う。

use serde_json::{Value, json};
use serenity::builder::{
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
};
use serenity::http::{Http, HttpBuilder};
use serenity::model::id::{ChannelId, InteractionId};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// port 0 で serve を起動し、実際のベース URL を返す
async fn start() -> (String, JoinHandle<anyhow::Result<()>>) {
    let (tx, rx) = oneshot::channel();
    let handle = tokio::spawn(noth_fakecord::serve_with_ready(0, None, Some(tx)));
    let addr = rx.await.expect("fakecord が起動しませんでした");
    (format!("http://{addr}"), handle)
}

/// ratelimiter_disabled(true) を外すと serenity は proxy を無視して本物の Discord に向かう
fn http_for(base: &str) -> Http {
    HttpBuilder::new("fake-token")
        .proxy(base)
        .ratelimiter_disabled(true)
        .build()
}

async fn fake_get(base: &str, path: &str) -> Value {
    reqwest::get(format!("{base}{path}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn fake_post(base: &str, path: &str, body: Value) -> Value {
    let resp = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{path}: {}", resp.status());
    resp.json().await.unwrap()
}

#[tokio::test]
async fn serenity_http_talks_to_fakecord() {
    let (base, server) = start().await;
    let http = http_for(&base);

    let me = http.get_current_user().await.unwrap();
    assert_eq!(me.name, "noth");
    assert!(me.bot);

    let status = fake_get(&base, "/_fake/status").await;
    let guild_id: u64 = status["guild_id"].as_str().unwrap().parse().unwrap();
    let guild = http.get_guild(guild_id.into()).await.unwrap();
    assert_eq!(guild.name, "noth-dev");

    let channels = http.get_channels(guild_id.into()).await.unwrap();
    let general = channels.iter().find(|c| c.name == "general").unwrap();

    let sent = ChannelId::new(general.id.get())
        .send_message(&http, CreateMessage::new().content("結合テスト"))
        .await
        .unwrap();
    assert_eq!(sent.content, "結合テスト");
    assert_eq!(sent.author.id, me.id);

    // 送ったメッセージが操作ログに Bot の投稿として残る
    let log = fake_get(&base, "/_fake/log").await;
    assert_eq!(log["actions"][0]["action"], "post");

    server.abort();
}

/// slash で作った interaction に、serenity の callback / followup / @original 編集で応答できる
/// (control.rs が作った token を rest/interactions.rs が同じ形で受けること)
#[tokio::test]
async fn slash_interaction_can_be_answered_via_serenity() {
    let (base, server) = start().await;
    let http = http_for(&base);
    // followup と @original は application_id が要る。noth は Gateway の READY から設定される
    let app = http.get_current_application_info().await.unwrap();
    http.set_application_id(app.id);

    // 未登録のコマンドは slash できないので、noth と同じく先に登録する
    let status = fake_get(&base, "/_fake/status").await;
    let guild: u64 = status["guild_id"].as_str().unwrap().parse().unwrap();
    http.create_guild_commands(
        serenity::model::id::GuildId::new(guild),
        &json!([{"name": "noth", "description": "noth", "type": 1,
                 "options": [{"name": "context", "description": "context", "type": 1}]}]),
    )
    .await
    .unwrap();

    let slash = fake_post(
        &base,
        "/_fake/slash",
        json!({"name": "noth", "path": ["context"]}),
    )
    .await;
    let id: u64 = slash["interaction_id"].as_str().unwrap().parse().unwrap();
    let token = slash["token"].as_str().unwrap().to_string();
    let cursor = slash["cursor"].as_u64().unwrap();

    let response = CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new().content("first"),
    );
    http.create_interaction_response(InteractionId::new(id), &token, &response, vec![])
        .await
        .unwrap();

    let followup = http
        .create_followup_message(&token, &json!({"content": "second"}), vec![])
        .await
        .unwrap();
    assert_eq!(followup.content, "second");

    let edited = http
        .edit_original_interaction_response(&token, &json!({"content": "first (edited)"}), vec![])
        .await
        .unwrap();
    assert_eq!(edited.content, "first (edited)");

    let waited = fake_post(
        &base,
        "/_fake/wait-reply",
        json!({"since": cursor, "timeout_secs": 5, "settle_ms": 100}),
    )
    .await;
    assert_eq!(waited["replied"], true);
    let kinds: Vec<&str> = waited["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "interaction_response",
            "interaction_followup",
            "interaction_edit"
        ]
    );

    server.abort();
}
