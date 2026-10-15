use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use noth_core::actor::{Actor, Job};
use noth_core::bus::{self, BusEvent, BusHandle};
use noth_core::config::*;
use noth_core::judge::{JudgeBatch, PendingMessage};
use noth_core::metering::MeteredProvider;
use noth_core::registry::Modules;
use noth_core::store::{CostGroup, NewWorkerRun, RunStatus, Store};
use noth_core::traits::*;
use noth_core::types::*;
use noth_core::{Error, Result};

const GUILD: GuildId = 1;
const GENERAL: ChannelId = 100;
const RANDOM: ChannelId = 200;

#[derive(Default)]
struct MockChannel {
    sent: Mutex<Vec<(ChannelId, String)>>,
    /// sent と同じ順の reply_to
    reply_to: Mutex<Vec<Option<MessageId>>>,
    scripted: Mutex<Vec<IncomingMessage>>,
    typing: Mutex<Vec<ChannelId>>,
    edits: Mutex<Vec<(ChannelId, MessageId, String, tokio::time::Instant)>>,
    edit_fail: AtomicBool,
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
        reply_to: Option<MessageId>,
    ) -> Result<MessageId> {
        self.reply_to.lock().unwrap().push(reply_to);
        let mut sent = self.sent.lock().unwrap();
        sent.push((channel_id, text.to_string()));
        Ok(sent.len() as MessageId)
    }
    async fn edit(&self, channel_id: ChannelId, message_id: MessageId, text: &str) -> Result<()> {
        if self.edit_fail.load(Ordering::SeqCst) {
            return Err(Error::Other("edit できない".into()));
        }
        self.edits.lock().unwrap().push((
            channel_id,
            message_id,
            text.to_string(),
            tokio::time::Instant::now(),
        ));
        Ok(())
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
    /// 空になるまで先頭から返す。空なら「応答<n>」
    scripted: Mutex<VecDeque<Completion>>,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            prompts: Mutex::default(),
            fail: AtomicBool::new(false),
            input_tokens: Mutex::new(10),
            scripted: Mutex::default(),
        }
    }

    fn script_text(&self, text: &str) {
        self.scripted.lock().unwrap().push_back(Completion {
            text: Some(text.into()),
            ..Default::default()
        });
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
        let usage = Usage {
            input_tokens: *self.input_tokens.lock().unwrap(),
            ..Default::default()
        };
        if let Some(mut c) = self.scripted.lock().unwrap().pop_front() {
            c.usage = usage;
            return Ok(c);
        }
        let n = self.prompts.lock().unwrap().len();
        Ok(Completion {
            text: Some(format!("応答{n}")),
            tool_calls: vec![],
            usage: Usage {
                input_tokens: *self.input_tokens.lock().unwrap(),
                ..Default::default()
            },
            ..Default::default()
        })
    }
    async fn health_check(&self) -> Result<String> {
        Ok("ok".into())
    }
}

struct MockWorker {
    tasks: Mutex<Vec<WorkerTask>>,
    /// run は許可を 1 つ取ってから終わる。0 にしておけばテスト側が離すまで止まる
    release: tokio::sync::Semaphore,
    delay: Mutex<Duration>,
    fail: AtomicBool,
    /// run の冒頭で、各待ち時間の後に進捗を送る
    progress: Mutex<Vec<(Duration, WorkerProgress)>>,
}

impl MockWorker {
    fn new(permits: usize) -> Self {
        Self {
            tasks: Mutex::default(),
            release: tokio::sync::Semaphore::new(permits),
            delay: Mutex::new(Duration::ZERO),
            fail: AtomicBool::new(false),
            progress: Mutex::default(),
        }
    }

    fn specs() -> Vec<ToolSpec> {
        ["web_search", "shell"]
            .iter()
            .map(|n| ToolSpec {
                name: n.to_string(),
                description: format!("{n} の説明"),
                input_schema: serde_json::json!({"type": "object"}),
            })
            .collect()
    }

    fn started(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }
}

#[async_trait]
impl Worker for MockWorker {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        Self::specs()
    }
    async fn run(&self, task: WorkerTask) -> WorkerOutcome {
        let goal = task.goal.clone();
        let tx = task.progress.clone();
        self.tasks.lock().unwrap().push(task);
        let steps = self.progress.lock().unwrap().clone();
        for (wait, p) in steps {
            tokio::time::sleep(wait).await;
            if let Some(tx) = &tx {
                let _ = tx.send(p);
            }
        }
        self.release.acquire().await.unwrap().forget();
        let delay = *self.delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        if self.fail.load(Ordering::SeqCst) {
            return WorkerOutcome::Failed("API エラー".into());
        }
        WorkerOutcome::Concluded(format!("結論: {goal}"))
    }
}

#[derive(Default)]
struct MockCompactor {
    calls: Mutex<Vec<(String, Vec<Turn>)>>,
    fail: AtomicBool,
}

