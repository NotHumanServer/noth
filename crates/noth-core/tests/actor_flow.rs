use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use noth_core::actor::{Actor, Job};
use noth_core::bus::{self, BusEvent, BusHandle};
use noth_core::config::*;
use noth_core::judge::{JudgeBatch, PendingMessage};
use noth_core::registry::Modules;
use noth_core::store::Store;
use noth_core::traits::*;
use noth_core::types::*;
use noth_core::{Error, Result};

const GUILD: GuildId = 1;
const GENERAL: ChannelId = 100;
const RANDOM: ChannelId = 200;

#[derive(Default)]
struct MockChannel {
    sent: Mutex<Vec<(ChannelId, String)>>,
    scripted: Mutex<Vec<IncomingMessage>>,
    typing: Mutex<Vec<ChannelId>>,
}

#[async_trait]
impl Channel for MockChannel {
    fn guild_id(&self) -> GuildId {
        GUILD
    }
    async fn run(&self, bus: BusHandle) -> Result<()> {
        let msgs: Vec<_> = self.scripted.lock().unwrap().drain(..).collect();
        for m in msgs {
            bus.publish(BusEvent::MessageReceived(m)).await?;
        }
        std::future::pending().await
    }
    async fn send(
        &self,
        channel_id: ChannelId,
        text: &str,
        _reply_to: Option<MessageId>,
    ) -> Result<MessageId> {
        self.sent
            .lock()
            .unwrap()
            .push((channel_id, text.to_string()));
        Ok(1)
    }
    async fn typing(&self, channel_id: ChannelId) -> Result<()> {
        self.typing.lock().unwrap().push(channel_id);
        Ok(())
    }
    async fn members(&self) -> Result<Vec<Member>> {
        Ok(vec![Member {
            user_id: 1,
            display_name: "alice".into(),
            roles: vec!["admin".into()],
        }])
    }
    async fn health_check(&self) -> Result<String> {
        Ok("ok".into())
    }
}

#[derive(Default)]
struct MockRouter {
    answers: Mutex<HashMap<String, Answer>>,
    fail: AtomicBool,
}

impl MockRouter {
    fn set(&self, answers: Vec<(&str, Answer)>) {
        *self.answers.lock().unwrap() = answers
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
    }
}

#[async_trait]
impl Router for MockRouter {
    async fn decide(
        &self,
        _state: &RouterState,
        _questions: &[(String, Question)],
    ) -> Result<Decision> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(Error::Api {
                status: 500,
                body: "boom".into(),
            });
        }
        Ok(Decision {
            answers: self.answers.lock().unwrap().clone(),
            latency: Duration::from_millis(1),
        })
    }
    fn max_questions(&self) -> usize {
        64
    }
    fn model_label(&self) -> String {
        "mock".into()
    }
    async fn health_check(&self) -> Result<String> {
        Ok("ok".into())
    }
}

struct MockProvider {
    prompts: Mutex<Vec<LayeredPrompt>>,
    fail: AtomicBool,
    input_tokens: Mutex<u64>,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            prompts: Mutex::default(),
            fail: AtomicBool::new(false),
            input_tokens: Mutex::new(10),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        prompt: &LayeredPrompt,
        _params: &CompletionParams,
    ) -> Result<Completion> {
        self.prompts.lock().unwrap().push(prompt.clone());
        if self.fail.load(Ordering::SeqCst) {
            return Err(Error::Api {
                status: 500,
                body: "boom".into(),
            });
        }
        let n = self.prompts.lock().unwrap().len();
        Ok(Completion {
            text: Some(format!("応答{n}")),
            tool_calls: vec![],
            usage: Usage {
                input_tokens: *self.input_tokens.lock().unwrap(),
                ..Default::default()
            },
        })
    }
    async fn health_check(&self) -> Result<String> {
        Ok("ok".into())
    }
}

fn noul(p: f32) -> Answer {
    Answer::Noul { p }
}

fn choice(name: &str, options: &[&str], confidence: f32) -> Answer {
    let probabilities = options
        .iter()
        .map(|o| (o.to_string(), if *o == name { 0.9 } else { 0.05 }))
        .collect();
    Answer::Choice {
        choice: name.into(),
        probabilities,
        confidence,
    }
}

