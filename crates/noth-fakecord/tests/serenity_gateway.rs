//! 本物の serenity クライアントを fakecord の Gateway につないで、READY / 返信 / Resume を確かめる結合テスト。
//! noth 本体は起動しない。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use noth_fakecord::router;
use noth_fakecord::state::{AppState, World};
use serde_json::{Value, json};
use serenity::all as sn;
use serenity::async_trait;
use serenity::client::ClientBuilder;
use serenity::http::HttpBuilder;
use serenity::prelude::*;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// ハンドラが見たもの
#[derive(Debug)]
enum Seen {
    Ready(Box<sn::Ready>),
    Resume,
    Message {
        content: String,
        referenced: Option<(String, bool)>,
        kind: sn::MessageType,
    },
    ThreadListSync(usize),
}

struct Handler(mpsc::UnboundedSender<Seen>);

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _: Context, ready: sn::Ready) {
        let _ = self.0.send(Seen::Ready(Box::new(ready)));
    }
    async fn resume(&self, _: Context, _: sn::ResumedEvent) {
        let _ = self.0.send(Seen::Resume);
    }
    async fn message(&self, _: Context, msg: sn::Message) {
        let _ = self.0.send(Seen::Message {
            content: msg.content.clone(),
            referenced: msg
                .referenced_message
                .as_ref()
                .map(|m| (m.content.clone(), m.author.bot)),
            kind: msg.kind,
        });
    }
    async fn thread_list_sync(&self, _: Context, sync: sn::ThreadListSyncEvent) {
        let _ = self.0.send(Seen::ThreadListSync(sync.threads.len()));
    }
}

struct Harness {
    /// fakecord への直接の URL(操作用 API と REST)
    base: String,
    state: AppState,
    /// Gateway の経路上の TCP 接続。abort すると切れる
    conns: Arc<Mutex<Vec<JoinHandle<()>>>>,
    rx: mpsc::UnboundedReceiver<Seen>,
    _client: JoinHandle<()>,
}

/// fakecord を立て、Gateway だけ TCP プロキシ越しにして serenity のクライアントをつなぐ
async fn start() -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = proxy.local_addr().unwrap().port();
    let mut world = World::seed();
    // `GET /gateway` と READY の resume_gateway_url がプロキシを指す
    world.listen_port = proxy_port;
    let state = AppState::new(world);
    let app = router::build(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let conns: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
    let conns2 = conns.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut a, _)) = proxy.accept().await else {
                break;
            };
            let handle = tokio::spawn(async move {
                let Ok(mut b) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
            });
            conns2.lock().unwrap().push(handle);
        }
    });

    let base = format!("http://127.0.0.1:{port}");
    let http = HttpBuilder::new("fake-token")
        .proxy(base.clone())
        .ratelimiter_disabled(true)
        .build();
    let intents =
        GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
    let (tx, rx) = mpsc::unbounded_channel();
    let mut client = ClientBuilder::new_with_http(http, intents)
        .event_handler(Handler(tx))
        .await
        .unwrap();
    let client_task = tokio::spawn(async move {
        let _ = client.start().await;
    });
    Harness {
        base,
        state,
        conns,
        rx,
        _client: client_task,
    }
}

impl Harness {
    async fn next(&mut self) -> Seen {
        tokio::time::timeout(Duration::from_secs(30), self.rx.recv())
            .await
            .expect("イベントが届きません")
            .expect("ハンドラが閉じました")
    }

    /// 条件に合うイベントまで読み進める
    async fn wait_for(&mut self, mut pred: impl FnMut(&Seen) -> bool) -> Seen {
        loop {
            let seen = self.next().await;
            if pred(&seen) {
                return seen;
            }
        }
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let resp = reqwest::Client::new()
            .post(format!("{}{path}", self.base))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{path}: {}", resp.status());
        resp.json().await.unwrap()
    }