#[async_trait]
impl Compactor for MockCompactor {
    async fn digest(&self, harness_rules: &str, turns: &[Turn]) -> Result<String> {
        self.calls
            .lock()
            .unwrap()
            .push((harness_rules.to_string(), turns.to_vec()));
        if self.fail.load(Ordering::SeqCst) {
            return Err(Error::Other("boom".into()));
        }
        Ok(format!("要約{}", self.calls.lock().unwrap().len()))
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

static HOME_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_home() -> PathBuf {
    std::env::temp_dir().join(format!(
        "noth-actor-flow-{}-{}",
        std::process::id(),
        HOME_COUNTER.fetch_add(1, Ordering::SeqCst)
    ))
}

fn config(hard_limit: u64, table: toml::Table) -> Config {
    Config {
        paths: Paths {
            home: temp_home(),
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
    worker: Arc<MockWorker>,
    compactor: Arc<MockCompactor>,
    jobs: tokio::sync::mpsc::UnboundedReceiver<Job>,
    home: PathBuf,
    next_message_id: MessageId,
    meter_rx: tokio::sync::mpsc::UnboundedReceiver<MeterEvent>,
}

#[derive(Clone, Copy)]
struct Setup {
    hard_limit: u64,
    worker: bool,
    compactor: bool,
    max_concurrent: usize,
    timeout_secs: u64,
    worker_permits: usize,
    keep_recent_turns: usize,
    show_progress: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            hard_limit: 80_000,
            worker: false,
            compactor: false,
            max_concurrent: 2,
            timeout_secs: 30,
            worker_permits: 100,
            keep_recent_turns: 2,
            show_progress: true,
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

impl Harness {
    async fn new() -> Self {
        Self::setup(Setup::default()).await
    }

    async fn with_limit(hard_limit: u64) -> Self {
        Self::setup(Setup {
            hard_limit,
            ..Setup::default()
        })
        .await
    }

    async fn with_worker() -> Self {
        Self::setup(Setup {
            worker: true,
            ..Setup::default()
        })
        .await
    }

    async fn setup(setup: Setup) -> Self {
        let store = Store::open_memory().await.unwrap();
        let channel = Arc::new(MockChannel::default());
        let router = Arc::new(MockRouter::default());
        let provider = Arc::new(MockProvider::new());
        let worker = Arc::new(MockWorker::new(setup.worker_permits));
        let compactor = Arc::new(MockCompactor::default());
        let (meter, meter_rx) = tokio::sync::mpsc::unbounded_channel();
        let modules = Modules {
            channel: channel.clone(),
            router: router.clone(),
            provider: Arc::new(MeteredProvider::new(
                provider.clone(),
                "provider-mock",
                meter.clone(),
            )),
            worker: setup.worker.then(|| worker.clone() as Arc<dyn Worker>),
            compactor: setup
                .compactor
                .then(|| compactor.clone() as Arc<dyn Compactor>),
            tools: Vec::new(),
            meter,
            meter_rx: tokio::sync::mpsc::unbounded_channel().1,
        };
        let table: toml::Table = toml::from_str(&format!(
            "[worker]\nmax_concurrent = {}\ntimeout_secs = {}\nshow_progress = {}\n[compactor]\nidle_minutes = 10\nkeep_recent_turns = {}\ndigest_max_chars = 12000",
            setup.max_concurrent, setup.timeout_secs, setup.show_progress, setup.keep_recent_turns
        ))
        .unwrap();
        let config = config(setup.hard_limit, table);
        let home = config.paths.home.clone();
        let (jobs_tx, jobs) = tokio::sync::mpsc::unbounded_channel();
        let actor = Actor::new(
            config,
            modules,
            store.clone(),
            &channel.members().await.unwrap(),
            jobs_tx,
        )
        .await
        .unwrap();
        Self {
            actor,
            store,
            channel,
            router,
            provider,
            worker,
            compactor,
            jobs,
            home,
            next_message_id: 1000,
            meter_rx,
        }
    }

    /// Worker の終了で積まれる Join か、check_idle が積む Compact を待って処理する
    async fn process_next_job(&mut self) -> &'static str {
        let job = tokio::time::timeout(Duration::from_secs(5), self.jobs.recv())
            .await
            .expect("仕事が積まれない")
            .unwrap();
        let label = match &job {
            Job::Judge(_) => "judge",
            Job::Join { .. } => "join",
            Job::Compact => "compact",
        };
        self.actor.handle(job).await.unwrap();
        label
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
        self.next_message_id += 1;
        let mut msg = incoming(channel_id, name, content, self.next_message_id);
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
        let job = tokio::time::timeout(Duration::from_secs(3), jobs_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let Job::Judge(b) = job else {
            panic!("Judge 以外が積まれた: {job:?}");
        };
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
        worker: None,
        compactor: None,
        tools: Vec::new(),
        meter: tokio::sync::mpsc::unbounded_channel().0,
        meter_rx: tokio::sync::mpsc::unbounded_channel().1,
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

fn spawn_answers(cat: &str) -> Vec<(&'static str, Answer)> {
    let mut a = answers("spawn_task", 0.0, 0.0);
    a.retain(|(k, _)| *k != "task_category");
    a.push(("task_category", category(cat)));
    a
}

fn delivery(style: &str) -> Answer {
    choice(
        style,
        &["normal_report", "brief_supplement", "reply_anchor", "abort"],
        1.0,
    )
}

fn with_delivery(mut a: Vec<(&'static str, Answer)>, style: &str) -> Vec<(&'static str, Answer)> {
    a.push(("delivery_style", delivery(style)));
    a
}

async fn wait_until(mut f: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("条件が満たされない");
}

async fn runs(store: &Store, status: RunStatus) -> Vec<noth_core::store::WorkerRunRow> {
    store.worker_runs(GUILD, &[status]).await.unwrap()
}

#[tokio::test]
async fn spawn_task_posts_ack_and_records_goal_before_fork() {
    let mut h = Harness::setup(Setup {
        worker: true,
        worker_permits: 0,
        ..Setup::default()
    })
    .await;
    h.router.set(spawn_answers("web_research"));
    h.provider
        .script_text("ちょっと調べるね\n[Goal]\n東京の明日の天気を調べる");
    h.say(GENERAL, "明日の東京の天気わかる?").await;

    assert_eq!(h.sent(), vec![(GENERAL, "ちょっと調べるね".to_string())]);
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].kind, TurnKind::Trigger);
    assert_eq!(turns[1].kind, TurnKind::Response);
    assert_eq!(
        turns[1].text,
        "ちょっと調べるね\n[Goal]\n東京の明日の天気を調べる"
    );
    // 本線にも Worker と同じ tool 定義を渡す
    let main = &h.prompts()[0];
    assert_eq!(main.tools.len(), 2);
    assert!(main.harness_rules.contains("[Goal]"));

    let worker = h.worker.clone();
    wait_until(|| worker.started() == 1).await;
    let task = h.worker.tasks.lock().unwrap()[0].clone();
    assert_eq!(task.goal, "東京の明日の天気を調べる");
    assert_eq!(task.task_category, TaskCategory::WebResearch);
    assert_eq!(task.base.active_log, turns);
    assert!(task.base.ephemeral_tail.is_empty());
    assert_eq!(
        serde_json::to_value(&task.base.tools).unwrap(),
        serde_json::to_value(&main.tools).unwrap()
    );
    assert_eq!(task.base.harness_rules, main.harness_rules);
    assert!(task.work_dir.starts_with(h.home.join("work")));
    assert!(task.work_dir.ends_with(&task.run_id));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&task.work_dir)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    let running = runs(&h.store, RunStatus::Running).await;
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].run_id, task.run_id);
    assert_eq!(running[0].trigger_message_id, Some(h.next_message_id));
    assert_eq!(h.actor.in_flight(), 1);
    h.worker.release.add_permits(1);
}

#[tokio::test]
async fn join_normal_report_appends_conclusion_and_posts() {
    let mut h = Harness::with_worker().await;
    h.router.set(with_delivery(
        spawn_answers("web_research"),
        "normal_report",
    ));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;

    assert_eq!(h.process_next_job().await, "join");
    assert_eq!(h.actor.in_flight(), 0);
    let sent = h.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].0, GENERAL);
    assert_eq!(*h.channel.reply_to.lock().unwrap(), vec![None, None]);

    let run = &runs(&h.store, RunStatus::Finished).await[0];
    assert_eq!(run.delivery_style.as_deref(), Some("normal_report"));
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns.len(), 4);
    assert_eq!(turns[2].kind, TurnKind::Conclusion);
    assert_eq!(
        turns[2].text,
        format!("[内部調査完了 {}: 結論: 天気]", run.run_id)
    );
    assert_eq!(turns[3].kind, TurnKind::Response);
    assert_eq!(turns[3].text, sent[1].1);
    assert!(turns.iter().all(|t| t.kind != TurnKind::Instruction));