fn action(name: &str) -> Answer {
    choice(name, &["silent", "reply_only", "spawn_task"], 1.0)
}

fn category(name: &str) -> Answer {
    choice(
        name,
        &["none", "web_research", "fact_check", "code_or_tool"],
        1.0,
    )
}

fn answers(action_name: &str, noise: f32, shift: f32) -> Vec<(&'static str, Answer)> {
    vec![
        ("is_noise_or_meme", noul(noise)),
        ("is_topic_shift", noul(shift)),
        ("intervention_action", action(action_name)),
        ("task_category", category("none")),
    ]
}

fn config(hard_limit: u64, table: toml::Table) -> Config {
    Config {
        paths: Paths {
            home: PathBuf::from("/nonexistent/noth-test"),
            config: PathBuf::new(),
            env: PathBuf::new(),
        },
        modules: ModulesSection { enabled: vec![] },
        profile: ProfileSection {
            name: "default".into(),
        },
        context: ContextSection {
            prompt_hard_limit_tokens: hard_limit,
            retry_max: 3,
            max_tokens: 256,
        },
        router: RouterSection {
            recent_messages: 30,
            state_max_chars: 24_000,
            candidate_chunks: 32,
            call_names: vec!["noth".into()],
            conversation_window_secs: 120,
            thresholds: Thresholds::default(),
        },
        table,
        env: Default::default(),
    }
}

struct Harness {
    actor: Actor,
    store: Store,
    channel: Arc<MockChannel>,
    router: Arc<MockRouter>,
    provider: Arc<MockProvider>,
}

impl Harness {
    async fn new() -> Self {
        Self::with_limit(80_000).await
    }

    async fn with_limit(hard_limit: u64) -> Self {
        let store = Store::open_memory().await.unwrap();
        let channel = Arc::new(MockChannel::default());
        let router = Arc::new(MockRouter::default());
        let provider = Arc::new(MockProvider::new());
        let modules = Modules {
            channel: channel.clone(),
            router: router.clone(),
            provider: provider.clone(),
            worker_enabled: false,
            compactor_enabled: false,
        };
        let actor = Actor::new(
            config(hard_limit, toml::Table::new()),
            modules,
            store.clone(),
            &channel.members().await.unwrap(),
        )
        .await
        .unwrap();
        Self {
            actor,
            store,
            channel,
            router,
            provider,
        }
    }

    async fn say(&mut self, channel_id: ChannelId, content: &str) {
        self.say_as(channel_id, content, false).await;
    }

    async fn say_as(&mut self, channel_id: ChannelId, content: &str, mentions_self: bool) {
        let name = if channel_id == GENERAL {
            "general"
        } else {
            "random"
        };
        let mut msg = incoming(channel_id, name, content, 0);
        msg.mentions_self = mentions_self;
        let row_id = self.store.insert_message(&msg).await.unwrap();
        let batch = JudgeBatch {
            channel_id,
            channel_name: name.into(),
            messages: vec![PendingMessage {
                row_id,
                author_name: "alice".into(),
                content: content.into(),
                at: msg.timestamp,
                mentions_self,
                reply_to_self: false,
            }],
        };
        self.actor.handle_judge(batch).await.unwrap();
    }

    fn sent(&self) -> Vec<(ChannelId, String)> {
        self.channel.sent.lock().unwrap().clone()
    }

    fn prompts(&self) -> Vec<LayeredPrompt> {
        self.provider.prompts.lock().unwrap().clone()
    }
}

fn incoming(
    channel_id: ChannelId,
    name: &str,
    content: &str,
    message_id: MessageId,
) -> IncomingMessage {
    IncomingMessage {
        guild_id: GUILD,
        channel_id,
        channel_name: name.into(),
        message_id,
        user_id: 1,
        author_name: "alice".into(),
        content: content.into(),
        timestamp: Timestamp::now(),
        is_self: false,
        mentions_self: false,
        reply_to_self: false,
    }
}

