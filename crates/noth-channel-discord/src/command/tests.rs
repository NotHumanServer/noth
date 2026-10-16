use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use noth_core::command::{Command, CommandEnv, ContextSnapshot, ContextSource, ReportSection};
use noth_core::exclusion::ChannelExclusions;
use noth_core::{Error, Result, async_trait};

use super::api::ApiError;
use super::render::FLAG_IS_COMPONENTS_V2;
use super::*;

const GUILD: u64 = 10;
const MASTER: u64 = 1;

type Rejects = fn(&Value) -> bool;

#[derive(Debug, Clone, PartialEq)]
enum Call {
    Sync(Value),
    Callback(u64, Value),
    Edit(Value),
    Followup(Value),
}

/// 呼び出しを記録し、`reject` に当たる本文を持つ編集・followup は 400 で断る
#[derive(Default)]
struct FakeApi {
    calls: Mutex<Vec<Call>>,
    reject: Mutex<Vec<Rejects>>,
}

impl FakeApi {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn rejecting(self, f: Rejects) -> Self {
        self.reject.lock().unwrap().push(f);
        self
    }

    fn check(&self, body: &Value) -> std::result::Result<(), ApiError> {
        if self.reject.lock().unwrap().iter().any(|f| f(body)) {
            Err(ApiError {
                status: 400,
                body: "rejected".into(),
                retry_after: None,
            })
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl InteractionApi for FakeApi {
    async fn sync_commands(&self, body: &Value) -> std::result::Result<(), ApiError> {
        self.calls.lock().unwrap().push(Call::Sync(body.clone()));
        Ok(())
    }
    async fn callback(
        &self,
        id: u64,
        _token: &str,
        body: &Value,
    ) -> std::result::Result<(), ApiError> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Callback(id, body.clone()));
        Ok(())
    }
    async fn edit_original(&self, _token: &str, body: &Value) -> std::result::Result<(), ApiError> {
        self.calls.lock().unwrap().push(Call::Edit(body.clone()));
        self.check(body)
    }
    async fn followup(&self, _token: &str, body: &Value) -> std::result::Result<(), ApiError> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Followup(body.clone()));
        self.check(body)
    }
}

struct FakeCommand {
    runs: Arc<AtomicUsize>,
    fail: bool,
}

#[async_trait]
impl Command for FakeCommand {
    fn name(&self) -> &'static str {
        "context"
    }
    fn description(&self) -> &'static str {
        "使用量を見る"
    }
    async fn run(&self, _env: &CommandEnv) -> Result<Report> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(Error::Other("actor が停止しています".into()));
        }
        Ok(Report {
            title: "文脈の使用量".into(),
            sections: vec![ReportSection {
                heading: "全体".into(),
                body: "42%".into(),
            }],
        })
    }
}

struct NoSnapshot;

#[async_trait]
impl ContextSource for NoSnapshot {
    async fn context_snapshot(&self) -> Result<ContextSnapshot> {
        Err(Error::Other("unused".into()))
    }
}

struct Fixture {
    api: Arc<FakeApi>,
    runs: Arc<AtomicUsize>,
    controller: Controller,
    exclusions: ChannelExclusions,
}

fn fixture(api: FakeApi, master: Option<u64>, fail: bool) -> Fixture {
    let api = Arc::new(api);
    let runs = Arc::new(AtomicUsize::new(0));
    let host = Arc::new(CommandHost::new(
        vec![Arc::new(FakeCommand {
            runs: runs.clone(),
            fail,
        })],
        CommandEnv::new(Arc::new(NoSnapshot)),
    ));
    let exclusions = ChannelExclusions::new(GUILD);
    Fixture {
        controller: Controller::new(api.clone(), host, master, GUILD, exclusions.clone()),
        api,
        runs,
        exclusions,
    }
}

fn slash(user_id: u64, guild_id: Option<u64>, sub: Option<&str>) -> Incoming {
    slash_named("noth", user_id, guild_id, sub)
}