    // 報告の生成では、結論メモは Layer 3 の末尾、指示は Layer 4 だけに入る
    let report = h.prompts().last().unwrap().clone();
    assert_eq!(report.active_log.last().unwrap().text, turns[2].text);
    assert_eq!(report.ephemeral_tail.len(), 1);
    assert_eq!(report.ephemeral_tail[0].kind, TurnKind::Instruction);
}

#[tokio::test]
async fn join_reply_anchor_replies_to_trigger() {
    let mut h = Harness::with_worker().await;
    h.router
        .set(with_delivery(spawn_answers("web_research"), "reply_anchor"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    let trigger_message_id = h.next_message_id;
    h.process_next_job().await;
    assert_eq!(
        *h.channel.reply_to.lock().unwrap(),
        vec![None, Some(trigger_message_id)]
    );
}

#[tokio::test]
async fn join_abort_posts_nothing() {
    let mut h = Harness::with_worker().await;
    h.router
        .set(with_delivery(spawn_answers("web_research"), "abort"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    let before = h.store.active_turns(GUILD).await.unwrap();

    h.process_next_job().await;
    assert_eq!(h.sent().len(), 1);
    assert_eq!(h.prompts().len(), 1);
    assert_eq!(h.store.active_turns(GUILD).await.unwrap(), before);
    let run = &runs(&h.store, RunStatus::Finished).await[0];
    assert_eq!(run.delivery_style.as_deref(), Some("abort"));
    assert_eq!(run.conclusion.as_deref(), Some("結論: 天気"));
}

#[tokio::test]
async fn join_router_error_is_abort() {
    let mut h = Harness::with_worker().await;
    h.router.set(spawn_answers("web_research"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    h.router.fail.store(true, Ordering::SeqCst);
    h.process_next_job().await;
    assert_eq!(h.sent().len(), 1);
    let run = &runs(&h.store, RunStatus::Finished).await[0];
    assert_eq!(run.delivery_style.as_deref(), Some("abort"));
}

#[tokio::test]
async fn join_provider_failure_keeps_conclusion_only() {
    let mut h = Harness::with_worker().await;
    h.router.set(with_delivery(
        spawn_answers("web_research"),
        "normal_report",
    ));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    h.provider.fail.store(true, Ordering::SeqCst);
    h.process_next_job().await;

    assert_eq!(h.sent().len(), 1);
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[2].kind, TurnKind::Conclusion);
}

#[tokio::test]
async fn worker_timeout_becomes_failure_memo() {
    let mut h = Harness::setup(Setup {
        worker: true,
        timeout_secs: 1,
        ..Setup::default()
    })
    .await;
    *h.worker.delay.lock().unwrap() = Duration::from_secs(10);
    h.router.set(with_delivery(
        spawn_answers("web_research"),
        "normal_report",
    ));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;

    h.process_next_job().await;
    let run = &runs(&h.store, RunStatus::Failed).await[0];
    assert!(run.conclusion.as_deref().unwrap().contains("1 秒"));
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert!(
        turns[2]
            .text
            .starts_with(&format!("[内部調査失敗 {}: ", run.run_id))
    );
    assert_eq!(h.sent().len(), 2);
}

#[tokio::test]
async fn worker_failure_becomes_failure_memo() {
    let mut h = Harness::with_worker().await;
    h.worker.fail.store(true, Ordering::SeqCst);
    h.router.set(with_delivery(
        spawn_answers("web_research"),
        "normal_report",
    ));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    h.process_next_job().await;
    let run = &runs(&h.store, RunStatus::Failed).await[0];
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(
        turns[2].text,
        format!("[内部調査失敗 {}: API エラー]", run.run_id)
    );
}

#[tokio::test]
async fn second_run_waits_when_max_concurrent_is_one() {
    let mut h = Harness::setup(Setup {
        worker: true,
        max_concurrent: 1,
        worker_permits: 0,
        ..Setup::default()
    })
    .await;
    h.router
        .set(with_delivery(spawn_answers("web_research"), "abort"));
    h.provider.script_text("調べるね\n[Goal]\n一つ目");
    h.say(GENERAL, "一つ目を調べて").await;
    h.provider.script_text("調べるね\n[Goal]\n二つ目");
    h.say(RANDOM, "二つ目を調べて").await;

    let worker = h.worker.clone();
    wait_until(|| worker.started() == 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.worker.started(), 1);
    assert_eq!(runs(&h.store, RunStatus::Queued).await[0].goal, "二つ目");
    assert_eq!(h.actor.in_flight(), 2);
    // 本線は Worker の実行中も Judge を処理できる
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "ところで").await;
    assert_eq!(h.sent().len(), 3);

    h.worker.release.add_permits(1);
    h.process_next_job().await;
    wait_until(|| worker.started() == 2).await;
    assert_eq!(h.worker.tasks.lock().unwrap()[1].goal, "二つ目");
    h.worker.release.add_permits(1);
    h.process_next_job().await;
    assert_eq!(h.actor.in_flight(), 0);
    assert_eq!(runs(&h.store, RunStatus::Finished).await.len(), 2);
}

#[tokio::test]
async fn tool_calls_become_fixed_ack_and_goal() {
    let mut h = Harness::setup(Setup {
        worker: true,
        worker_permits: 0,
        ..Setup::default()
    })
    .await;
    // router は reply_only でも、本線が tool を呼んだら Worker に回す
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.provider.scripted.lock().unwrap().push_back(Completion {
        text: None,
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "web_search".into(),
            input: serde_json::json!({"query": "東京 天気"}),
        }],
        ..Default::default()
    });
    h.say(GENERAL, "天気は?").await;

    let ack = noth_core::layers::FIXED_ACK;
    assert_eq!(h.sent(), vec![(GENERAL, ack.to_string())]);
    let turns = h.store.active_turns(GUILD).await.unwrap();
    let response = &turns[1].text;
    assert!(response.starts_with(&format!("{ack}\n[Goal]\n")));
    assert!(response.contains("web_search"));
    assert!(response.contains("東京 天気"));
    let run = &runs(&h.store, RunStatus::Queued)
        .await
        .into_iter()
        .chain(runs(&h.store, RunStatus::Running).await)
        .next()
        .unwrap();
    assert_eq!(run.task_category, TaskCategory::WebResearch);

    h.worker.release.add_permits(1);
}

fn shell_call() -> Completion {
    Completion {
        text: None,
        tool_calls: vec![ToolCall {
            id: "c2".into(),
            name: "shell".into(),
            input: serde_json::json!({"command": "ls"}),
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn shell_call_is_code_or_tool_only_when_router_agrees() {
    let mut h = Harness::setup(Setup {
        worker: true,
        worker_permits: 0,
        max_concurrent: 3,
        ..Setup::default()
    })
    .await;
    let worker = h.worker.clone();

    // router が作業不要と見ているのに本線が shell を呼んでも、shell は許さない
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.provider.scripted.lock().unwrap().push_back(shell_call());
    h.say(GENERAL, "ls して").await;
    wait_until(|| worker.started() == 1).await;

    h.router.set(spawn_answers("code_or_tool"));
    h.provider.scripted.lock().unwrap().push_back(shell_call());
    h.say(GENERAL, "ls して").await;
    wait_until(|| worker.started() == 2).await;

    // router が code_or_tool でも、Web の tool を呼んだら web_research
    h.provider.scripted.lock().unwrap().push_back(Completion {
        text: None,
        tool_calls: vec![ToolCall {
            id: "c3".into(),
            name: "web_search".into(),
            input: serde_json::json!({"query": "x"}),
        }],
        ..Default::default()
    });
    h.say(GENERAL, "調べて").await;
    wait_until(|| worker.started() == 3).await;

    let categories: Vec<TaskCategory> = h
        .worker
        .tasks
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.task_category)
        .collect();
    assert_eq!(
        categories,
        vec![
            TaskCategory::WebResearch,
            TaskCategory::CodeOrTool,
            TaskCategory::WebResearch
        ]
    );
    h.worker.release.add_permits(3);
}

#[tokio::test]
async fn spawn_task_without_goal_line_is_plain_reply() {
    let mut h = Harness::with_worker().await;
    h.router.set(spawn_answers("web_research"));
    h.provider.script_text("それなら知ってるよ、晴れだって");
    h.say(GENERAL, "天気調べて").await;

    assert_eq!(
        h.sent(),
        vec![(GENERAL, "それなら知ってるよ、晴れだって".to_string())]
    );
    assert_eq!(h.actor.in_flight(), 0);
    assert!(
        h.store
            .worker_runs(
                GUILD,
                &[RunStatus::Queued, RunStatus::Running, RunStatus::Finished]
            )
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn idle_compact_replaces_old_turns_with_digest() {
    let mut h = Harness::setup(Setup {
        compactor: true,
        keep_recent_turns: 2,
        ..Setup::default()
    })
    .await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    for q in ["質問1", "質問2", "質問3"] {
        h.say(GENERAL, q).await;
    }
    let before = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(before.len(), 6);

    // まだアイドルではない
    h.actor.check_idle(Timestamp::now()).await.unwrap();
    assert!(h.jobs.try_recv().is_err());

    let later = Timestamp::now() + jiff::SignedDuration::from_mins(11);
    h.actor.check_idle(later).await.unwrap();
    // 同じアイドル期間には 1 回しか積まない
    h.actor.check_idle(later).await.unwrap();
    assert_eq!(h.process_next_job().await, "compact");
    assert!(h.jobs.try_recv().is_err());

    let after = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(after, before[4..].to_vec());
    let calls = h.compactor.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, before[..4].to_vec());
    // Profile は渡さない
    assert!(!calls[0].0.contains("alice"));
    let digest = h.store.latest_digest(GUILD).await.unwrap().unwrap();
    assert!(digest.starts_with("[エピソード "));
    assert!(digest.ends_with("要約1"));

    h.actor.check_idle(later).await.unwrap();
    assert!(h.jobs.try_recv().is_err());

    // 新しい発言で再びアイドル判定の対象になり、Layer 2 は追記される
    h.say(GENERAL, "質問4").await;
    let later = Timestamp::now() + jiff::SignedDuration::from_mins(11);
    h.actor.check_idle(later).await.unwrap();
    assert_eq!(h.process_next_job().await, "compact");
    h.say(GENERAL, "質問5").await;
    let prompt = h.prompts().last().unwrap().clone();
    assert!(prompt.episodic_digest.contains("要約1"));
    assert!(prompt.episodic_digest.ends_with("要約2"));
}

#[tokio::test]
async fn idle_compact_failure_keeps_turns() {
    let mut h = Harness::setup(Setup {
        compactor: true,
        keep_recent_turns: 2,
        ..Setup::default()
    })
    .await;
    h.compactor.fail.store(true, Ordering::SeqCst);
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "質問1").await;
    h.say(GENERAL, "質問2").await;
    let later = Timestamp::now() + jiff::SignedDuration::from_mins(11);
    h.actor.check_idle(later).await.unwrap();
    h.process_next_job().await;
    assert_eq!(h.store.active_turns(GUILD).await.unwrap().len(), 4);
    assert!(h.store.latest_digest(GUILD).await.unwrap().is_none());
}

#[tokio::test]
async fn idle_compact_waits_for_running_worker() {
    let mut h = Harness::setup(Setup {
        worker: true,
        compactor: true,
        worker_permits: 0,
        keep_recent_turns: 0,
        ..Setup::default()
    })
    .await;
    h.router
        .set(with_delivery(spawn_answers("web_research"), "abort"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    let later = Timestamp::now() + jiff::SignedDuration::from_mins(11);
    h.actor.check_idle(later).await.unwrap();
    assert!(h.jobs.try_recv().is_err());

    h.worker.release.add_permits(1);
    assert_eq!(h.process_next_job().await, "join");
    h.actor.check_idle(later).await.unwrap();
    assert_eq!(h.process_next_job().await, "compact");
    assert!(h.store.active_turns(GUILD).await.unwrap().is_empty());
}

#[tokio::test]
async fn emergency_compaction_summarizes_oldest_chunk() {
    let mut h = Harness::setup(Setup {
        hard_limit: 100,
        compactor: true,
        ..Setup::default()
    })
    .await;
    *h.provider.input_tokens.lock().unwrap() = 1_000;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "話題A").await;
    assert_eq!(h.store.active_turns(GUILD).await.unwrap().len(), 2);

    h.router.set(answers("reply_only", 0.0, 0.9));
    h.say(GENERAL, "話題B").await;
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns.len(), 2);
    assert!(turns[0].text.contains("話題B"));
    let calls = h.compactor.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].1[0].text.contains("話題A"));
    let digest = h.store.latest_digest(GUILD).await.unwrap().unwrap();
    assert!(digest.ends_with("要約1"));
    let chunk_a = calls[0].1[0].chunk_id.unwrap();
    assert!(h.store.chunk(chunk_a).await.unwrap().unwrap().compacted);
}

#[tokio::test]
async fn restore_aborts_queued_and_running_runs() {
    let store = Store::open_memory().await.unwrap();
    for id in ["q", "r", "f"] {
        store
            .insert_worker_run(
                GUILD,
                &NewWorkerRun {
                    run_id: id.into(),
                    goal: "g".into(),
                    task_category: TaskCategory::WebResearch,
                    channel_id: GENERAL,
                    chunk_id: 1,
                    trigger_message_id: None,
                    fork_message_id: 0,
                },
            )
            .await
            .unwrap();
    }
    store.start_run("r").await.unwrap();
    store.start_run("f").await.unwrap();
    store
        .finish_run("f", RunStatus::Finished, "memo")
        .await
        .unwrap();

    noth_core::restore::restore(&store, GUILD).await.unwrap();
    let aborted: Vec<String> = runs(&store, RunStatus::Aborted)
        .await
        .into_iter()
        .map(|r| r.run_id)
        .collect();
    assert_eq!(aborted, vec!["q", "r"]);
    assert_eq!(runs(&store, RunStatus::Finished).await.len(), 1);
}

#[tokio::test]
async fn run_waits_for_worker_join_on_shutdown() {
    let store = Store::open_memory().await.unwrap();
    let channel = Arc::new(MockChannel::default());
    channel
        .scripted
        .lock()
        .unwrap()
        .push(incoming(GENERAL, "general", "Noth、天気調べて", 1));
    let router = Arc::new(MockRouter::default());
    router.set(with_delivery(
        spawn_answers("web_research"),
        "normal_report",
    ));
    let provider = Arc::new(MockProvider::new());
    provider.script_text("調べるね\n[Goal]\n天気");
    let worker = Arc::new(MockWorker::new(100));
    *worker.delay.lock().unwrap() = Duration::from_millis(300);
    let modules = Modules {
        channel: channel.clone(),
        router: router.clone(),
        provider: provider.clone(),
        worker: Some(worker.clone()),
        compactor: None,
        tools: Vec::new(),
        meter: tokio::sync::mpsc::unbounded_channel().0,
        meter_rx: tokio::sync::mpsc::unbounded_channel().1,
    };
    let table: toml::Table =
        toml::from_str("[channel.discord]\ndebounce_secs = 0.05\n[worker]\ntimeout_secs = 5")
            .unwrap();
    let config = config(80_000, table);
    let home = config.paths.home.clone();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(noth_core::actor::run(
        config,
        modules,
        store.clone(),
        async move {
            let _ = stop_rx.await;
        },
    ));

    wait_until(|| worker.started() == 1).await;
    assert_eq!(channel.sent.lock().unwrap().len(), 1);
    // Worker の実行中に止めても、Join の報告まで済ませてから終わる
    stop_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(channel.sent.lock().unwrap().len(), 2);
    assert_eq!(runs(&store, RunStatus::Finished).await.len(), 1);
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
async fn idle_compact_is_skipped_when_conversation_resumed() {
    let mut h = Harness::setup(Setup {
        compactor: true,
        keep_recent_turns: 2,
        ..Setup::default()
    })
    .await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "質問1").await;
    h.say(GENERAL, "質問2").await;
    let later = Timestamp::now() + jiff::SignedDuration::from_mins(11);
    h.actor.check_idle(later).await.unwrap();
    // Compact を積んだ後、処理される前に会話が再開した
    h.say(GENERAL, "質問3").await;
    assert_eq!(h.process_next_job().await, "compact");
    assert_eq!(h.store.active_turns(GUILD).await.unwrap().len(), 6);
    assert!(h.compactor.calls.lock().unwrap().is_empty());

    // 次のアイドル期間には改めて積まれる
    let later = Timestamp::now() + jiff::SignedDuration::from_mins(11);
    h.actor.check_idle(later).await.unwrap();
    assert_eq!(h.process_next_job().await, "compact");
    assert_eq!(h.store.active_turns(GUILD).await.unwrap().len(), 2);
}

#[tokio::test]
async fn idle_compact_keeps_at_least_configured_turns() {
    let mut h = Harness::setup(Setup {
        compactor: true,
        keep_recent_turns: 1,
        ..Setup::default()
    })
    .await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "質問1").await;
    h.say(GENERAL, "質問2").await;
    let before = h.store.active_turns(GUILD).await.unwrap();
    let later = Timestamp::now() + jiff::SignedDuration::from_mins(11);
    h.actor.check_idle(later).await.unwrap();
    h.process_next_job().await;
    // 末尾の応答 1 つだけでは先頭が assistant になるので、その trigger も残す
    assert_eq!(
        h.store.active_turns(GUILD).await.unwrap(),
        before[2..].to_vec()
    );
}

#[tokio::test]
async fn emergency_compaction_shrinks_single_long_chunk() {
    let mut h = Harness::setup(Setup {
        hard_limit: 100,
        compactor: true,
        keep_recent_turns: 2,
        ..Setup::default()
    })
    .await;
    *h.provider.input_tokens.lock().unwrap() = 1_000;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "話題A-1").await;
    h.say(GENERAL, "話題A-2").await;
    h.say(GENERAL, "話題A-3").await;
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns.len(), 2);
    assert!(turns[0].text.contains("話題A-3"));
    assert_eq!(h.compactor.calls.lock().unwrap().len(), 2);
    let digest = h.store.latest_digest(GUILD).await.unwrap().unwrap();
    assert!(digest.contains("要約1") && digest.ends_with("要約2"));
    // 進行中のチャンクなので compacted にはしない
    let chunk = turns[0].chunk_id.unwrap();
    assert!(!h.store.chunk(chunk).await.unwrap().unwrap().compacted);
}