#[tokio::test]
async fn silent_buffers_then_reply_promotes_one_block() {
    let mut h = Harness::new().await;
    h.router.set(answers("silent", 0.0, 0.0));
    h.say(GENERAL, "昨日の映画よかった").await;
    h.say(GENERAL, "続編も出るらしい").await;
    assert!(h.sent().is_empty());
    assert_eq!(h.store.buffered_messages(GUILD).await.unwrap().len(), 2);
    assert!(h.store.active_turns(GUILD).await.unwrap().is_empty());

    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "Noth はどう思う?").await;

    assert_eq!(h.sent(), vec![(GENERAL, "応答1".to_string())]);
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0].kind, TurnKind::Buffer);
    assert!(
        turns[0].text.contains("昨日の映画よかった") && turns[0].text.contains("続編も出るらしい")
    );
    assert_eq!(turns[1].kind, TurnKind::Trigger);
    assert_eq!(turns[2].kind, TurnKind::Response);
    assert_eq!(turns[2].text, "応答1");
    assert!(h.store.buffered_messages(GUILD).await.unwrap().is_empty());

    let prompt = &h.prompts()[0];
    assert_eq!(prompt.active_log, vec![turns[0].clone()]);
    let last = prompt.ephemeral_tail.last().unwrap();
    assert_eq!(last.text, turns[1].text);
    assert!(prompt.tools.is_empty());
    assert!(!prompt.harness_rules.contains("Goal"));
    assert!(prompt.profile.contains("alice"));
}

#[tokio::test]
async fn second_turn_prefix_is_append_only() {
    let mut h = Harness::new().await;
    h.router.set(answers("silent", 0.0, 0.0));
    h.say(GENERAL, "雑談").await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "質問1").await;
    h.say(GENERAL, "質問2").await;

    let prompts = h.prompts();
    let (p1, p2) = (&prompts[0], &prompts[1]);
    assert_eq!(p1.harness_rules, p2.harness_rules);
    assert_eq!(p1.profile, p2.profile);
    assert_eq!(p1.episodic_digest, p2.episodic_digest);
    assert_eq!(p2.active_log.len(), p1.active_log.len() + 2);
    assert_eq!(&p2.active_log[..p1.active_log.len()], &p1.active_log[..]);
    assert_eq!(p2.active_log[p1.active_log.len()].kind, TurnKind::Trigger);
    assert_eq!(p2.active_log.last().unwrap().kind, TurnKind::Response);
}

#[tokio::test]
async fn provider_error_leaves_trigger_in_buffer() {
    let mut h = Harness::new().await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.provider.fail.store(true, Ordering::SeqCst);
    h.say(GENERAL, "Noth いる?").await;

    assert!(h.sent().is_empty());
    assert!(h.store.active_turns(GUILD).await.unwrap().is_empty());
    assert_eq!(h.store.buffered_messages(GUILD).await.unwrap().len(), 1);

    h.provider.fail.store(false, Ordering::SeqCst);
    h.say(GENERAL, "もう一度").await;
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns[0].kind, TurnKind::Buffer);
    assert!(turns[0].text.contains("Noth いる?"));
    assert!(h.store.buffered_messages(GUILD).await.unwrap().is_empty());
}

#[tokio::test]
async fn noise_never_enters_buffer() {
    let mut h = Harness::new().await;
    h.router.set(answers("silent", 0.95, 0.0));
    h.say(GENERAL, "wwwwwww").await;
    assert!(h.store.buffered_messages(GUILD).await.unwrap().is_empty());

    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "Noth").await;
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert!(turns.iter().all(|t| !t.text.contains("wwwwwww")));
}

#[tokio::test]
async fn spawn_task_runs_as_reply_only_without_worker() {
    let mut h = Harness::new().await;
    let mut a = answers("spawn_task", 0.0, 0.0);
    a.retain(|(k, _)| *k != "task_category");
    a.push(("task_category", category("web_research")));
    h.router.set(a);
    h.say(GENERAL, "これ調べて").await;

    assert_eq!(h.sent().len(), 1);
    assert!(h.prompts()[0].tools.is_empty());
}

