//! Gateway(WebSocket)。Hello / Identify / heartbeat / READY / GUILD_CREATE / Resume とイベント配信を担当する

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::{AppState, Session};

/// serenity が接続時に付ける `?v=10&encoding=json`。どちらも省略されうる
#[derive(Debug, Default, Deserialize)]
pub struct GatewayQuery {
    pub v: Option<u8>,
    pub encoding: Option<String>,
}

/// `GET /ws`。`GET /api/v10/gateway` が返す url の受け口
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(_query): Query<GatewayQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_connection(socket, state))
}

/// Discord が application.flags に立てる GATEWAY_MESSAGE_CONTENT_LIMITED。noth の health_check が要求する
pub const APPLICATION_FLAGS: u64 = 1 << 19;

/// Gateway 切断時に送る Close フレーム。1012(再起動)なら serenity は Resume を試みる。
/// フレーム無しの Close は serenity をビジーループに入れるので使わない
fn restart_close() -> Message {
    Message::Close(Some(CloseFrame {
        code: 1012,
        reason: "".into(),
    }))
}

async fn send_text(socket: &mut WebSocket, text: String) -> bool {
    socket.send(Message::Text(text.into())).await.is_ok()
}

/// Identify への応答。READY と GUILD_CREATE を新しいセッションの履歴に積み、送るフレームを返す。
/// 購読位置の決定とスナップショットの作成は World のロックの中で行い、間のイベントが二重に届かないようにする
fn identify(state: &AppState, shard: Value) -> (String, Vec<String>) {
    let mut w = state.world();
    let sid = w.alloc_id().to_string();
    let bot_user = w
        .users
        .get(&w.bot_user_id)
        .map(|u| u.to_discord_json())
        .unwrap_or(Value::Null);
    let guilds: Vec<Value> = w
        .guilds
        .keys()
        .map(|id| json!({"id": id, "unavailable": true}))
        .collect();
    let creates: Vec<Value> = w
        .guilds
        .keys()
        .filter_map(|id| w.guild_create_json(*id))
        .collect();
    let mut ready = json!({
        "v": 10,
        "user": bot_user,
        "guilds": guilds,
        "session_id": sid,
        "resume_gateway_url": format!("ws://127.0.0.1:{}/ws", w.listen_port),
        "application": {"id": w.application_id, "flags": APPLICATION_FLAGS},
    });
    if !shard.is_null() {
        ready["shard"] = shard;
    }

    // ここから先に publish されたイベントだけをセッションに流す
    let mut session = Session::new(state.event_log().head());
    session.push("READY", ready);
    for gc in creates {
        session.push("GUILD_CREATE", gc);
    }
    let frames = session.frames_after(0);
    state.sessions().insert(sid.clone(), session);
    (sid, frames)
}