#[tokio::test]
async fn hard_limit_drops_older_part_of_single_chunk_without_compactor() {
    let mut h = Harness::with_limit(100).await;
    *h.provider.input_tokens.lock().unwrap() = 1_000;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "話題A-1").await;
    h.say(GENERAL, "話題A-2").await;
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns.len(), 2);
    assert!(turns[0].text.contains("話題A-2"));
    assert!(h.store.latest_digest(GUILD).await.unwrap().is_none());
}

#[tokio::test]
async fn typing_continues_while_worker_runs_and_stops_after() {
    let mut h = Harness::setup(Setup {
        worker: true,
        worker_permits: 0,
        max_concurrent: 1,
        ..Setup::default()
    })
    .await;
    h.actor.set_typing_interval(Duration::from_millis(20));
    h.router
        .set(with_delivery(spawn_answers("web_research"), "abort"));
    h.provider.script_text("調べるね\n[Goal]\n一つ目");
    h.say(GENERAL, "一つ目を調べて").await;
    h.provider.script_text("調べるね\n[Goal]\n二つ目");
    h.say(RANDOM, "二つ目を調べて").await;

    let typing_count = |channel: ChannelId| {
        let typing = h.channel.typing.lock().unwrap().clone();
        typing.iter().filter(|c| **c == channel).count()
    };
    let (general_before, random_before) = (typing_count(GENERAL), typing_count(RANDOM));
    tokio::time::sleep(Duration::from_millis(200)).await;
    // 実行中の run も、semaphore 待ちの run も入力中を出し続ける
    assert_eq!(h.worker.started(), 1);
    assert!(typing_count(GENERAL) >= general_before + 3);
    assert!(typing_count(RANDOM) >= random_before + 3);

    h.worker.release.add_permits(2);
    h.process_next_job().await;
    h.process_next_job().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let settled = h.channel.typing.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.channel.typing.lock().unwrap().len(), settled);
}