#[tokio::test]
async fn low_confidence_is_silent() {
    let mut h = Harness::new().await;
    let mut a = answers("reply_only", 0.0, 0.0);
    a.retain(|(k, _)| *k != "intervention_action");
    a.push((
        "intervention_action",
        choice("reply_only", &["silent", "reply_only", "spawn_task"], 0.05),
    ));
    h.router.set(a);
    h.say(GENERAL, "どうかな").await;

    assert!(h.sent().is_empty());
    assert_eq!(h.store.buffered_messages(GUILD).await.unwrap().len(), 1);
}

#[tokio::test]
async fn name_call_ignores_min_confidence_but_not_router_choice() {
    let mut h = Harness::new().await;
    let low = |name: &str| {
        let mut a = answers(name, 0.0, 0.0);
        a.retain(|(k, _)| *k != "intervention_action");
        a.push((
            "intervention_action",
            choice(name, &["silent", "reply_only", "spawn_task"], 0.01),
        ));
        a
    };
    h.router.set(low("silent"));
    h.say(GENERAL, "Nothの実装どうする").await;
    assert!(h.sent().is_empty());

    h.router.set(low("reply_only"));
    h.say(GENERAL, "nothing").await;
    assert!(h.sent().is_empty());

    h.router.set(low("reply_only"));
    h.say(GENERAL, "noth〜？").await;
    assert_eq!(h.sent().len(), 1);
}

#[tokio::test]
async fn mention_is_answered_even_if_router_says_silent_noise() {
    let mut h = Harness::new().await;
    h.router.set(answers("silent", 0.9, 0.0));
    h.say_as(GENERAL, "@Noth", true).await;

    assert_eq!(h.sent().len(), 1);
    assert!(h.store.buffered_messages(GUILD).await.unwrap().is_empty());
}

#[tokio::test]
async fn relevant_chunk_goes_to_layer4_before_trigger() {
    let mut h = Harness::new().await;
    h.router.set(answers("silent", 0.0, 0.0));
    h.say(RANDOM, "旅行の計画を立てよう").await;
    // 別の話題に切り替えて、旅行のチャンクを Layer 3 にも現在チャンクにも残らない候補にする
    h.router.set(answers("silent", 0.0, 0.9));
    h.say(RANDOM, "ところで晩ごはん").await;
    h.say(GENERAL, "別チャンネルの話").await;
    let chunks = h.store.candidate_chunks(GUILD, 10, &[]).await.unwrap();
    let travel = chunks
        .iter()
        .find(|c| c.summary.contains("旅行"))
        .unwrap()
        .id;
    // 圧縮済み相当にして、バッファにも Layer 3 にも残らない状態にする
    let travel_rows: Vec<i64> = h
        .store
        .chunk_messages(travel)
        .await
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    h.store.mark_promoted(&travel_rows, 9_999).await.unwrap();

    let mut a = answers("reply_only", 0.0, 0.0);
    a.push((
        Box::leak(format!("relevant_chunk_{travel}").into_boxed_str()),
        noul(0.9),
    ));
    h.router.set(a);
    h.say(GENERAL, "旅行の話どうなった?").await;

    let tail = &h.prompts()[0].ephemeral_tail;
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].kind, TurnKind::RelevantChunk);
    assert!(tail[0].text.contains("旅行の計画を立てよう"));
    assert_eq!(tail[1].kind, TurnKind::Trigger);
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert!(turns.iter().all(|t| t.kind != TurnKind::RelevantChunk));
}

#[tokio::test]
async fn buffered_chunk_is_not_offered_as_relevant_chunk() {
    let mut h = Harness::new().await;
    h.router.set(answers("silent", 0.0, 0.0));
    h.say(RANDOM, "旅行の計画を立てよう").await;
    h.router.set(answers("silent", 0.0, 0.9));
    h.say(RANDOM, "ところで晩ごはん").await;
    let travel = h
        .store
        .candidate_chunks(GUILD, 10, &[])
        .await
        .unwrap()
        .iter()
        .find(|c| c.summary.contains("旅行"))
        .unwrap()
        .id;

    let mut a = answers("reply_only", 0.0, 0.0);
    a.push((
        Box::leak(format!("relevant_chunk_{travel}").into_boxed_str()),
        noul(0.9),
    ));
    h.router.set(a);
    h.say(GENERAL, "旅行の話どうなった?").await;

    let prompt = &h.prompts()[0];
    assert!(
        prompt
            .ephemeral_tail
            .iter()
            .all(|t| t.kind != TurnKind::RelevantChunk)
    );
    for tail in &prompt.ephemeral_tail {
        for log in &prompt.active_log {
            assert!(
                !(tail.text.contains("旅行の計画を立てよう")
                    && log.text.contains("旅行の計画を立てよう"))
            );
        }
    }
    assert!(
        prompt
            .active_log
            .iter()
            .any(|t| t.text.contains("旅行の計画を立てよう"))
    );
}