    async fn send(&self, content: &str) -> Value {
        self.post(
            "/_fake/send",
            json!({"channel": "general", "content": content}),
        )
        .await
    }

    /// Gateway の TCP 接続を全部切る(Close フレームも送らない)
    fn cut_tcp(&self) {
        for handle in self.conns.lock().unwrap().drain(..) {
            handle.abort();
        }
    }
}

fn is_message(seen: &Seen, want: &str) -> bool {
    matches!(seen, Seen::Message { content, .. } if content == want)
}

/// READY の application.flags と shard が serenity の型に載る
#[tokio::test]
async fn ready_carries_flags_and_shard() {
    let mut h = start().await;
    let Seen::Ready(ready) = h.wait_for(|s| matches!(s, Seen::Ready(_))).await else {
        unreachable!()
    };
    assert_eq!(ready.application.flags.bits(), 1 << 19);
    let shard = ready.shard.expect("shard が READY に無い");
    assert_eq!((shard.id.get(), shard.total), (0, 1));
}

/// 返信には referenced_message と type 19 が付き、noth が返信先の作者を判定できる
#[tokio::test]
async fn reply_has_referenced_message() {
    let mut h = start().await;
    h.wait_for(|s| matches!(s, Seen::Ready(_))).await;

    // bot の発言を REST で作り、それへ kurazu が返信する
    let http = HttpBuilder::new("fake-token")
        .proxy(h.base.clone())
        .ratelimiter_disabled(true)
        .build();
    let channel = h.state.world().find_channel("general").unwrap().id;
    let bot_msg = sn::ChannelId::new(channel.0)
        .say(&http, "bot says")
        .await
        .unwrap();
    h.post(
        "/_fake/send",
        json!({"channel": "general", "content": "reply", "reply_to": bot_msg.id.to_string()}),
    )
    .await;

    let seen = h.wait_for(|s| is_message(s, "reply")).await;
    let Seen::Message {
        referenced, kind, ..
    } = seen
    else {
        unreachable!()
    };
    assert_eq!(kind, sn::MessageType::InlineReply);
    assert_eq!(referenced, Some(("bot says".to_string(), true)));
}

/// THREAD_LIST_SYNC を配信する操作が、serenity の ThreadListSyncEvent として読める
#[tokio::test]
async fn thread_list_sync_reaches_client() {
    let mut h = start().await;
    h.wait_for(|s| matches!(s, Seen::Ready(_))).await;
    h.post(
        "/_fake/threads",
        json!({"channel": "general", "name": "t1"}),
    )
    .await;
    h.post("/_fake/thread-list-sync", json!({})).await;
    let seen = h.wait_for(|s| matches!(s, Seen::ThreadListSync(_))).await;
    assert!(matches!(seen, Seen::ThreadListSync(1)));
}

/// TCP を切っても Resume でき、切断中のイベントが届いて、resume ハンドラが呼ばれる
#[tokio::test]
async fn resume_delivers_events_missed_during_outage() {
    let mut h = start().await;
    h.wait_for(|s| matches!(s, Seen::Ready(_))).await;

    h.send("before").await;
    h.wait_for(|s| is_message(s, "before")).await;

    h.cut_tcp();
    h.send("during-outage").await;

    // 切断中に起きた発言は、再接続(Resume)のあとに届く。Identify し直していれば READY が先に来る
    let mut order = Vec::new();
    loop {
        let seen = h.next().await;
        match &seen {
            Seen::Ready(_) => panic!("Identify し直した。Resume できていない"),
            Seen::Resume => order.push("resume"),
            s if is_message(s, "during-outage") => order.push("during-outage"),
            _ => {}
        }
        if order.len() == 2 {
            break;
        }
    }
    // 再送されたイベントのあとに RESUMED が来る
    assert_eq!(order, ["during-outage", "resume"]);

    // Resume のあとも seq が続き、新しいイベントが届く
    h.send("after").await;
    h.wait_for(|s| is_message(s, "after")).await;
}