fn progress_step(ms: u64, tool: &str, summary: &str) -> (Duration, WorkerProgress) {
    (
        Duration::from_millis(ms),
        WorkerProgress {
            tool: tool.into(),
            summary: summary.into(),
        },
    )
}

async fn progress_harness(show_progress: bool) -> Harness {
    let mut h = Harness::setup(Setup {
        worker: true,
        worker_permits: 0,
        show_progress,
        ..Setup::default()
    })
    .await;
    h.actor.set_progress_interval(Duration::from_millis(300));
    h.router
        .set(with_delivery(spawn_answers("web_research"), "abort"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h
}

#[tokio::test]
async fn progress_edits_are_throttled_and_finish_with_footer() {
    let mut h = progress_harness(true).await;
    *h.worker.progress.lock().unwrap() = vec![
        progress_step(0, "web_search", "東京 天気"),
        progress_step(50, "web_fetch", "https://example.com"),
        progress_step(50, "web_agent", "天気サイトを操作"),
    ];
    h.say(GENERAL, "天気調べて").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    h.worker.release.add_permits(1);
    h.process_next_job().await;

    let edits = h.channel.edits.lock().unwrap().clone();
    // 3 件の進捗 + 結びで 4 回にならず、間引かれてまとめて反映される
    assert!(edits.len() <= 3, "{edits:?}");
    for pair in edits.windows(2).take(edits.len().saturating_sub(2)) {
        assert!(pair[1].3 - pair[0].3 >= Duration::from_millis(290));
    }
    for (channel, message_id, text, _) in &edits {
        assert_eq!((*channel, *message_id), (GENERAL, 1));
        assert!(text.starts_with("調べるね\n-# 🔍 web_search 東京 天気"));
    }
    let last = &edits.last().unwrap().2;
    assert_eq!(
        last,
        "調べるね\n-# 🔍 web_search 東京 天気\n-# 📄 web_fetch https://example.com\n-# 🧭 web_agent 天気サイトを操作\n-# ✅ 調べ終わりました"
    );
    // Layer 3 には進捗を残さない
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns[1].text, "調べるね\n[Goal]\n天気");
}

#[tokio::test]
async fn progress_final_line_marks_failure() {
    let mut h = progress_harness(true).await;
    h.worker.fail.store(true, Ordering::SeqCst);
    h.say(GENERAL, "天気調べて").await;
    h.worker.release.add_permits(1);
    h.process_next_job().await;
    let edits = h.channel.edits.lock().unwrap().clone();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].2, "調べるね\n-# ⚠️ 調査に失敗しました");
}