fn slash_named(name: &str, user_id: u64, guild_id: Option<u64>, sub: Option<&str>) -> Incoming {
    Incoming {
        id: 77,
        token: "tok".into(),
        user_id,
        guild_id,
        kind: Kind::Slash {
            name: name.into(),
            sub: sub.map(Into::into),
        },
    }
}

fn button(user_id: u64, custom_id: &str) -> Incoming {
    Incoming {
        id: 77,
        token: "tok".into(),
        user_id,
        guild_id: Some(GUILD),
        kind: Kind::Button {
            custom_id: custom_id.into(),
        },
    }
}

fn is_v2(body: &Value) -> bool {
    body["flags"]
        .as_u64()
        .is_some_and(|f| f & FLAG_IS_COMPONENTS_V2 != 0)
}

fn is_embed(body: &Value) -> bool {
    body.get("embeds").is_some()
}

fn assert_denied(calls: &[Call]) {
    assert_eq!(
        calls,
        [Call::Callback(
            77,
            json!({"type": 4, "data": {
                "content": "権限がありません",
                "allowed_mentions": {"parse": []},
                "flags": 64,
            }})
        )]
    );
}

#[tokio::test]
async fn non_master_is_denied_without_running_the_command() {
    let f = fixture(FakeApi::default(), Some(MASTER), false);
    f.controller
        .handle(slash(2, Some(GUILD), Some("context")))
        .await;
    assert_denied(&f.api.calls());
    assert_eq!(f.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn nobody_can_run_it_when_master_is_unset() {
    let f = fixture(FakeApi::default(), None, false);
    f.controller
        .handle(slash(MASTER, Some(GUILD), Some("context")))
        .await;
    assert_denied(&f.api.calls());
    assert_eq!(f.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn other_guild_and_dm_are_denied() {
    let f = fixture(FakeApi::default(), Some(MASTER), false);
    f.controller
        .handle(slash(MASTER, Some(GUILD + 1), Some("context")))
        .await;
    f.controller
        .handle(slash(MASTER, None, Some("context")))
        .await;
    assert_eq!(f.api.calls().len(), 2);
    assert_eq!(f.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn non_master_cannot_press_refresh() {
    let f = fixture(FakeApi::default(), Some(MASTER), false);
    f.controller.handle(button(2, "noth:refresh:context")).await;
    assert_denied(&f.api.calls());
    assert_eq!(f.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn master_gets_deferred_ephemeral_then_v2_edit() {
    let f = fixture(FakeApi::default(), Some(MASTER), false);
    f.controller
        .handle(slash(MASTER, Some(GUILD), Some("context")))
        .await;
    let calls = f.api.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[0],
        Call::Callback(77, json!({"type": 5, "data": {"flags": 64}}))
    );
    let Call::Edit(body) = &calls[1] else {
        panic!("{calls:?}")
    };
    assert!(is_v2(body));
    assert_eq!(
        body["components"][0]["components"][2]["content"],
        "**全体**\n42%"
    );
    assert_eq!(f.runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn falls_back_from_v2_to_embed() {
    let api = FakeApi::default().rejecting(is_v2);
    let f = fixture(api, Some(MASTER), false);
    f.controller
        .handle(slash(MASTER, Some(GUILD), Some("context")))
        .await;
    let calls = f.api.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    let (Call::Edit(v2), Call::Edit(embed)) = (&calls[1], &calls[2]) else {
        panic!("{calls:?}")
    };
    assert!(is_v2(v2) && is_embed(embed));
    assert_eq!(embed["embeds"][0]["fields"][0]["name"], "全体");
}

#[tokio::test]
async fn falls_back_from_embed_to_text() {
    let api = FakeApi::default().rejecting(is_v2).rejecting(is_embed);
    let f = fixture(api, Some(MASTER), false);
    f.controller
        .handle(slash(MASTER, Some(GUILD), Some("context")))
        .await;
    let calls = f.api.calls();
    assert_eq!(calls.len(), 4, "{calls:?}");
    let Call::Edit(text) = &calls[3] else {
        panic!("{calls:?}")
    };
    assert!(text["content"].as_str().unwrap().contains("42%"));
}

#[tokio::test]
async fn command_failure_is_shown_in_the_deferred_reply() {
    let f = fixture(FakeApi::default(), Some(MASTER), true);
    f.controller
        .handle(slash(MASTER, Some(GUILD), Some("context")))
        .await;
    let calls = f.api.calls();
    let Call::Edit(body) = &calls[1] else {
        panic!("{calls:?}")
    };
    let text = body["content"].as_str().unwrap();
    assert!(text.contains("取得に失敗しました"));
    assert!(!text.contains("actor が停止"), "{text}");
}

#[tokio::test]
async fn refresh_failure_is_a_fixed_followup() {
    let f = fixture(FakeApi::default(), Some(MASTER), true);
    f.controller
        .handle(button(MASTER, "noth:refresh:context"))
        .await;
    let calls = f.api.calls();
    let Call::Followup(body) = &calls[1] else {
        panic!("{calls:?}")
    };
    let text = body["content"].as_str().unwrap();
    assert!(text.contains("更新に失敗しました"));
    assert!(!text.contains("actor が停止"), "{text}");
    assert_eq!(body["flags"], 64);
}

#[tokio::test]
async fn unknown_subcommand_is_rejected_ephemerally() {
    let f = fixture(FakeApi::default(), Some(MASTER), false);
    f.controller
        .handle(slash(MASTER, Some(GUILD), Some("nope")))
        .await;
    f.controller.handle(slash(MASTER, Some(GUILD), None)).await;
    f.controller
        .handle(button(MASTER, "noth:refresh:nope"))
        .await;
    f.controller
        .handle(slash_named("other", MASTER, Some(GUILD), Some("context")))
        .await;
    let calls = f.api.calls();
    assert_eq!(calls.len(), 4);
    for call in calls {
        let Call::Callback(_, body) = call else {
            panic!()
        };
        assert_eq!(body["type"], 4);
        assert_eq!(body["data"]["flags"], 64);
        assert!(
            body["data"]["content"]
                .as_str()
                .unwrap()
                .starts_with("未知の")
        );
    }
    assert_eq!(f.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn refresh_defers_update_then_edits_the_pressed_message() {
    let f = fixture(FakeApi::default(), Some(MASTER), false);
    f.controller
        .handle(button(MASTER, "noth:refresh:context"))
        .await;
    let calls = f.api.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0], Call::Callback(77, json!({"type": 6})));
    let Call::Edit(body) = &calls[1] else {
        panic!("{calls:?}")
    };
    assert!(is_v2(body));
}

#[tokio::test]
async fn refresh_falls_back_to_a_new_ephemeral_message() {
    let api = FakeApi::default().rejecting(is_v2);
    let f = fixture(api, Some(MASTER), false);
    f.controller
        .handle(button(MASTER, "noth:refresh:context"))
        .await;
    let calls = f.api.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    let Call::Followup(body) = &calls[2] else {
        panic!("{calls:?}")
    };
    assert!(is_embed(body));
    assert_eq!(body["flags"], 64);
}

#[tokio::test]
async fn sync_registers_noth_with_one_subcommand_per_command() {
    let f = fixture(FakeApi::default(), Some(MASTER), false);
    f.controller.sync().await;
    let calls = f.api.calls();
    let [Call::Sync(body)] = calls.as_slice() else {
        panic!()
    };
    assert_eq!(body[0]["name"], "noth");
    assert_eq!(body[0]["type"], 1);
    let options = body[0]["options"].as_array().unwrap();
    assert_eq!(options[0]["name"], "ignore");
    assert_eq!(options[0]["type"], 2);
    assert_eq!(
        options[1],
        json!({"type": 1, "name": "context", "description": "使用量を見る"})
    );
}

#[tokio::test]
async fn sync_clears_commands_when_master_is_unset() {
    let f = fixture(FakeApi::default(), None, false);
    f.controller.sync().await;
    assert_eq!(f.api.calls(), [Call::Sync(json!([]))]);
}

#[tokio::test]
async fn sync_registers_only_ignore_when_no_command_is_enabled() {
    let api = Arc::new(FakeApi::default());
    let host = Arc::new(CommandHost::new(
        Vec::new(),
        CommandEnv::new(Arc::new(NoSnapshot)),
    ));
    Controller::new(
        api.clone(),
        host,
        Some(MASTER),
        GUILD,
        ChannelExclusions::new(GUILD),
    )
    .sync()
    .await;
    let calls = api.calls();
    let [Call::Sync(body)] = calls.as_slice() else {
        panic!()
    };
    assert_eq!(body[0]["options"].as_array().unwrap().len(), 1);
    assert_eq!(body[0]["options"][0]["name"], "ignore");
}

fn ignore_cmd(user_id: u64, op: IgnoreOp, channel: Option<u64>) -> Incoming {
    Incoming {
        id: 77,
        token: "tok".into(),
        user_id,
        guild_id: Some(GUILD),
        kind: Kind::Ignore { op, channel },
    }
}

fn reply_text(call: &Call) -> String {
    let Call::Callback(_, body) = call else {
        panic!("{call:?}")
    };
    assert_eq!(body["type"], 4);
    assert_eq!(body["data"]["flags"], 64);
    body["data"]["content"].as_str().unwrap().to_string()
}

/// add / remove は先に ephemeral で defer し、結果で最初の応答を書き換える。`calls[at]` が defer、その次が結果
fn deferred_text(calls: &[Call], at: usize) -> String {
    let Call::Callback(_, defer) = &calls[at] else {
        panic!("{calls:?}")
    };
    assert_eq!(defer["type"], 5);
    assert_eq!(defer["data"]["flags"], 64);
    let Call::Edit(body) = &calls[at + 1] else {
        panic!("{calls:?}")
    };
    body["content"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn ignore_add_takes_effect_immediately_and_remove_undoes_it() {
    let f = fixture(FakeApi::default(), Some(MASTER), false);
    assert!(!f.exclusions.is_excluded(&[500]));
    f.controller
        .handle(ignore_cmd(MASTER, IgnoreOp::Add, Some(500)))
        .await;
    assert!(f.exclusions.is_excluded(&[500]));
    assert!(deferred_text(&f.api.calls(), 0).contains("<#500> を除外に追加しました"));

    f.controller
        .handle(ignore_cmd(MASTER, IgnoreOp::Remove, Some(500)))
        .await;
    assert!(!f.exclusions.is_excluded(&[500]));
    assert!(deferred_text(&f.api.calls(), 2).contains("<#500> を除外から外しました"));
}

#[tokio::test]
async fn ignore_add_twice_and_remove_unknown_are_reported() {
    let f = fixture(FakeApi::default(), Some(MASTER), false);
    for _ in 0..2 {
        f.controller
            .handle(ignore_cmd(MASTER, IgnoreOp::Add, Some(500)))
            .await;
    }
    f.controller
        .handle(ignore_cmd(MASTER, IgnoreOp::Remove, Some(501)))
        .await;
    let calls = f.api.calls();
    assert!(deferred_text(&calls, 2).contains("すでに除外されています"));
    assert!(deferred_text(&calls, 4).contains("除外されていません"));
}

#[tokio::test]
async fn ignore_requires_a_channel_for_add_and_remove() {
    let f = fixture(FakeApi::default(), Some(MASTER), false);
    f.controller
        .handle(ignore_cmd(MASTER, IgnoreOp::Add, None))
        .await;
    assert!(reply_text(&f.api.calls()[0]).contains("チャンネルを指定"));
    assert!(f.exclusions.is_empty());
}

#[tokio::test]
async fn config_entries_cannot_be_removed_by_command() {
    let f = fixture(FakeApi::default(), Some(MASTER), false);
    f.exclusions.set_config([700]);
    f.controller
        .handle(ignore_cmd(MASTER, IgnoreOp::Remove, Some(700)))
        .await;
    assert!(f.exclusions.is_excluded(&[700]));
    let text = deferred_text(&f.api.calls(), 0);
    assert!(
        text.contains("config") && text.contains("外せません"),
        "{text}"
    );
}

#[tokio::test]
async fn non_master_cannot_change_exclusions() {
    let f = fixture(FakeApi::default(), Some(MASTER), false);
    f.controller
        .handle(ignore_cmd(MASTER, IgnoreOp::Add, Some(500)))
        .await;
    for op in [IgnoreOp::Add, IgnoreOp::Remove, IgnoreOp::List] {
        f.controller
            .handle(ignore_cmd(MASTER + 1, op, Some(500)))
            .await;
    }
    f.controller
        .handle(ignore_cmd(MASTER + 1, IgnoreOp::Add, Some(600)))
        .await;
    let calls = f.api.calls();
    assert_eq!(calls.len(), 6, "{calls:?}");
    for call in &calls[2..] {
        assert_denied(std::slice::from_ref(call));
    }
    assert_eq!(f.exclusions.effective(), vec![500]);
}

#[tokio::test]
async fn master_unset_or_other_guild_cannot_use_ignore() {
    let f = fixture(FakeApi::default(), None, false);
    f.controller
        .handle(ignore_cmd(MASTER, IgnoreOp::Add, Some(500)))
        .await;
    assert_denied(&f.api.calls());
    assert!(f.exclusions.is_empty());

    let f = fixture(FakeApi::default(), Some(MASTER), false);
    let mut other = ignore_cmd(MASTER, IgnoreOp::Add, Some(500));
    other.guild_id = Some(GUILD + 1);
    f.controller.handle(other).await;
    assert_denied(&f.api.calls());
    assert!(f.exclusions.is_empty());
}

#[tokio::test]
async fn ignore_list_shows_origin_ephemerally() {
    let f = fixture(FakeApi::default(), Some(MASTER), false);
    f.exclusions.set_config([700]);
    f.controller
        .handle(ignore_cmd(MASTER, IgnoreOp::Add, Some(500)))
        .await;
    f.controller
        .handle(ignore_cmd(MASTER, IgnoreOp::List, None))
        .await;
    let calls = f.api.calls();
    assert_eq!(calls.len(), 4, "{calls:?}");
    let Call::Callback(_, defer) = &calls[2] else {
        panic!()
    };
    assert_eq!(defer["type"], 5);
    assert_eq!(defer["data"]["flags"], 64);
    let Call::Edit(body) = &calls[3] else {
        panic!("{calls:?}")
    };
    assert!(is_v2(body));
    let text = body.to_string();
    let config_at = text.find("config 由来").unwrap();
    let command_at = text.find("コマンド由来").unwrap();
    assert!(text.find("<#700>").unwrap() > config_at);
    assert!(text.find("<#500>").unwrap() > command_at);
    // 一覧には更新ボタンを付けない
    assert!(!text.contains("noth:refresh"));
}

#[tokio::test]
async fn exclusions_added_by_command_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("noth-discord-ignore-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("noth.db");

    let f = fixture(FakeApi::default(), Some(MASTER), false);
    f.exclusions
        .attach(noth_core::store::Store::open(&path).await.unwrap())
        .await
        .unwrap();
    f.controller
        .handle(ignore_cmd(MASTER, IgnoreOp::Add, Some(500)))
        .await;

    let restarted = ChannelExclusions::new(GUILD);
    restarted
        .attach(noth_core::store::Store::open(&path).await.unwrap())
        .await
        .unwrap();
    assert!(restarted.is_excluded(&[500]));
    let _ = std::fs::remove_dir_all(&dir);
}

mod rest {
    use std::sync::Mutex;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::command::api::RestInteractionApi;

    async fn serve(statuses: Vec<&'static str>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for status in statuses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 16384];
                let n = sock.read(&mut buf).await.unwrap();
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
                let body = if status.starts_with("429") {
                    r#"{"message":"slow","retry_after":0.01}"#
                } else if status.starts_with('4') {
                    r#"{"message":"Invalid Form Body"}"#
                } else {
                    ""
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (base, seen)
    }

    #[tokio::test]
    async fn endpoints_methods_and_auth() {
        let (base, seen) = serve(vec![
            "200 OK",
            "204 No Content",
            "200 OK",
            "200 OK",
            "400 Bad Request",
        ])
        .await;
        let api = RestInteractionApi::with_base("bot-tok".into(), 5, 10, base);
        api.sync_commands(&json!([])).await.unwrap();
        api.callback(77, "itok", &json!({"type": 5})).await.unwrap();
        api.edit_original("itok", &json!({"flags": 32768}))
            .await
            .unwrap();
        api.followup("itok", &json!({"content": "x"}))
            .await
            .unwrap();
        let e = api.edit_original("itok", &json!({})).await.unwrap_err();
        assert_eq!(e.status, 400);
        assert!(e.body.contains("Invalid Form Body"));

        let seen = seen.lock().unwrap();
        assert!(
            seen[0].starts_with("PUT /applications/5/guilds/10/commands "),
            "{}",
            seen[0]
        );
        assert!(
            seen[0]
                .to_lowercase()
                .contains("authorization: bot bot-tok")
        );
        assert!(
            seen[1].starts_with("POST /interactions/77/itok/callback "),
            "{}",
            seen[1]
        );
        assert!(!seen[1].to_lowercase().contains("authorization"));
        assert!(
            seen[2].starts_with("PATCH /webhooks/5/itok/messages/@original "),
            "{}",
            seen[2]
        );
        assert!(seen[2].contains(r#"{"flags":32768}"#));
        assert!(seen[3].starts_with("POST /webhooks/5/itok "), "{}", seen[3]);
    }

    #[tokio::test]
    async fn sync_retries_once_after_a_short_rate_limit() {
        let (base, seen) = serve(vec!["429 Too Many Requests", "200 OK"]).await;
        let api = RestInteractionApi::with_base("bot-tok".into(), 5, 10, base);
        api.sync_commands(&json!([])).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sync_does_not_retry_a_second_rate_limit_and_callbacks_never_retry() {
        let (base, seen) = serve(vec![
            "429 Too Many Requests",
            "429 Too Many Requests",
            "429 Too Many Requests",
        ])
        .await;
        let api = RestInteractionApi::with_base("bot-tok".into(), 5, 10, base);
        let e = api.sync_commands(&json!([])).await.unwrap_err();
        assert_eq!((e.status, e.retry_after), (429, Some(0.01)));
        assert!(api.callback(77, "itok", &json!({})).await.is_err());
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn connection_error_does_not_leak_the_interaction_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let api = RestInteractionApi::with_base("bot-tok".into(), 5, 10, base);
        let secret = "SECRET-INTERACTION-TOKEN";
        for e in [
            api.callback(77, secret, &json!({})).await.unwrap_err(),
            api.edit_original(secret, &json!({})).await.unwrap_err(),
            api.followup(secret, &json!({})).await.unwrap_err(),
        ] {
            assert_eq!(e.status, 0);
            assert!(!format!("{e}").contains(secret), "{e}");
        }
    }
}

struct Named(&'static str);

#[async_trait]
impl Command for Named {
    fn name(&self) -> &'static str {
        self.0
    }
    fn description(&self) -> &'static str {
        "x"
    }
    async fn run(&self, _env: &CommandEnv) -> Result<Report> {
        unreachable!()
    }
}

#[tokio::test]
async fn duplicate_subcommand_names_are_registered_once() {
    let api = Arc::new(FakeApi::default());
    let host = Arc::new(CommandHost::new(
        vec![
            Arc::new(Named("context")),
            Arc::new(Named("context")),
            Arc::new(Named("ignore")),
        ],
        CommandEnv::new(Arc::new(NoSnapshot)),
    ));
    Controller::new(
        api.clone(),
        host,
        Some(MASTER),
        GUILD,
        ChannelExclusions::new(GUILD),
    )
    .sync()
    .await;
    let calls = api.calls();
    let [Call::Sync(body)] = calls.as_slice() else {
        panic!()
    };
    let names: Vec<&str> = body[0]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["ignore", "context"]);
    // command module の同名のものではなく、組み込みの ignore グループ(type 2)が残る
    assert_eq!(body[0]["options"][0]["type"], 2);
    assert_eq!(body[0]["options"][1]["type"], 1);
}