async fn handle_connection(socket: WebSocket, state: AppState) {
    let mut socket = socket;

    // 接続直後に Hello
    let hello = json!({"op": 10, "d": {"heartbeat_interval": 41250}});
    if !send_text(&mut socket, hello.to_string()).await {
        return;
    }

    // この接続が持っているセッション(ID と、Resume で奪われたか判定する札)
    let mut session: Option<(String, u64)> = None;
    // セッションのうち、この接続へ送り終えた seq
    let mut sent: u64 = 0;
    let mut head_rx = state.head.subscribe();
    let mut shutdown_rx = state.shutdown.subscribe();

    'outer: loop {
        let active = session.is_some();
        tokio::select! {
            msg = socket.recv() => {
                let Some(msg) = msg else { break 'outer; };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::debug!("gateway recv error: {e}");
                        break 'outer;
                    }
                };
                match msg {
                    Message::Text(text) => {
                        let value: Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let op = value.get("op").and_then(|v| v.as_u64()).unwrap_or(99);
                        match op {
                            1 => {
                                let ack = json!({"op": 11});
                                let _ = send_text(&mut socket, ack.to_string()).await;
                            }
                            2 => {
                                // shard はそのまま READY に返す
                                let shard = value
                                    .pointer("/d/shard")
                                    .cloned()
                                    .unwrap_or(Value::Null);
                                let (sid, frames) = identify(&state, shard);
                                session = Some((sid, 1));
                                sent = frames.len() as u64;
                                for frame in frames {
                                    if !send_text(&mut socket, frame).await {
                                        break 'outer;
                                    }
                                }
                            }
                            6 => {
                                let d = value.get("d").cloned().unwrap_or(Value::Null);
                                let sid = d
                                    .get("session_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let client_seq = d.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
                                match resume(&state, &sid, client_seq) {
                                    Some(resumed) => {
                                        session = Some((sid, resumed.token));
                                        sent = resumed.seq;
                                        for frame in resumed.frames {
                                            if !send_text(&mut socket, frame).await {
                                                break 'outer;
                                            }
                                        }
                                    }
                                    None => {
                                        let invalid = json!({"op": 9, "d": false});
                                        let _ = send_text(&mut socket, invalid.to_string()).await;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Message::Binary(_) => {}
                    Message::Ping(p) => {
                        let _ = socket.send(Message::Pong(p)).await;
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => {
                        break 'outer;
                    }
                }
            }
            changed = async {
                if active {
                    head_rx.changed().await
                } else {
                    std::future::pending().await
                }
            } => {
                if changed.is_err() {
                    break 'outer;
                }
                let Some((sid, token)) = session.as_ref() else { continue; };
                // 新しいイベントをセッションへ取り込み、未送信の分を取り出す。
                // Resume で別の接続に引き継がれていたら、この接続は降りる
                let frames = {
                    let mut sessions = state.sessions();
                    match sessions.get_mut(sid) {
                        Some(s) if s.owner == *token => {
                            if !s.pull(&state.event_log()) {
                                tracing::warn!("gateway のイベントが履歴から溢れました");
                            }
                            let frames = s.frames_after(sent);
                            sent = s.seq;
                            frames
                        }
                        _ => break 'outer,
                    }
                };
                for frame in frames {
                    if !send_text(&mut socket, frame).await {
                        break 'outer;
                    }
                }
            }
            // serve の終了時に AppState::shutdown が呼ばれる。開いたままの WebSocket が graceful shutdown を止めないよう閉じる
            _ = shutdown_rx.changed() => {
                let _ = socket.send(restart_close()).await;
                break 'outer;
            }
        }
    }
}

/// Resume が通ったときの結果
struct Resumed {
    /// 引き継いだセッションの札
    token: u64,
    /// 再送するイベントと、最後の RESUMED
    frames: Vec<String>,
    /// 送り終えたあとの seq
    seq: u64,
}

/// Resume を試みる。セッションが無い・seq が未来・履歴から溢れている・ログから溢れているときは None
/// (溢れたセッションは捨てる。クライアントは op 9 を受けて Identify し直す)
fn resume(state: &AppState, sid: &str, client_seq: u64) -> Option<Resumed> {
    let mut sessions = state.sessions();
    let s = sessions.get_mut(sid)?;
    // 切断中に溜まった分を先に取り込む(取りこぼしがあれば、その位置以前からは再開できない)
    s.pull(&state.event_log());
    if !s.can_resume_from(client_seq) {
        sessions.remove(sid);
        return None;
    }
    s.owner += 1;
    // serenity の ResumedEvent は空のマップしか読めない
    let resumed = json!({"op": 0, "s": s.seq, "t": "RESUMED", "d": {}});
    let mut frames = s.frames_after(client_seq);
    frames.push(resumed.to_string());
    Some(Resumed {
        token: s.owner,
        frames,
        seq: s.seq,
    })
}

#[cfg(test)]
mod tests {
    use crate::router;
    use crate::state::{AppState, World};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    async fn start_server() -> (u16, AppState) {
        let mut world = World::seed();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        world.listen_port = port;
        let state = AppState::new(world);
        let app = router::build(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, state)
    }

    fn parse(msg: WsMessage) -> Value {
        serde_json::from_str(msg.to_text().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn hello_identify_ready_guild_create() {
        let (port, _state) = start_server().await;
        let url = format!("ws://127.0.0.1:{}/ws", port);
        let (mut ws, _) = connect_async(&url).await.unwrap();

        let hello = parse(ws.next().await.unwrap().unwrap());
        assert_eq!(hello["op"], 10);
        assert_eq!(hello["d"]["heartbeat_interval"], 41250);

        let identify =
            json!({"op": 2, "d": {"token": "x", "intents": 0, "properties": {}, "compress": true}});
        ws.send(WsMessage::Text(identify.to_string().into()))
            .await
            .unwrap();

        let ready = parse(ws.next().await.unwrap().unwrap());
        assert_eq!(ready["op"], 0);
        assert_eq!(ready["t"], "READY");
        assert_eq!(ready["s"], 1);
        assert_eq!(ready["d"]["v"], 10);
        assert!(ready["d"]["user"]["bot"].as_bool().unwrap());
        assert_eq!(ready["d"]["guilds"][0]["unavailable"], true);
        assert!(ready["d"]["session_id"].is_string());
        assert!(
            ready["d"]["resume_gateway_url"]
                .as_str()
                .unwrap()
                .ends_with("/ws")
        );
        assert_eq!(ready["d"]["application"]["flags"], 1 << 19);

        let gc = parse(ws.next().await.unwrap().unwrap());
        assert_eq!(gc["op"], 0);
        assert_eq!(gc["t"], "GUILD_CREATE");
    }

    #[tokio::test]
    async fn heartbeat_ack() {
        let (port, _state) = start_server().await;
        let url = format!("ws://127.0.0.1:{}/ws", port);
        let (mut ws, _) = connect_async(&url).await.unwrap();
        let _ = ws.next().await; // hello
        let identify = json!({"op": 2, "d": {"token": "x", "intents": 0, "properties": {}}});
        ws.send(WsMessage::Text(identify.to_string().into()))
            .await
            .unwrap();
        let _ = ws.next().await; // READY
        let _ = ws.next().await; // GUILD_CREATE

        let hb = json!({"op": 1, "d": null});
        ws.send(WsMessage::Text(hb.to_string().into()))
            .await
            .unwrap();
        let ack = parse(ws.next().await.unwrap().unwrap());
        assert_eq!(ack["op"], 11);
    }

    #[tokio::test]
    async fn dispatch_event_delivered() {
        let (port, state) = start_server().await;
        let url = format!("ws://127.0.0.1:{}/ws", port);
        let (mut ws, _) = connect_async(&url).await.unwrap();
        let _ = ws.next().await; // hello
        let identify = json!({"op": 2, "d": {"token": "x", "intents": 0, "properties": {}}});
        ws.send(WsMessage::Text(identify.to_string().into()))
            .await
            .unwrap();
        let _ = ws.next().await; // READY
        let _ = ws.next().await; // GUILD_CREATE

        state.publish("MESSAGE_CREATE", json!({"id": "123"}));
        let msg = parse(ws.next().await.unwrap().unwrap());
        assert_eq!(msg["op"], 0);
        assert_eq!(msg["t"], "MESSAGE_CREATE");
        assert_eq!(msg["d"]["id"], "123");
    }

    #[tokio::test]
    async fn resume_unknown_invalid_session() {
        let (port, _state) = start_server().await;
        let url = format!("ws://127.0.0.1:{}/ws", port);
        let (mut ws, _) = connect_async(&url).await.unwrap();
        let _ = ws.next().await; // hello
        let resume = json!({"op": 6, "d": {"token": "x", "session_id": "nope", "seq": 0}});
        ws.send(WsMessage::Text(resume.to_string().into()))
            .await
            .unwrap();
        let msg = parse(ws.next().await.unwrap().unwrap());
        assert_eq!(msg["op"], 9);
        assert_eq!(msg["d"], false);
    }

    #[tokio::test]
    async fn resume_known_resumed() {
        let (port, _state) = start_server().await;
        let url = format!("ws://127.0.0.1:{}/ws", port);
        let (mut ws, _) = connect_async(&url).await.unwrap();
        let _ = ws.next().await; // hello
        let identify = json!({"op": 2, "d": {"token": "x", "intents": 0, "properties": {}}});
        ws.send(WsMessage::Text(identify.to_string().into()))
            .await
            .unwrap();
        let ready = parse(ws.next().await.unwrap().unwrap());
        let sid = ready["d"]["session_id"].as_str().unwrap().to_string();
        let _ = ws.next().await; // GUILD_CREATE
        drop(ws);

        let (mut ws2, _) = connect_async(&url).await.unwrap();
        let _ = ws2.next().await; // hello
        let resume = json!({"op": 6, "d": {"token": "x", "session_id": sid, "seq": 2}});
        ws2.send(WsMessage::Text(resume.to_string().into()))
            .await
            .unwrap();
        let msg = parse(ws2.next().await.unwrap().unwrap());
        assert_eq!(msg["t"], "RESUMED");
        assert_eq!(msg["d"], json!({}));
    }

    /// 終了の合図はサーバーごと。片方を止めても、もう片方の接続は生きている
    #[tokio::test]
    async fn shutdown_is_per_server() {
        let (port_a, state_a) = start_server().await;
        let (port_b, _state_b) = start_server().await;
        let (mut ws_a, _) = connect_async(&format!("ws://127.0.0.1:{port_a}/ws"))
            .await
            .unwrap();
        let (mut ws_b, _) = connect_async(&format!("ws://127.0.0.1:{port_b}/ws"))
            .await
            .unwrap();
        let _ = ws_a.next().await; // hello
        let _ = ws_b.next().await; // hello

        state_a.shutdown();
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), ws_a.next())
            .await
            .expect("A の接続が閉じられるはず");
        // フレーム無しの Close は serenity をビジーループに入れる。code 付きで閉じる
        match closed {
            Some(Ok(WsMessage::Close(Some(frame)))) => {
                assert_eq!(u16::from(frame.code), 1012);
            }
            other => panic!("code 付きの Close が来るはず: {other:?}"),
        }

        // B は生きていて、heartbeat に ACK を返す
        let hb = json!({"op": 1, "d": null});
        ws_b.send(WsMessage::Text(hb.to_string().into()))
            .await
            .unwrap();
        let ack = parse(ws_b.next().await.unwrap().unwrap());
        assert_eq!(ack["op"], 11);
    }

    /// Resume できるセッション ID もサーバーごと
    #[tokio::test]
    async fn sessions_are_per_server() {
        let (port_a, _state_a) = start_server().await;
        let (port_b, _state_b) = start_server().await;
        let (mut ws_a, _) = connect_async(&format!("ws://127.0.0.1:{port_a}/ws"))
            .await
            .unwrap();
        let _ = ws_a.next().await; // hello
        let identify = json!({"op": 2, "d": {"token": "x", "intents": 0, "properties": {}}});
        ws_a.send(WsMessage::Text(identify.to_string().into()))
            .await
            .unwrap();
        let ready = parse(ws_a.next().await.unwrap().unwrap());
        let sid = ready["d"]["session_id"].as_str().unwrap().to_string();

        let (mut ws_b, _) = connect_async(&format!("ws://127.0.0.1:{port_b}/ws"))
            .await
            .unwrap();
        let _ = ws_b.next().await; // hello
        let resume = json!({"op": 6, "d": {"token": "x", "session_id": sid, "seq": 0}});
        ws_b.send(WsMessage::Text(resume.to_string().into()))
            .await
            .unwrap();
        let msg = parse(ws_b.next().await.unwrap().unwrap());
        assert_eq!(msg["op"], 9);
    }

    type Ws = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn send_json(ws: &mut Ws, v: Value) {
        ws.send(WsMessage::Text(v.to_string().into()))
            .await
            .unwrap();
    }

    /// Hello を読み、Identify して READY と GUILD_CREATE を読み終える。(接続, session_id, 最後の seq)
    async fn identify(port: u16, shard: Option<Value>) -> (Ws, String, Value) {
        let (mut ws, _) = connect_async(&format!("ws://127.0.0.1:{port}/ws"))
            .await
            .unwrap();
        let _ = ws.next().await; // hello
        let mut d = json!({"token": "x", "intents": 0, "properties": {}});
        if let Some(shard) = shard {
            d["shard"] = shard;
        }
        send_json(&mut ws, json!({"op": 2, "d": d})).await;
        let ready = parse(ws.next().await.unwrap().unwrap());
        let _ = ws.next().await; // GUILD_CREATE
        let sid = ready["d"]["session_id"].as_str().unwrap().to_string();
        (ws, sid, ready)
    }

    async fn resume(port: u16, sid: &str, seq: u64) -> Ws {
        let (mut ws, _) = connect_async(&format!("ws://127.0.0.1:{port}/ws"))
            .await
            .unwrap();
        let _ = ws.next().await; // hello
        send_json(
            &mut ws,
            json!({"op": 6, "d": {"token": "x", "session_id": sid, "seq": seq}}),
        )
        .await;
        ws
    }

    /// Identify の shard はそのまま READY に返る
    #[tokio::test]
    async fn ready_echoes_shard() {
        let (port, _state) = start_server().await;
        let (_ws, _sid, ready) = identify(port, Some(json!([0, 1]))).await;
        assert_eq!(ready["d"]["shard"], json!([0, 1]));
        let event: serenity::model::event::ReadyEvent =
            serde_json::from_value(ready["d"].clone()).unwrap();
        assert_eq!(event.ready.shard.unwrap().total, 1);
    }

    /// 切断中のイベントは Resume で再送され、そのあとに RESUMED(d は {})が続く。seq は続きから振る
    #[tokio::test]
    async fn resume_replays_missed_events() {
        let (port, state) = start_server().await;
        let (ws, sid, _) = identify(port, None).await; // READY が 1、GUILD_CREATE が 2
        drop(ws);
        for i in 0..3 {
            state.publish("MESSAGE_CREATE", json!({"id": i.to_string()}));
        }

        let mut ws = resume(port, &sid, 2).await;
        for (i, want_seq) in (3..=5).enumerate() {
            let m = parse(ws.next().await.unwrap().unwrap());
            assert_eq!(m["t"], "MESSAGE_CREATE");
            assert_eq!(m["s"], want_seq);
            assert_eq!(m["d"]["id"], i.to_string());
        }
        let resumed = parse(ws.next().await.unwrap().unwrap());
        assert_eq!(resumed["t"], "RESUMED");
        assert_eq!(resumed["d"], json!({}));
        // serenity の ResumedEvent に読める(null だと UnknownEvent になる)
        let _: serenity::model::event::ResumedEvent =
            serde_json::from_value(resumed["d"].clone()).unwrap();

        // seq は続く
        state.publish("MESSAGE_CREATE", json!({"id": "later"}));
        let m = parse(ws.next().await.unwrap().unwrap());
        assert_eq!(m["s"], 6);
        assert_eq!(m["d"]["id"], "later");
    }

    /// クライアントが受け取り済みの seq までは再送しない
    #[tokio::test]
    async fn resume_skips_events_already_received() {
        let (port, state) = start_server().await;
        let (ws, sid, _) = identify(port, None).await;
        for i in 0..3 {
            state.publish("MESSAGE_CREATE", json!({"id": i.to_string()}));
        }
        // 3 件とも送り終えるのを待つ
        let mut ws = ws;
        for _ in 0..3 {
            let _ = ws.next().await;
        }
        drop(ws);

        // seq 4 まで受け取った(5 だけ取りこぼした)つもりで Resume する
        let mut ws = resume(port, &sid, 4).await;
        let m = parse(ws.next().await.unwrap().unwrap());
        assert_eq!(
            (m["s"].as_u64(), m["d"]["id"].as_str()),
            (Some(5), Some("2"))
        );
        let resumed = parse(ws.next().await.unwrap().unwrap());
        assert_eq!(resumed["t"], "RESUMED");
    }

    /// 古い接続がまだ生きているうちに Resume が来ても、イベントは新しい接続にだけ 1 回届く
    #[tokio::test]
    async fn resume_takes_over_live_session() {
        let (port, state) = start_server().await;
        let (mut old, sid, _) = identify(port, None).await;
        let mut new = resume(port, &sid, 2).await;
        let resumed = parse(new.next().await.unwrap().unwrap());
        assert_eq!(resumed["t"], "RESUMED");

        state.publish("MESSAGE_CREATE", json!({"id": "x"}));
        let m = parse(new.next().await.unwrap().unwrap());
        assert_eq!(
            (m["s"].as_u64(), m["d"]["id"].as_str()),
            (Some(3), Some("x"))
        );
        // 古い接続は降ろされる(イベントは来ず、閉じられる)
        let old_next = tokio::time::timeout(std::time::Duration::from_secs(5), old.next())
            .await
            .expect("古い接続が閉じられるはず");
        assert!(!matches!(old_next, Some(Ok(WsMessage::Text(_)))));
    }

    /// 履歴から溢れたセッションは op 9(d: false)
    #[tokio::test]
    async fn resume_beyond_history_is_invalid() {
        let (port, state) = start_server().await;
        // 接続中に履歴の上限を超えて送る
        let (mut ws, sid, _) = identify(port, None).await;
        let total = crate::state::EVENT_HISTORY + 100;
        for i in 0..total {
            state.publish("MESSAGE_CREATE", json!({"id": i.to_string()}));
            tokio::task::yield_now().await;
        }
        for _ in 0..total {
            let _ = ws.next().await.unwrap().unwrap();
        }
        drop(ws);

        let mut ws = resume(port, &sid, 2).await;
        let m = parse(ws.next().await.unwrap().unwrap());
        assert_eq!((m["op"].as_u64(), &m["d"]), (Some(9), &json!(false)));
    }

    /// 切断中にログの上限を超えて溜まったときも op 9
    #[tokio::test]
    async fn resume_beyond_event_log_is_invalid() {
        let (port, state) = start_server().await;
        let (ws, sid, _) = identify(port, None).await;
        drop(ws);
        for i in 0..crate::state::EVENT_HISTORY + 100 {
            state.publish("MESSAGE_CREATE", json!({"id": i.to_string()}));
        }
        let mut ws = resume(port, &sid, 2).await;
        let m = parse(ws.next().await.unwrap().unwrap());
        assert_eq!((m["op"].as_u64(), &m["d"]), (Some(9), &json!(false)));
    }

    /// 存在しない未来の seq での Resume も op 9
    #[tokio::test]
    async fn resume_with_future_seq_is_invalid() {
        let (port, _state) = start_server().await;
        let (_ws, sid, _) = identify(port, None).await;
        let mut ws = resume(port, &sid, 99).await;
        let m = parse(ws.next().await.unwrap().unwrap());
        assert_eq!(m["op"], 9);
    }

    /// Identify の前に publish されたイベントは GUILD_CREATE のスナップショットに入っているので、
    /// 二重に届けない。Identify の後のものだけ届く
    #[tokio::test]
    async fn events_before_identify_are_not_redelivered() {
        let (port, state) = start_server().await;
        {
            let mut w = state.world();
            let guild_id = *w.guilds.keys().next().unwrap();
            let id = w.alloc_id();
            w.channels.insert(
                id,
                crate::state::Channel {
                    id,
                    guild_id,
                    kind: crate::state::CHANNEL_TEXT,
                    name: "snap".into(),
                    parent_id: None,
                    position: 1,
                    topic: None,
                    owner_id: None,
                    archived: false,
                },
            );
            let cj = w.channels[&id].to_discord_json();
            state.publish("CHANNEL_CREATE", cj);
        }
        let (mut ws, _) = connect_async(&format!("ws://127.0.0.1:{port}/ws"))
            .await
            .unwrap();
        let _ = ws.next().await; // hello
        send_json(
            &mut ws,
            json!({"op": 2, "d": {"token": "x", "intents": 0, "properties": {}}}),
        )
        .await;
        let _ = ws.next().await; // READY
        let gc = parse(ws.next().await.unwrap().unwrap());
        let names: Vec<&str> = gc["d"]["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"snap"));

        state.publish("MESSAGE_CREATE", json!({"id": "after"}));
        let m = parse(ws.next().await.unwrap().unwrap());
        assert_eq!(m["t"], "MESSAGE_CREATE");
        assert_eq!(m["s"], 3);
    }
}