#[tokio::test]
async fn progress_edit_error_does_not_stop_worker() {
    let mut h = progress_harness(true).await;
    h.channel.edit_fail.store(true, Ordering::SeqCst);
    *h.worker.progress.lock().unwrap() = vec![
        progress_step(0, "web_search", "a"),
        progress_step(10, "web_search", "b"),
    ];
    h.say(GENERAL, "天気調べて").await;
    h.worker.release.add_permits(1);
    assert_eq!(h.process_next_job().await, "join");
    assert!(h.channel.edits.lock().unwrap().is_empty());
    assert_eq!(runs(&h.store, RunStatus::Finished).await.len(), 1);
}

#[tokio::test]
async fn progress_disabled_never_edits() {
    let mut h = progress_harness(false).await;
    *h.worker.progress.lock().unwrap() = vec![progress_step(0, "web_search", "a")];
    h.say(GENERAL, "天気調べて").await;
    h.worker.release.add_permits(1);
    h.process_next_job().await;
    assert!(h.channel.edits.lock().unwrap().is_empty());
    assert!(h.worker.tasks.lock().unwrap()[0].progress.is_none());
}

fn drain_purposes(h: &mut Harness) -> Vec<(Purpose, Option<String>)> {
    let mut out = Vec::new();
    while let Ok(event) = h.meter_rx.try_recv() {
        if let MeterEvent::Cost(c) = event {
            out.push((c.purpose, c.run_id));
        }
    }
    out
}