fn silent_with_reply_p(reply: f32) -> Vec<(&'static str, Answer)> {
    let mut a = answers("silent", 0.0, 0.0);
    a.retain(|(k, _)| *k != "intervention_action");
    a.push((
        "intervention_action",
        Answer::Choice {
            choice: "silent".into(),
            probabilities: [
                ("silent".to_string(), 1.0 - reply),
                ("reply_only".to_string(), reply),
                ("spawn_task".to_string(), 0.0),
            ]
            .into(),
            confidence: 0.3,
        },
    ));
    a
}

#[tokio::test]
async fn follow_up_after_reply_counts_as_name_call() {
    let mut h = Harness::new().await;
    h.router.set(silent_with_reply_p(0.44));
    h.say(GENERAL, "ココナッツ食べたことない").await;
    assert!(h.sent().is_empty(), "応答前の会話では静観のまま");

    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say_as(GENERAL, "@Noth", true).await;
    assert_eq!(h.sent().len(), 1);

    h.router.set(silent_with_reply_p(0.44));
    h.say(GENERAL, "ココナッツ食べたことない").await;
    assert_eq!(h.sent().len(), 2);

    h.router.set(silent_with_reply_p(0.1));
    h.say(GENERAL, "w").await;
    assert_eq!(h.sent().len(), 2);

    h.router.set(silent_with_reply_p(0.44));
    h.say(RANDOM, "別チャンネル").await;
    assert_eq!(h.sent().len(), 2);
}

#[tokio::test]
async fn noise_does_not_extend_summary_or_open_chunk() {
    let mut h = Harness::new().await;
    h.router.set(answers("silent", 0.0, 0.0));
    h.say(GENERAL, "本題").await;
    let before = h
        .store
        .current_chunk(GUILD, GENERAL)
        .await
        .unwrap()
        .unwrap();

    h.router.set(answers("silent", 0.9, 0.9));
    h.say(GENERAL, "wwww").await;
    let after = h
        .store
        .current_chunk(GUILD, GENERAL)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.id, before.id);
    assert_eq!(after.summary, before.summary);
}

#[tokio::test]
async fn hard_limit_drops_oldest_chunk_only() {
    let mut h = Harness::with_limit(100).await;
    *h.provider.input_tokens.lock().unwrap() = 1_000;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "話題A").await;
    assert_eq!(h.store.active_turns(GUILD).await.unwrap().len(), 2);

    h.router.set(answers("reply_only", 0.0, 0.9));
    h.say(GENERAL, "話題B").await;
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns.len(), 2);
    assert!(turns[0].text.contains("話題B"));
}

#[tokio::test]
async fn router_failure_still_answers_when_called_by_name() {
    let mut h = Harness::new().await;
    h.router.fail.store(true, Ordering::SeqCst);

    h.say(GENERAL, "今日は寒いね").await;
    assert!(h.sent().is_empty());

    h.say(GENERAL, "Noth はどう思う?").await;
    assert_eq!(h.sent(), vec![(GENERAL, "応答1".to_string())]);
}