#[tokio::test]
async fn main_and_join_calls_are_metered_with_purpose() {
    let mut h = Harness::with_worker().await;
    h.router.set(with_delivery(
        spawn_answers("web_research"),
        "normal_report",
    ));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    assert_eq!(drain_purposes(&mut h), vec![(Purpose::Main, None)]);

    h.process_next_job().await;
    let run_id = runs(&h.store, RunStatus::Finished).await[0].run_id.clone();
    assert_eq!(drain_purposes(&mut h), vec![(Purpose::Join, Some(run_id))]);
}

#[tokio::test]
async fn run_saves_meter_events_before_exit() {
    let store = Store::open_memory().await.unwrap();
    let channel = Arc::new(MockChannel::default());
    channel
        .scripted
        .lock()
        .unwrap()
        .push(incoming(GENERAL, "general", "Noth、いる?", 1));
    let router = Arc::new(MockRouter::default());
    router.set(answers("reply_only", 0.0, 0.0));
    let (meter, meter_rx) = tokio::sync::mpsc::unbounded_channel();
    let provider = Arc::new(MeteredProvider::new(
        Arc::new(MockProvider::new()),
        "provider-mock",
        meter.clone(),
    ));
    let modules = Modules {
        channel: channel.clone(),
        router,
        provider,
        worker: None,
        compactor: None,
        tools: Vec::new(),
        meter: meter.clone(),
        meter_rx,
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
    let sent = channel.clone();
    wait_until(|| !sent.sent.lock().unwrap().is_empty()).await;
    // tool の課金とレート制限も同じ経路で届く。停止の直前に届いた分も保存される
    meter
        .send(MeterEvent::RateLimit(RateLimitSnapshot {
            source: "tool-web-search".into(),
            bucket: "search".into(),
            limit: 100,
            remaining: 99,
            reset_at: None,
        }))
        .unwrap();
    stop_tx.send(()).unwrap();
    handle.await.unwrap().unwrap();

    let by_purpose = store
        .cost_summary(GUILD, 0, CostGroup::Purpose)
        .await
        .unwrap();
    assert_eq!(by_purpose.len(), 1);
    assert_eq!(by_purpose[0].key, "main");
    assert_eq!(by_purpose[0].calls, 1);
    assert_eq!(by_purpose[0].input_tokens, 10);
    assert_eq!(by_purpose[0].unpriced_calls, 1);
    let limits = store.rate_limits(GUILD).await.unwrap();
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].0.remaining, 99);
}