#[tokio::test]
async fn ingest_sends_typing_immediately_for_direct_calls_only() {
    let store = Store::open_memory().await.unwrap();
    let channel = Arc::new(MockChannel::default());
    let (bus, rx) = bus::channel(16);
    let (jobs_tx, _jobs_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    // デバウンスを長くして、typing がデバウンス前に出ることを確かめる
    tokio::spawn(noth_core::ingest::run(
        store,
        channel.clone(),
        rx,
        Duration::from_secs(30),
        jobs_tx,
    ));

    bus.publish(BusEvent::MessageReceived(incoming(
        GENERAL,
        "general",
        "ただの発言",
        1,
    )))
    .await
    .unwrap();
    let mut mention = incoming(RANDOM, "random", "@Noth", 2);
    mention.mentions_self = true;
    bus.publish(BusEvent::MessageReceived(mention))
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(3), async {
        while channel.typing.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*channel.typing.lock().unwrap(), vec![RANDOM]);
}

#[tokio::test]
async fn ingest_debounces_per_channel_and_skips_self() {
    let store = Store::open_memory().await.unwrap();
    let (bus, rx) = bus::channel(16);
    let (jobs_tx, mut jobs_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    tokio::spawn(noth_core::ingest::run(
        store.clone(),
        Arc::new(MockChannel::default()),
        rx,
        Duration::from_millis(100),
        jobs_tx,
    ));

    let mut own = incoming(GENERAL, "general", "Noth の発言", 3);
    own.is_self = true;
    for m in [
        incoming(GENERAL, "general", "一", 1),
        incoming(RANDOM, "random", "別", 2),
        own,
        incoming(GENERAL, "general", "二", 4),
    ] {
        bus.publish(BusEvent::MessageReceived(m)).await.unwrap();
    }

    let mut batches = Vec::new();
    for _ in 0..2 {
        let Job::Judge(b) = tokio::time::timeout(Duration::from_secs(3), jobs_rx.recv())
            .await
            .unwrap()
            .unwrap();
        batches.push(b);
    }
    let general = batches.iter().find(|b| b.channel_id == GENERAL).unwrap();
    assert_eq!(
        general
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec!["一", "二"]
    );
    assert_eq!(
        batches
            .iter()
            .find(|b| b.channel_id == RANDOM)
            .unwrap()
            .messages
            .len(),
        1
    );
    // 自分の発言も含めて保存はされ、判定対象にはならない
    assert_eq!(
        store
            .recent_messages(GUILD, i64::MAX, 10)
            .await
            .unwrap()
            .len(),
        4
    );
    assert_eq!(store.unjudged_messages(GUILD).await.unwrap().len(), 3);
}

#[tokio::test]
async fn restore_groups_unjudged_by_channel() {
    let store = Store::open_memory().await.unwrap();
    for (c, n, text, id) in [
        (GENERAL, "general", "a", 1),
        (RANDOM, "random", "b", 2),
        (GENERAL, "general", "c", 3),
    ] {
        store
            .insert_message(&incoming(c, n, text, id))
            .await
            .unwrap();
    }
    let batches = noth_core::restore::restore(&store, GUILD).await.unwrap();
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0].channel_id, GENERAL);
    assert_eq!(batches[0].messages.len(), 2);
    assert_eq!(batches[1].channel_id, RANDOM);
}

#[tokio::test]
async fn run_replies_end_to_end_and_marks_disconnected() {
    let store = Store::open_memory().await.unwrap();
    let channel = Arc::new(MockChannel::default());
    channel
        .scripted
        .lock()
        .unwrap()
        .push(incoming(GENERAL, "general", "Noth、いる?", 1));
    let router = Arc::new(MockRouter::default());
    router.set(answers("reply_only", 0.0, 0.0));
    let provider = Arc::new(MockProvider::new());
    let modules = Modules {
        channel: channel.clone(),
        router: router.clone(),
        provider: provider.clone(),
        worker_enabled: false,
        compactor_enabled: false,
    };
    let table: toml::Table = toml::from_str("[channel.discord]\ndebounce_secs = 0.05").unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(noth_core::actor::run(
        config(80_000, table),
        modules,
        store.clone(),
        async move {
            let _ = stop_rx.await;
        },
    ));

    for _ in 0..100 {
        if !channel.sent.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(channel.sent.lock().unwrap().len(), 1);
    stop_tx.send(()).unwrap();
    handle.await.unwrap().unwrap();
    assert_eq!(
        store.read_heartbeat().await.unwrap().unwrap().state,
        "disconnected"
    );
}
