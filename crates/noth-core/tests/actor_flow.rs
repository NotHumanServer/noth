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
    reactions: Mutex<Vec<(ChannelId, MessageId, String)>>,
    exclusions: std::sync::OnceLock<noth_core::exclusion::ChannelExclusions>,
}

#[async_trait]
impl Channel for MockChannel {
    fn guild_id(&self) -> GuildId {
        GUILD
    }
    fn exclusions(&self) -> Option<noth_core::exclusion::ChannelExclusions> {
        self.exclusions.get().cloned()
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
    async fn react(&self, channel_id: ChannelId, message_id: MessageId, emoji: &str) -> Result<()> {
        self.reactions
            .lock()
            .unwrap()
            .push((channel_id, message_id, emoji.to_string()));
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
    states: Mutex<Vec<RouterState>>,
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
        state: &RouterState,
        _questions: &[(String, Question)],
    ) -> Result<Decision> {
        self.states.lock().unwrap().push(state.clone());
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
    /// fail 時に返す Api エラーの status と本文
    fail_with: Mutex<(u16, String)>,
    input_tokens: Mutex<u64>,
    /// 空になるまで先頭から返す。空なら「応答<n>」
    scripted: Mutex<VecDeque<Completion>>,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            prompts: Mutex::default(),
            fail: AtomicBool::new(false),
            fail_with: Mutex::new((500, "boom".into())),
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
            let (status, body) = self.fail_with.lock().unwrap().clone();
            return Err(Error::Api { status, body });
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
    fn label(&self) -> String {
        "mock-model".into()
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
    /// Some なら結論メモをこの文字列にする
    conclusion: Mutex<Option<String>>,
}

impl MockWorker {
    fn new(permits: usize) -> Self {
        Self {
            tasks: Mutex::default(),
            release: tokio::sync::Semaphore::new(permits),
            delay: Mutex::new(Duration::ZERO),
            fail: AtomicBool::new(false),
            progress: Mutex::default(),
            conclusion: Mutex::default(),
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
        let tool_log = task.tool_log.clone();
        self.tasks.lock().unwrap().push(task);
        let steps = self.progress.lock().unwrap().clone();
        for (wait, p) in steps {
            tokio::time::sleep(wait).await;
            tool_log.lock().unwrap().push(p.clone());
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
        if let Some(memo) = self.conclusion.lock().unwrap().clone() {
            return WorkerOutcome::Concluded(memo);
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
    async fn digest(
        &self,
        harness_rules: &str,
        turns: &[Turn],
        _cache_session: &str,
    ) -> Result<String> {
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

const ADDRESSEES: [&str; 4] = ["other", "about_noth", "other_bot", "noth_request"];

fn addressee(name: &str) -> Answer {
    choice(name, &ADDRESSEES, 1.0)
}

fn reaction(name: &str) -> Answer {
    choice(name, &["none", "r0"], 1.0)
}

/// 旧い語彙(`silent` / `react` / `reply_only` / `spawn_task`)で書かれたテストを v2.1 の問いの回答に直す。
fn answers(action_name: &str, noise: f32, shift: f32) -> Vec<(&'static str, Answer)> {
    let (who, outside, react) = match action_name {
        "silent" => ("other", 0.0, "none"),
        "react" => ("about_noth", 0.0, "r0"),
        "reply_only" => ("noth_request", 0.0, "none"),
        "spawn_task" => ("noth_request", 1.0, "none"),
        other => panic!("未知の動き: {other}"),
    };
    vec![
        ("is_noise_or_meme", noul(noise)),
        ("is_topic_shift", noul(shift)),
        ("addressee", addressee(who)),
        ("needs_outside_work", noul(outside)),
        ("corrects_noth", noul(0.0)),
        ("worth_supplement", noul(0.0)),
        ("reaction", reaction(react)),
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
            retry_max: 2,
            max_tokens: 256,
        },
        router: RouterSection {
            recent_messages: 30,
            state_max_chars: 24_000,
            call_names: vec!["noth".into()],
            conversation_window_secs: 120,
            supplement_min_interval_secs: 300,
            supplement_max_per_day: 50,
            questions: Default::default(),
            thresholds: Thresholds::default(),
            reactions: vec![Reaction {
                emoji: "👀".into(),
                meaning: "気になる".into(),
            }],
        },
        memory: MemorySettings::default(),
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
    memo: bool,
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
            memo: true,
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
            commands: Vec::new(),
            meter,
            meter_rx: tokio::sync::mpsc::unbounded_channel().1,
        };
        let table: toml::Table = toml::from_str(&format!(
            "[worker]\nmax_concurrent = {}\ntimeout_secs = {}\nshow_progress = {}\n[compactor]\nidle_minutes = 10\nkeep_recent_turns = {}\ndigest_max_chars = 12000",
            setup.max_concurrent, setup.timeout_secs, setup.show_progress, setup.keep_recent_turns
        ))
        .unwrap();
        let mut config = config(setup.hard_limit, table);
        config.memory.memo = setup.memo;
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
            Job::Snapshot(_) => "snapshot",
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
                message_id: msg.message_id,
                author_name: "alice".into(),
                content: content.into(),
                at: msg.timestamp,
                mentions_self,
                reply_to_self: false,
                reply_to: None,
            }],
        };
        self.actor.handle_judge(batch).await.unwrap();
    }

    /// 判定にかけずに記録だけする。Worker の実行中に届いた人間の発言を作る
    async fn record_human(&mut self, channel_id: ChannelId, content: &str) {
        self.record_from(channel_id, 1, content, None).await;
    }

    async fn record_from(
        &mut self,
        channel_id: ChannelId,
        user_id: UserId,
        content: &str,
        bot_marker: Option<BotMarker>,
    ) {
        let name = if channel_id == GENERAL {
            "general"
        } else {
            "random"
        };
        self.next_message_id += 1;
        let mut msg = incoming(channel_id, name, content, self.next_message_id);
        msg.user_id = user_id;
        msg.bot_marker = bot_marker;
        self.store.insert_message(&msg).await.unwrap();
    }

    fn router_calls(&self) -> usize {
        self.router.states.lock().unwrap().len()
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
        reply_to: None,
        bot_marker: None,
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
    let [trigger] = &prompt.ephemeral_tail[..] else {
        panic!("tail: {:?}", prompt.ephemeral_tail);
    };
    // 発言先はトリガーのブロックに含め、Layer 3 と同一の文字列で昇格する
    assert_eq!(trigger.text, turns[1].text);
    assert!(trigger.text.contains("#general(channel_id: 100)"));
    assert!(!trigger.text.contains("[Goal]"));
    assert!(prompt.tools.is_empty());
    assert!(!prompt.harness_rules.contains("Goal"));
    assert!(prompt.profile.contains("alice"));
}

#[tokio::test]
async fn react_adds_emoji_to_last_message_without_speaking() {
    let mut h = Harness::new().await;
    h.router.set(answers("react", 0.0, 0.0));
    h.say(GENERAL, "新しいキーボード買った").await;
    h.say(GENERAL, "打鍵感が最高").await;

    let reactions = h.channel.reactions.lock().unwrap().clone();
    assert_eq!(
        reactions,
        vec![
            (GENERAL, 1001, "👀".to_string()),
            (GENERAL, 1002, "👀".to_string())
        ]
    );
    assert!(h.sent().is_empty());
    assert!(h.prompts().is_empty());
    assert!(h.store.active_turns(GUILD).await.unwrap().is_empty());
    assert_eq!(h.store.buffered_messages(GUILD).await.unwrap().len(), 2);
}

#[tokio::test]
async fn about_noth_with_reaction_none_stays_silent() {
    let mut h = Harness::new().await;
    let mut a = answers("react", 0.0, 0.0);
    a.push(("reaction", reaction("none")));
    h.router.set(a);
    h.say(GENERAL, "新しいキーボード買った").await;

    assert!(h.channel.reactions.lock().unwrap().is_empty());
    assert!(h.sent().is_empty());
}

async fn snapshot(h: &mut Harness) -> noth_core::command::ContextSnapshot {
    let (tx, rx) = tokio::sync::oneshot::channel();
    h.actor.handle(Job::Snapshot(tx)).await.unwrap();
    rx.await.unwrap().unwrap()
}

#[tokio::test]
async fn snapshot_reports_layers_usage_and_counts_without_changing_state() {
    let mut h = Harness::new().await;
    let empty = snapshot(&mut h).await;
    assert!(empty.last_request.is_none());
    assert_eq!(
        (empty.turns, empty.turn_channels, empty.episodes),
        (0, 0, 0)
    );

    h.router.set(answers("reply_only", 0.0, 0.0));
    *h.provider.input_tokens.lock().unwrap() = 1234;
    h.say(GENERAL, "質問1").await;
    h.say(RANDOM, "質問2").await;
    let prompts_before = h.prompts().len();
    // 課金記録は run() が計測イベントから保存するので、この harness では自分で保存する
    while let Ok(event) = h.meter_rx.try_recv() {
        if let MeterEvent::Cost(cost) = event {
            h.store.record_cost(GUILD, &cost).await.unwrap();
        }
    }

    let snap = snapshot(&mut h).await;
    assert_eq!(snap.model, "mock-model");
    let names: Vec<_> = snap.layers.iter().map(|l| l.name).collect();
    assert_eq!(
        names,
        [
            "tools",
            "Layer 0",
            "Layer 1",
            "Layer 1.5",
            "Layer 2",
            "Layer 3"
        ]
    );
    let layer3 = snap.layers.last().unwrap();
    assert!(layer3.chars > 0 && layer3.est_tokens > 0);
    assert_eq!(snap.turns, 4);
    assert_eq!(snap.turn_channels, 2);
    let req = snap.last_request.expect("実測が入る");
    assert_eq!(req.input_tokens, 1234);
    assert_eq!((req.cache_read_tokens, req.cache_write_tokens), (0, 0));
    assert!(!req.compacted_since);
    assert_eq!(snap.hard_limit_tokens, 80_000);
    assert_eq!((snap.workers_running, snap.workers_queued), (0, 0));
    assert_eq!(h.prompts().len(), prompts_before);

    tokio::time::sleep(Duration::from_millis(5)).await;
    h.store.insert_digest(GUILD, "要約").await.unwrap();
    let after = snapshot(&mut h).await;
    assert!(after.last_request.unwrap().compacted_since);
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

    let sent = h.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, GENERAL);
    assert!(
        sent[0].1.starts_with("⚠️ 応答を生成できませんでした"),
        "{sent:?}"
    );
    assert!(sent[0].1.contains("mock-model"), "{sent:?}");
    // 失敗の通知は Layer 3 に入れない
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
async fn provider_error_post_is_rate_limited_per_channel() {
    let mut h = Harness::new().await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.provider.fail.store(true, Ordering::SeqCst);
    h.say(GENERAL, "Noth いる?").await;
    h.say(GENERAL, "Noth まだ?").await;
    assert_eq!(h.sent().len(), 1);

    // 別のチャンネルには独立して知らせる
    h.say(RANDOM, "Noth こっちは?").await;
    let sent = h.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].0, RANDOM);
}

#[tokio::test]
async fn provider_error_message_follows_the_cause() {
    for (status, body, expected) in [
        (
            503,
            "unavailable",
            "mock-model が応答しませんでした)。少し待ってから",
        ),
        (
            401,
            "bad key",
            "mock-model の認証に失敗しました。管理者に確認してもらってください)",
        ),
        (
            429,
            "limit(コード: usage_limit_reached)",
            "mock-model の利用上限に達しました)",
        ),
        (400, "bad request", "mock-model でエラーが発生しました)"),
    ] {
        let mut h = Harness::new().await;
        h.router.set(answers("reply_only", 0.0, 0.0));
        h.provider.fail.store(true, Ordering::SeqCst);
        *h.provider.fail_with.lock().unwrap() = (status, body.into());
        h.say(GENERAL, "Noth いる?").await;
        let sent = h.sent();
        assert!(sent[0].1.contains(expected), "{status}: {sent:?}");
    }
}

#[tokio::test]
async fn join_failure_is_not_swallowed_by_a_reply_failure() {
    let mut h = Harness::with_worker().await;
    h.router
        .set(with_delivery(spawn_answers(), "normal_report"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    h.provider.fail.store(true, Ordering::SeqCst);
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "Noth ところで").await;
    assert_eq!(h.sent().len(), 2);
    h.router
        .set(with_delivery(spawn_answers(), "normal_report"));
    h.process_next_job().await;

    let sent = h.sent();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent[1].1.starts_with("⚠️ 応答を生成できませんでした"));
    assert!(
        sent[2]
            .1
            .starts_with("⚠️ 調査結果の報告を生成できませんでした")
    );
}

#[tokio::test]
async fn spawn_task_runs_as_reply_only_without_worker() {
    let mut h = Harness::new().await;
    h.router.set(answers("spawn_task", 0.0, 0.0));
    h.say(GENERAL, "これ調べて").await;

    assert_eq!(h.sent().len(), 1);
    assert!(h.prompts()[0].tools.is_empty());
}

#[tokio::test]
async fn mention_is_answered_even_if_router_says_other() {
    let mut h = Harness::new().await;
    h.router.set(answers("silent", 0.9, 0.0));
    h.say_as(GENERAL, "@Noth", true).await;

    assert_eq!(h.sent().len(), 1);
    assert!(h.store.buffered_messages(GUILD).await.unwrap().is_empty());
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
async fn router_failure_still_answers_when_mentioned() {
    let mut h = Harness::new().await;
    h.router.fail.store(true, Ordering::SeqCst);

    h.say(GENERAL, "今日は寒いね").await;
    assert!(h.sent().is_empty());

    h.say_as(GENERAL, "@Noth どう思う?", true).await;
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
        commands: Vec::new(),
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

fn spawn_answers() -> Vec<(&'static str, Answer)> {
    answers("spawn_task", 0.0, 0.0)
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
async fn worker_task_carries_distinct_requesters_without_noth() {
    let mut h = Harness::setup(Setup {
        worker: true,
        worker_permits: 0,
        ..Setup::default()
    })
    .await;
    h.router.set(spawn_answers());
    h.provider.script_text("調べるね\n[Goal]\n調査");
    let mut pending = Vec::new();
    // 発言者 1, 2, 1(重複)と、Noth 自身(99)
    for (user_id, is_self) in [(1, false), (2, false), (1, false), (99, true)] {
        h.next_message_id += 1;
        let mut m = incoming(GENERAL, "general", "調べて", h.next_message_id);
        m.user_id = user_id;
        m.is_self = is_self;
        let row_id = h.store.insert_message(&m).await.unwrap();
        pending.push(PendingMessage {
            row_id,
            message_id: m.message_id,
            author_name: "x".into(),
            content: m.content.clone(),
            at: m.timestamp,
            mentions_self: false,
            reply_to_self: false,
            reply_to: None,
        });
    }
    h.actor
        .handle_judge(JudgeBatch {
            channel_id: GENERAL,
            channel_name: "general".into(),
            messages: pending,
        })
        .await
        .unwrap();
    let worker = h.worker.clone();
    wait_until(|| worker.started() == 1).await;
    let task = h.worker.tasks.lock().unwrap()[0].clone();
    assert_eq!(task.audience.requesters, vec![1, 2]);
    assert_eq!(task.audience.channel_id, Some(GENERAL));
    h.worker.release.add_permits(1);
}

#[tokio::test]
async fn worker_task_has_no_requesters_when_a_trigger_row_is_missing() {
    let mut h = Harness::setup(Setup {
        worker: true,
        worker_permits: 0,
        ..Setup::default()
    })
    .await;
    h.router.set(spawn_answers());
    h.provider.script_text("調べるね\n[Goal]\n調査");
    h.next_message_id += 1;
    let m = incoming(GENERAL, "general", "調べて", h.next_message_id);
    let row_id = h.store.insert_message(&m).await.unwrap();
    let pending = |row_id, message_id| PendingMessage {
        row_id,
        message_id,
        author_name: "x".into(),
        content: "調べて".into(),
        at: m.timestamp,
        mentions_self: false,
        reply_to_self: false,
        reply_to: None,
    };
    // DB に無い行が混ざると、発言者を一部しか特定できないので依頼者を空にして閉じる
    h.actor
        .handle_judge(JudgeBatch {
            channel_id: GENERAL,
            channel_name: "general".into(),
            messages: vec![pending(row_id, m.message_id), pending(999_999, 999)],
        })
        .await
        .unwrap();
    let worker = h.worker.clone();
    wait_until(|| worker.started() == 1).await;
    let task = h.worker.tasks.lock().unwrap()[0].clone();
    assert!(task.audience.requesters.is_empty());
    assert_eq!(task.audience.channel_id, Some(GENERAL));
    h.worker.release.add_permits(1);
}

#[tokio::test]
async fn spawn_task_posts_ack_and_records_goal_before_fork() {
    let mut h = Harness::setup(Setup {
        worker: true,
        worker_permits: 0,
        ..Setup::default()
    })
    .await;
    h.router.set(spawn_answers());
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
    // main にも Worker と同じ tool 定義を渡す
    let main = &h.prompts()[0];
    assert_eq!(main.tools.len(), 2);
    assert!(main.harness_rules.contains("[Goal]"));

    let worker = h.worker.clone();
    wait_until(|| worker.started() == 1).await;
    let task = h.worker.tasks.lock().unwrap()[0].clone();
    assert_eq!(task.goal, "東京の明日の天気を調べる");
    assert_eq!(task.origin, Origin::Request);
    assert_eq!(task.audience.requesters, vec![1]);
    // fork 時点の main のリクエスト(`[判定]` まで)が先頭に残り、昇格した Ack + Goal が続く
    let sequence = |p: &LayeredPrompt| {
        p.active_log
            .iter()
            .chain(&p.ephemeral_tail)
            .map(|t| (t.role, t.kind, t.text.clone()))
            .collect::<Vec<_>>()
    };
    let main_sequence = sequence(&h.prompts()[0]);
    let worker_sequence = sequence(&task.base);
    assert_eq!(worker_sequence[..main_sequence.len()], main_sequence[..]);
    assert_eq!(
        worker_sequence[main_sequence.len()..],
        [(turns[1].role, turns[1].kind, turns[1].text.clone())]
    );
    assert_eq!(task.audience.channel_id, Some(GENERAL));
    assert_eq!(task.audience.channel_name.as_deref(), Some("general"));
    // router が spawn_task と判定したときだけ、Layer 4 の末尾で Worker に回すよう促す
    let hint = main.ephemeral_tail.last().unwrap();
    assert!(hint.text.contains("[Goal]"));
    assert_eq!(
        serde_json::to_value(&task.base.tools).unwrap(),
        serde_json::to_value(&main.tools).unwrap()
    );
    assert_eq!(task.base.harness_rules, main.harness_rules);
    assert!(task.work_dir.starts_with(h.home.join("work")));
    assert!(task.work_dir.ends_with(&task.run_id));
    // 作業ディレクトリは shell が初めて使うときに作る。使われない run に空のディレクトリを残さない
    assert!(!task.work_dir.exists());
    // 残す成果物の置き場所は、Worker が動く前に 0700 で用意される
    let workspace = h.home.join("workspace");
    assert!(workspace.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&workspace).unwrap().permissions().mode();
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
    h.router
        .set(with_delivery(spawn_answers(), "normal_report"));
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
    h.router.set(with_delivery(spawn_answers(), "reply_anchor"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    let trigger_message_id = h.next_message_id;
    h.record_human(GENERAL, "ところで昼ごはんは").await;
    h.process_next_job().await;
    assert_eq!(
        *h.channel.reply_to.lock().unwrap(),
        vec![None, Some(trigger_message_id)]
    );
}

#[tokio::test]
async fn join_without_human_follow_up_is_normal_report_even_if_router_says_anchor() {
    let mut h = Harness::with_worker().await;
    h.router.set(with_delivery(spawn_answers(), "reply_anchor"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    h.process_next_job().await;
    assert_eq!(*h.channel.reply_to.lock().unwrap(), vec![None, None]);
    let run = &runs(&h.store, RunStatus::Finished).await[0];
    assert_eq!(run.delivery_style.as_deref(), Some("normal_report"));
}

#[tokio::test]
async fn join_abort_posts_nothing() {
    let mut h = Harness::with_worker().await;
    h.router.set(with_delivery(spawn_answers(), "abort"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    h.record_human(GENERAL, "ところで昼ごはんは").await;
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
    h.router.set(spawn_answers());
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    h.record_human(GENERAL, "ところで昼ごはんは").await;
    h.router.fail.store(true, Ordering::SeqCst);
    h.process_next_job().await;
    assert_eq!(h.sent().len(), 1);
    let run = &runs(&h.store, RunStatus::Finished).await[0];
    assert_eq!(run.delivery_style.as_deref(), Some("abort"));
}

#[tokio::test]
async fn join_provider_failure_keeps_conclusion_only() {
    let mut h = Harness::with_worker().await;
    h.router
        .set(with_delivery(spawn_answers(), "normal_report"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    h.provider.fail.store(true, Ordering::SeqCst);
    h.process_next_job().await;

    let sent = h.sent();
    assert_eq!(sent.len(), 2);
    assert!(
        sent[1]
            .1
            .starts_with("⚠️ 調査結果の報告を生成できませんでした"),
        "{sent:?}"
    );
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[2].kind, TurnKind::Conclusion);
}

#[tokio::test]
async fn tool_log_survives_a_worker_timeout() {
    let mut h = Harness::setup(Setup {
        worker: true,
        timeout_secs: 1,
        ..Setup::default()
    })
    .await;
    *h.worker.delay.lock().unwrap() = Duration::from_secs(10);
    *h.worker.progress.lock().unwrap() = vec![(
        Duration::ZERO,
        WorkerProgress {
            tool: "web_search".into(),
            summary: "「OpenCode Zen」".into(),
        },
    )];
    h.router
        .set(with_delivery(spawn_answers(), "normal_report"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    h.process_next_job().await;

    let run = &runs(&h.store, RunStatus::Failed).await[0];
    let log = h.store.tool_log(&run.run_id).await.unwrap().unwrap();
    assert!(log.contains("OpenCode Zen"), "{log}");
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
    h.router
        .set(with_delivery(spawn_answers(), "normal_report"));
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
    h.router
        .set(with_delivery(spawn_answers(), "normal_report"));
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
    h.router.set(with_delivery(spawn_answers(), "abort"));
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
    // main は Worker の実行中も Judge を処理できる
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
    // router は reply_only でも、main が tool を呼んだら Worker に回す
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
    assert_eq!(run.origin, Origin::Request);

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
async fn main_initiated_runs_are_requests_regardless_of_router_and_tools() {
    let mut h = Harness::setup(Setup {
        worker: true,
        worker_permits: 0,
        max_concurrent: 3,
        ..Setup::default()
    })
    .await;
    let worker = h.worker.clone();

    // router が作業不要と見ていても、main が Worker に回したなら依頼として記録する
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.provider.scripted.lock().unwrap().push_back(shell_call());
    h.say(GENERAL, "ls して").await;
    wait_until(|| worker.started() == 1).await;

    h.router.set(spawn_answers());
    h.provider.scripted.lock().unwrap().push_back(shell_call());
    h.say(GENERAL, "ls して").await;
    wait_until(|| worker.started() == 2).await;

    // tool の種類で origin は変えない
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

    let origins: Vec<Origin> = h
        .worker
        .tasks
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.origin)
        .collect();
    assert_eq!(origins, vec![Origin::Request; 3]);
    h.worker.release.add_permits(3);
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
async fn compactor_rules_exclude_worker_sections() {
    let mut h = Harness::setup(Setup {
        worker: true,
        compactor: true,
        keep_recent_turns: 2,
        ..Setup::default()
    })
    .await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    for q in ["質問1", "質問2", "質問3"] {
        h.say(GENERAL, q).await;
    }
    let main_rules = h.prompts()[0].harness_rules.clone();
    assert!(main_rules.contains("Worker に頼めること"));
    let later = Timestamp::now() + jiff::SignedDuration::from_mins(11);
    h.actor.check_idle(later).await.unwrap();
    assert_eq!(h.process_next_job().await, "compact");
    let calls = h.compactor.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert!(!calls[0].0.contains("Worker に頼めること"));
    assert!(!calls[0].0.contains("[Goal]"));
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
    h.router.set(with_delivery(spawn_answers(), "abort"));
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
                    origin: Origin::Request,
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
    router.set(with_delivery(spawn_answers(), "normal_report"));
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
        commands: Vec::new(),
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
    h.router.set(with_delivery(spawn_answers(), "abort"));
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
    h.router.set(with_delivery(spawn_answers(), "abort"));
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
    h.router
        .set(with_delivery(spawn_answers(), "normal_report"));
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
        commands: Vec::new(),
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

fn memory_file(h: &Harness) -> PathBuf {
    h.home.join("profiles").join("memory").join("MEMORY.md")
}

#[tokio::test]
async fn memo_line_is_stripped_from_post_kept_in_layer3_and_written_to_file() {
    let mut h = Harness::setup(Setup {
        compactor: true,
        keep_recent_turns: 2,
        ..Setup::default()
    })
    .await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.provider
        .script_text("了解です\n[Memo] alice は猫が好き\n[Memo] 文中ではなく行頭の2件目");
    h.say(GENERAL, "質問1").await;

    assert_eq!(h.sent(), vec![(GENERAL, "了解です".to_string())]);
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(
        turns[1].text,
        "了解です\n[Memo] alice は猫が好き\n[Memo] 文中ではなく行頭の2件目"
    );
    let file = std::fs::read_to_string(memory_file(&h)).unwrap();
    assert!(file.contains("- alice は猫が好き <!-- observed: 20"));
    assert!(file.contains("- 文中ではなく行頭の2件目 <!-- observed: 20"));
    let versions = h.store.memory_versions(GUILD).await.unwrap();
    assert_eq!(versions, vec![("memo".to_string(), String::new())]);

    // 追記しても Layer 1.5 は差し替えない(キャッシュ保護)
    h.say(GENERAL, "質問2").await;
    h.say(GENERAL, "質問3").await;
    let prompts = h.prompts();
    assert!(prompts.iter().all(|p| p.memory.is_empty()));
    assert!(prompts[0].harness_rules.contains("[Memo]"));

    // コンパクション後に読み直す
    let later = Timestamp::now() + jiff::SignedDuration::from_mins(11);
    h.actor.check_idle(later).await.unwrap();
    assert_eq!(h.process_next_job().await, "compact");
    h.say(GENERAL, "質問4").await;
    let last = h.prompts().last().unwrap().clone();
    assert!(last.memory.starts_with("# Noth の記憶"));
    assert!(last.memory.contains("alice は猫が好き"));
}

#[tokio::test]
async fn memo_only_reply_posts_nothing_but_is_remembered() {
    let mut h = Harness::new().await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.provider.script_text("[Memo] bob は夜型");
    h.say(GENERAL, "Noth、bob は夜型らしいよ").await;

    assert!(h.sent().is_empty());
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1].text, "[Memo] bob は夜型");
    assert!(
        std::fs::read_to_string(memory_file(&h))
            .unwrap()
            .contains("bob は夜型")
    );
}

#[tokio::test]
async fn member_memo_goes_under_the_member_subsection() {
    let mut h = Harness::new().await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.provider
        .script_text("返事\n[Memo alice] 猫が好き\n[Memo 知らない人] 謎\n[Memo] Noth は Bot");
    h.say(GENERAL, "質問1").await;

    assert_eq!(h.sent(), vec![(GENERAL, "返事".to_string())]);
    let file = std::fs::read_to_string(memory_file(&h)).unwrap();
    assert!(file.contains("### alice <!-- id: 1 -->\n- 猫が好き <!-- observed: 20"));
    assert!(file.contains("- [知らない人] 謎 <!-- observed: 20"));
    assert!(file.contains("- Noth は Bot <!-- observed: 20"));
}

#[tokio::test]
async fn memo_limits_and_disabled_setting() {
    let mut h = Harness::new().await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.provider
        .script_text("返事\n[Memo] a\n[Memo] b\n[Memo] c\n[Memo] d");
    h.say(GENERAL, "質問1").await;
    assert_eq!(h.sent(), vec![(GENERAL, "返事".to_string())]);
    let file = std::fs::read_to_string(memory_file(&h)).unwrap();
    assert_eq!(file.matches("<!-- observed:").count(), 3);
    assert!(!file.contains("- d "));
}

#[tokio::test]
async fn memo_in_ack_is_stripped_and_goal_keeps_its_own_memo_lines() {
    let mut h = Harness::setup(Setup {
        worker: true,
        worker_permits: 0,
        ..Setup::default()
    })
    .await;
    h.router.set(spawn_answers());
    h.provider.script_text(
        "調べるね\n[Memo] carol は Rust 勉強中\n[Goal]\n調査\n[Memo] これは Goal の一部",
    );
    h.say(GENERAL, "Rust の最新版を調べて").await;

    assert_eq!(h.sent(), vec![(GENERAL, "調べるね".to_string())]);
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(
        turns[1].text,
        "調べるね\n[Memo] carol は Rust 勉強中\n[Goal]\n調査\n[Memo] これは Goal の一部"
    );
    let file = std::fs::read_to_string(memory_file(&h)).unwrap();
    assert!(file.contains("carol は Rust 勉強中"));
    assert!(!file.contains("これは Goal の一部"));
    let worker = h.worker.clone();
    wait_until(|| worker.started() == 1).await;
    assert_eq!(
        h.worker.tasks.lock().unwrap()[0].goal,
        "調査\n[Memo] これは Goal の一部"
    );
    h.worker.release.add_permits(1);
}

#[tokio::test]
async fn router_state_carries_noth_and_members_from_memory_file() {
    let mut h = Harness::new().await;
    let path = memory_file(&h);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        "## Noth について\nNoth は Discord の Bot\n\n## メンバー\n- 小見出しの外\n\n### alice <!-- id: 1 -->\n- 開発者 <!-- observed: 2026-10-01 -->\n\n## サーバー\n\n## メモ\n",
    )
    .unwrap();
    h.router.set(answers("silent", 0.0, 0.0));
    h.say(GENERAL, "雑談").await;

    let state = h.router.states.lock().unwrap()[0].0.clone();
    assert_eq!(state["noth"], "Noth は Discord の Bot");
    // 小見出しの外の項目は state に入れない
    assert_eq!(
        state["members"]["list"],
        serde_json::json!([{"name": "alice", "roles": ["admin"], "memory": "- 開発者"}])
    );
}

#[tokio::test]
async fn memo_disabled_leaves_text_untouched_and_writes_nothing() {
    let mut h = Harness::setup(Setup {
        memo: false,
        ..Setup::default()
    })
    .await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.provider.script_text("返事\n[Memo] alice は猫好き");
    h.say(GENERAL, "質問1").await;

    assert_eq!(
        h.sent(),
        vec![(GENERAL, "返事\n[Memo] alice は猫好き".to_string())]
    );
    assert!(!memory_file(&h).exists());
    assert!(h.store.memory_versions(GUILD).await.unwrap().is_empty());
    assert!(!h.prompts()[0].harness_rules.contains("[Memo]"));
}

#[tokio::test]
async fn memo_only_join_report_is_not_posted_but_is_remembered() {
    let mut h = Harness::with_worker().await;
    h.router
        .set(with_delivery(spawn_answers(), "normal_report"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.provider.script_text("[Memo] 東京は雨が多い");
    h.say(GENERAL, "天気調べて").await;
    assert_eq!(h.process_next_job().await, "join");

    // Ack だけが投稿され、Layer 3 には結論メモだけが残る(投稿していない応答は残さない)
    assert_eq!(h.sent().len(), 1);
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[2].kind, TurnKind::Conclusion);
    assert!(
        std::fs::read_to_string(memory_file(&h))
            .unwrap()
            .contains("東京は雨が多い")
    );
}

#[tokio::test]
async fn memo_in_plain_reply_with_worker_enabled_is_stripped() {
    let mut h = Harness::with_worker().await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.provider.script_text("そうなんですね\n[Memo] dave は朝型");
    h.say(GENERAL, "私は朝型です").await;

    assert_eq!(h.sent(), vec![(GENERAL, "そうなんですね".to_string())]);
    assert!(
        std::fs::read_to_string(memory_file(&h))
            .unwrap()
            .contains("dave は朝型")
    );
}

#[tokio::test]
async fn posted_join_report_strips_memo_and_appends_it() {
    let mut h = Harness::with_worker().await;
    h.router
        .set(with_delivery(spawn_answers(), "normal_report"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.provider.script_text("晴れでした\n[Memo] 東京は今日晴れ");
    h.say(GENERAL, "天気調べて").await;
    assert_eq!(h.process_next_job().await, "join");

    let sent = h.sent();
    assert_eq!(sent[1].1, "晴れでした");
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns[3].text, "晴れでした\n[Memo] 東京は今日晴れ");
    assert!(
        std::fs::read_to_string(memory_file(&h))
            .unwrap()
            .contains("東京は今日晴れ")
    );
}

#[tokio::test]
async fn name_call_follows_addressee_but_mention_does_not() {
    let mut h = Harness::new().await;
    h.router.set(answers("silent", 0.0, 0.0));
    h.say(GENERAL, "noth〜？").await;
    assert!(h.sent().is_empty(), "名前だけでは決定的に応答しない");

    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "noth〜？").await;
    assert_eq!(h.sent().len(), 1);
}

#[tokio::test]
async fn conversation_window_is_a_hint_to_router_not_a_reply_rule() {
    let mut h = Harness::new().await;
    h.router.set(answers("silent", 0.0, 0.0));
    h.say_as(GENERAL, "@Noth", true).await;
    assert_eq!(h.sent().len(), 1);

    h.say(GENERAL, "ココナッツ食べたことない").await;
    assert_eq!(
        h.sent().len(),
        1,
        "窓の中でも router が other と答えれば静観"
    );
    h.say(RANDOM, "別チャンネル").await;

    let states = h.router.states.lock().unwrap().clone();
    let window = |i: usize| states[i].0["trigger"][0]["in_conversation_window"].clone();
    assert_eq!(window(0), false);
    assert_eq!(window(1), true);
    assert_eq!(window(2), false);

    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "ココナッツ食べたことない").await;
    assert_eq!(h.sent().len(), 2);
}

#[tokio::test]
async fn other_never_reacts_even_with_reaction_answer() {
    let mut h = Harness::new().await;
    let mut a = answers("silent", 0.0, 0.0);
    a.push(("reaction", reaction("r0")));
    h.router.set(a);
    h.say(GENERAL, "新しいキーボード買った").await;

    assert!(h.channel.reactions.lock().unwrap().is_empty());
    assert!(h.sent().is_empty());
}

fn bot_marker_message(channel_id: ChannelId, message_id: MessageId) -> IncomingMessage {
    let mut msg = incoming(channel_id, "general", "", message_id);
    msg.user_id = 500;
    msg.author_name = "秘書ちゃん".into();
    msg.bot_marker = Some(BotMarker {
        author_name: "秘書ちゃん".into(),
        reply_to_author: Some("alice".into()),
        chars: 1180,
    });
    msg
}

#[tokio::test]
async fn ingest_stores_bot_marker_without_starting_a_judge() {
    let store = Store::open_memory().await.unwrap();
    let (bus, rx) = bus::channel(16);
    let (jobs_tx, mut jobs_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    tokio::spawn(noth_core::ingest::run(
        store.clone(),
        Arc::new(MockChannel::default()),
        rx,
        Duration::from_millis(50),
        jobs_tx,
    ));

    bus.publish(BusEvent::MessageReceived(bot_marker_message(GENERAL, 1)))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(400), jobs_rx.recv())
            .await
            .is_err(),
        "目印だけでは判定を起動しない"
    );
    let stored = store.recent_messages(GUILD, i64::MAX, 10).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert!(stored[0].bot_marker.is_some() && stored[0].content.is_empty());
    assert!(store.unjudged_messages(GUILD).await.unwrap().is_empty());
}

#[tokio::test]
async fn bot_marker_rides_along_with_the_next_judge() {
    let mut h = Harness::new().await;
    h.store
        .insert_message(&bot_marker_message(GENERAL, 900))
        .await
        .unwrap();
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say_as(GENERAL, "@Noth これ見て", true).await;

    let states = h.router.states.lock().unwrap().clone();
    let recent = &states[0].0["recent"];
    assert_eq!(
        recent[0]["bot_marker"],
        serde_json::json!({"chars": 1180, "count": 1, "reply_to": "alice"})
    );
    assert!(recent[0].get("content").is_none());

    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert!(turns.iter().any(|t| {
        t.text.contains(
            "[Bot 秘書ちゃん が発言(alice への返信、約1,200字。embed を含む) message_id: 900]",
        )
    }));
    assert!(h.store.buffered_messages(GUILD).await.unwrap().is_empty());
}

#[tokio::test]
async fn bot_marker_between_batch_messages_reaches_router_state() {
    let mut h = Harness::new().await;
    h.router.set(answers("silent", 0.0, 0.0));
    let mut ids = Vec::new();
    let mut pending = Vec::new();
    for (i, content) in ["一つ目", "二つ目"].into_iter().enumerate() {
        let msg = incoming(GENERAL, "general", content, 700 + i as u64 * 2);
        let row_id = h.store.insert_message(&msg).await.unwrap();
        ids.push(row_id);
        pending.push(PendingMessage {
            row_id,
            message_id: msg.message_id,
            author_name: "alice".into(),
            content: content.into(),
            at: msg.timestamp,
            mentions_self: false,
            reply_to_self: false,
            reply_to: None,
        });
        if i == 0 {
            h.store
                .insert_message(&bot_marker_message(GENERAL, 701))
                .await
                .unwrap();
        }
    }
    h.actor
        .handle_judge(JudgeBatch {
            channel_id: GENERAL,
            channel_name: "general".into(),
            messages: pending,
        })
        .await
        .unwrap();

    let states = h.router.states.lock().unwrap().clone();
    let recent = states[0].0["recent"].as_array().unwrap();
    assert!(
        recent.iter().any(|e| e.get("bot_marker").is_some()),
        "バッチの間に挟まった目印が recent に出る: {recent:?}"
    );
}

#[tokio::test]
async fn message_older_than_the_reply_is_not_in_the_conversation_window() {
    let mut h = Harness::new().await;
    h.router.set(answers("silent", 0.0, 0.0));
    h.say_as(GENERAL, "@Noth", true).await;

    let mut msg = incoming(GENERAL, "general", "前の発言", 800);
    msg.timestamp = Timestamp::UNIX_EPOCH;
    let row_id = h.store.insert_message(&msg).await.unwrap();
    h.actor
        .handle_judge(JudgeBatch {
            channel_id: GENERAL,
            channel_name: "general".into(),
            messages: vec![PendingMessage {
                row_id,
                message_id: 800,
                author_name: "alice".into(),
                content: "前の発言".into(),
                at: msg.timestamp,
                mentions_self: false,
                reply_to_self: false,
                reply_to: None,
            }],
        })
        .await
        .unwrap();
    let states = h.router.states.lock().unwrap().clone();
    assert_eq!(states[1].0["trigger"][0]["in_conversation_window"], false);
}

#[tokio::test]
async fn corrections_do_not_use_up_the_daily_supplement_budget() {
    let mut h = Harness::with_worker().await;
    let old = |i: u32| NewWorkerRun {
        run_id: format!("old{i}"),
        goal: "g".into(),
        origin: Origin::Supplement,
        channel_id: 999,
        chunk_id: 1,
        trigger_message_id: None,
        fork_message_id: 0,
    };
    // 上限 50 の残りが 1 回
    for i in 0..49 {
        h.store.insert_worker_run(GUILD, &old(i)).await.unwrap();
    }
    h.router.set(correction_answers());
    let worker = h.worker.clone();
    for (i, text) in ["それ違うよ", "いや違うって", "やっぱり違う"]
        .into_iter()
        .enumerate()
    {
        h.say(GENERAL, text).await;
        wait_until(|| worker.started() == i + 1).await;
        assert_eq!(h.process_next_job().await, "join");
    }
    h.router.set(supplement_answers());
    h.say(RANDOM, "別の話 $5 らしい").await;
    wait_until(|| worker.started() == 4).await;
}

#[tokio::test]
async fn silent_run_is_not_started_between_finish_and_join() {
    let mut h = Harness::with_worker().await;
    h.router.set(supplement_answers());
    h.say(GENERAL, "プランは月 $10 らしい").await;
    let worker = h.worker.clone();
    wait_until(|| worker.started() == 1).await;
    // Worker は finished にして Join を積んだが、actor はまだ Join を処理していない
    tokio::time::timeout(Duration::from_secs(5), async {
        while h
            .store
            .worker_runs(GUILD, &[RunStatus::Finished])
            .await
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("run が finished にならない");
    h.router.set(supplement_answers());
    h.say(GENERAL, "上限は 100 件だって").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.worker.started(), 1);
}

fn with_answer(
    mut a: Vec<(&'static str, Answer)>,
    key: &'static str,
    answer: Answer,
) -> Vec<(&'static str, Answer)> {
    a.retain(|(k, _)| *k != key);
    a.push((key, answer));
    a
}

fn reactions_of(h: &Harness) -> Vec<(ChannelId, MessageId, String)> {
    h.channel.reactions.lock().unwrap().clone()
}

#[tokio::test]
async fn noise_never_enters_buffer_but_a_request_is_kept() {
    let mut h = Harness::new().await;
    h.router.set(answers("silent", 0.95, 0.0));
    h.say(GENERAL, "wwwwwww").await;
    assert!(h.store.buffered_messages(GUILD).await.unwrap().is_empty());

    // ノイズと判定されても Noth への依頼なら応答を優先し、Layer 3 に入れる
    h.router.set(answers("reply_only", 0.95, 0.0));
    h.say(GENERAL, "Noth").await;
    assert_eq!(h.sent().len(), 1);
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert!(turns.iter().any(|t| t.text.contains("Noth")));
    assert!(turns.iter().all(|t| !t.text.contains("wwwwwww")));
}

#[tokio::test]
async fn noise_does_not_open_a_new_chunk() {
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
}

#[tokio::test]
async fn pattern_c_without_goal_withholds_reply_and_starts_worker_with_eyes() {
    let mut h = Harness::setup(Setup {
        worker: true,
        worker_permits: 0,
        ..Setup::default()
    })
    .await;
    h.router.set(spawn_answers());
    h.provider.script_text("たぶん晴れですよ");
    h.say(GENERAL, "明日の東京の天気わかる?").await;

    assert!(h.sent().is_empty(), "main の返事は投稿しない");
    assert_eq!(reactions_of(&h), vec![(GENERAL, 1001, "👀".to_string())]);
    let worker = h.worker.clone();
    wait_until(|| worker.started() == 1).await;
    let task = h.worker.tasks.lock().unwrap()[0].clone();
    assert_eq!(task.origin, Origin::Request);
    assert!(task.goal.contains("明日の東京の天気わかる?"));
    assert!(task.goal.contains("alice"));

    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].kind, TurnKind::Trigger);
    assert!(turns[1].text.starts_with("[Goal]\n"));
    assert!(turns.iter().all(|t| !t.text.contains("たぶん晴れ")));
    h.worker.release.add_permits(1);
}

#[tokio::test]
async fn spawn_hint_tells_main_to_write_ack_and_goal() {
    let mut h = Harness::with_worker().await;
    h.router.set(spawn_answers());
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    let hint = h.prompts()[0].ephemeral_tail.last().unwrap().text.clone();
    assert!(hint.contains("Ack と `[Goal]` を書いて"));
}

fn supplement_answers() -> Vec<(&'static str, Answer)> {
    with_answer(answers("silent", 0.0, 0.0), "worth_supplement", noul(0.9))
}

fn correction_answers() -> Vec<(&'static str, Answer)> {
    with_answer(answers("silent", 0.0, 0.0), "corrects_noth", noul(0.9))
}

#[tokio::test]
async fn supplement_runs_silently_and_replies_to_the_last_trigger_when_needed() {
    let mut h = Harness::with_worker().await;
    *h.worker.conclusion.lock().unwrap() = Some("調べた\n補足の要否: 要\n確信: 高".into());
    h.router
        .set(with_delivery(supplement_answers(), "normal_report"));
    h.provider.script_text("料金は月 $20 です");
    h.say(GENERAL, "プランは月 $10 らしい").await;

    assert!(h.sent().is_empty(), "Ack は出さない");
    assert!(reactions_of(&h).is_empty());
    let worker = h.worker.clone();
    wait_until(|| worker.started() == 1).await;
    let task = h.worker.tasks.lock().unwrap()[0].clone();
    assert_eq!(task.origin, Origin::Supplement);
    assert!(task.goal.contains("プランは月 $10 らしい"));
    assert!(h.prompts().is_empty(), "main は呼ばない");

    assert_eq!(h.process_next_job().await, "join");
    assert_eq!(h.sent(), vec![(GENERAL, "料金は月 $20 です".to_string())]);
    assert_eq!(*h.channel.reply_to.lock().unwrap(), vec![Some(1001)]);
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert!(turns.iter().any(|t| t.kind == TurnKind::Conclusion));
    assert_eq!(turns.last().unwrap().text, "料金は月 $20 です");
    let runs = h
        .store
        .worker_runs(GUILD, &[RunStatus::Finished])
        .await
        .unwrap();
    assert_eq!(runs[0].origin, Origin::Supplement);
}

#[tokio::test]
async fn supplement_is_not_posted_unless_confidence_is_high() {
    for memo in [
        "調べた\n補足の要否: 要\n確信: 中",
        "調べた\n補足の要否: 要\n確信: 低",
        "調べた\n補足の要否: 要",
    ] {
        let mut h = Harness::with_worker().await;
        *h.worker.conclusion.lock().unwrap() = Some(memo.into());
        h.router
            .set(with_delivery(supplement_answers(), "normal_report"));
        h.provider.script_text("料金は月 $20 です");
        h.say(GENERAL, "プランは月 $10 らしい").await;
        assert_eq!(h.process_next_job().await, "join");

        assert!(h.sent().is_empty(), "{memo}");
        assert!(h.prompts().is_empty(), "main は呼ばない: {memo}");
        let turns = h.store.active_turns(GUILD).await.unwrap();
        assert!(turns.iter().any(|t| t.kind == TurnKind::Conclusion));
    }
}

#[tokio::test]
async fn correction_without_human_follow_up_still_asks_the_join_judge() {
    let mut h = Harness::with_worker().await;
    *h.worker.conclusion.lock().unwrap() =
        Some("確かに指摘どおり\n補足の要否: 要\n確信: 高".into());
    h.router.set(with_delivery(correction_answers(), "abort"));
    h.provider.script_text("確かにそうでした");
    h.say(GENERAL, "それ違うよ").await;
    let judged = h.router.states.lock().unwrap().len();
    assert_eq!(h.process_next_job().await, "join");

    assert_eq!(
        h.router.states.lock().unwrap().len(),
        judged + 1,
        "後続が無くても合流判定を呼ぶ"
    );
    assert!(h.sent().is_empty(), "abort なら投稿しない");

    let mut h = Harness::with_worker().await;
    *h.worker.conclusion.lock().unwrap() =
        Some("確かに指摘どおり\n補足の要否: 要\n確信: 高".into());
    h.router
        .set(with_delivery(correction_answers(), "normal_report"));
    h.provider.script_text("確かにそうでした");
    h.say(GENERAL, "それ違うよ").await;
    assert_eq!(h.process_next_job().await, "join");
    assert_eq!(h.sent(), vec![(GENERAL, "確かにそうでした".to_string())]);
}

#[tokio::test]
async fn correction_acknowledgement_posts_without_high_confidence() {
    let mut h = Harness::with_worker().await;
    *h.worker.conclusion.lock().unwrap() =
        Some("指摘どおり\n補足の要否: 要\n種類: 認める\n確信: 低".into());
    h.router
        .set(with_delivery(correction_answers(), "normal_report"));
    h.provider
        .script_text("無料になってたんですね、失礼しました");
    h.say(GENERAL, "ここ数日で無料になったよ").await;
    assert_eq!(h.process_next_job().await, "join");
    assert_eq!(h.sent().len(), 1);
    let hint = h
        .prompts()
        .last()
        .unwrap()
        .ephemeral_tail
        .last()
        .unwrap()
        .text
        .clone();
    assert!(hint.contains("ぼかした書き方の禁止の例外"));
}

#[tokio::test]
async fn addition_posts_at_medium_confidence_but_rebuttal_does_not() {
    for (kind, posted) in [("追加", true), ("反論", false)] {
        let mut h = Harness::with_worker().await;
        *h.worker.conclusion.lock().unwrap() =
            Some(format!("調べた\n補足の要否: 要\n種類: {kind}\n確信: 中"));
        h.router
            .set(with_delivery(supplement_answers(), "normal_report"));
        h.provider.script_text("答えは $20 です");
        h.say(GENERAL, "プランはいくら？").await;
        assert_eq!(h.process_next_job().await, "join");
        assert_eq!(!h.sent().is_empty(), posted, "{kind}");
    }
}

#[tokio::test]
async fn silent_run_records_the_tool_log() {
    let mut h = Harness::with_worker().await;
    h.router.set(supplement_answers());
    *h.worker.progress.lock().unwrap() = vec![
        (
            Duration::ZERO,
            WorkerProgress {
                tool: "web_search".into(),
                summary: "「OpenCode Zen 無料」".into(),
            },
        ),
        (
            Duration::ZERO,
            WorkerProgress {
                tool: "web_fetch".into(),
                summary: "https://opencode.ai/docs/zen/".into(),
            },
        ),
    ];
    h.say(GENERAL, "プランは月 $10 らしい").await;
    assert_eq!(h.process_next_job().await, "join");

    let run_id = h.worker.tasks.lock().unwrap()[0].run_id.clone();
    let log = h.store.tool_log(&run_id).await.unwrap().unwrap();
    let v: serde_json::Value = serde_json::from_str(&log).unwrap();
    assert_eq!(v[0]["tool"], "web_search");
    assert_eq!(v[1]["summary"], "https://opencode.ai/docs/zen/");
}

#[tokio::test]
async fn supplement_not_needed_keeps_conclusion_in_layer3_without_posting() {
    let mut h = Harness::with_worker().await;
    *h.worker.conclusion.lock().unwrap() = Some("正しかった\n補足の要否: 不要".into());
    h.router.set(supplement_answers());
    h.say(GENERAL, "Rust 2024 edition は最近出た").await;
    assert_eq!(h.process_next_job().await, "join");

    assert!(h.sent().is_empty());
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert!(
        turns
            .iter()
            .any(|t| t.kind == TurnKind::Conclusion && t.text.contains("補足の要否: 不要"))
    );
    assert!(turns.iter().all(|t| t.role != Role::Assistant));
}

#[tokio::test]
async fn supplement_without_human_follow_up_skips_the_join_judge() {
    let mut h = Harness::with_worker().await;
    *h.worker.conclusion.lock().unwrap() = Some("調べた\n補足の要否: 要\n確信: 高".into());
    h.router.set(with_delivery(supplement_answers(), "abort"));
    h.provider.script_text("料金は月 $20 です");
    h.say(GENERAL, "プランは月 $10 らしい").await;
    let judged = h.router.states.lock().unwrap().len();
    assert_eq!(h.process_next_job().await, "join");

    assert_eq!(h.router.states.lock().unwrap().len(), judged);
    assert_eq!(h.sent(), vec![(GENERAL, "料金は月 $20 です".to_string())]);
}

#[tokio::test]
async fn supplement_is_dropped_when_the_join_judge_aborts() {
    let mut h = Harness::with_worker().await;
    *h.worker.conclusion.lock().unwrap() = Some("調べた\n補足の要否: 要\n確信: 高".into());
    h.router.set(with_delivery(supplement_answers(), "abort"));
    h.say(GENERAL, "プランは月 $10 らしい").await;
    h.record_human(GENERAL, "それは違うよ、$20 だ").await;
    assert_eq!(h.process_next_job().await, "join");

    assert!(h.sent().is_empty());
    assert!(h.prompts().is_empty(), "abort なら main を呼ばない");
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert!(turns.iter().any(|t| t.kind == TurnKind::Conclusion));
    assert!(turns.iter().all(|t| t.role != Role::Assistant));
}

#[tokio::test]
async fn supplement_is_dropped_when_main_writes_nothing() {
    let mut h = Harness::with_worker().await;
    *h.worker.conclusion.lock().unwrap() = Some("調べた\n補足の要否: 要\n確信: 高".into());
    h.router
        .set(with_delivery(supplement_answers(), "normal_report"));
    h.provider.script_text("");
    h.say(GENERAL, "プランは月 $10 らしい").await;
    assert_eq!(h.process_next_job().await, "join");

    assert!(h.sent().is_empty());
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert!(turns.iter().any(|t| t.kind == TurnKind::Conclusion));
    assert!(turns.iter().all(|t| t.role != Role::Assistant));
}

#[tokio::test]
async fn noise_does_not_start_a_supplement() {
    let mut h = Harness::with_worker().await;
    h.router.set(with_answer(
        supplement_answers(),
        "is_noise_or_meme",
        noul(0.9),
    ));
    h.say(GENERAL, "www").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.worker.started(), 0);
}

#[tokio::test]
async fn failed_silent_run_leaves_no_trace_in_conversation() {
    let mut h = Harness::with_worker().await;
    h.worker.fail.store(true, Ordering::SeqCst);
    h.router.set(supplement_answers());
    h.say(GENERAL, "プランは月 $10 らしい").await;
    assert_eq!(h.process_next_job().await, "join");
    assert!(h.sent().is_empty());
    assert!(h.store.active_turns(GUILD).await.unwrap().is_empty());
}

#[tokio::test]
async fn correction_runs_silently_without_counting_toward_supplement_budget() {
    let mut h = Harness::with_worker().await;
    h.router.set(correction_answers());
    let worker = h.worker.clone();
    // 動作中の黙った Worker があると同じチャンネルでは起動しないので、1件ずつ合流させる
    for (i, text) in ["それ違うよ", "いや違うって", "やっぱり違う"]
        .into_iter()
        .enumerate()
    {
        h.say(GENERAL, text).await;
        wait_until(|| worker.started() == i + 1).await;
        assert_eq!(h.process_next_job().await, "join");
    }
    assert!(
        h.worker
            .tasks
            .lock()
            .unwrap()
            .iter()
            .all(|t| t.origin == Origin::Correction)
    );
    assert!(h.sent().is_empty());

    // 訂正は最短間隔にも数えられないので、その後の補足は起動できる
    h.router.set(supplement_answers());
    h.say(GENERAL, "プランは月 $10 らしい").await;
    wait_until(|| worker.started() == 4).await;
}

#[tokio::test]
async fn silent_run_is_not_started_while_another_is_active_in_the_channel() {
    let mut h = Harness::with_worker().await;
    h.router.set(supplement_answers());
    h.say(GENERAL, "プランは月 $10 らしい").await;
    let worker = h.worker.clone();
    wait_until(|| worker.started() == 1).await;
    h.say(GENERAL, "上限は 100 件だって").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.worker.started(), 1);
}

#[tokio::test]
async fn supplement_respects_min_interval_per_channel_and_daily_cap() {
    let mut h = Harness::with_worker().await;
    h.router.set(supplement_answers());
    h.say(GENERAL, "プランは月 $10 らしい").await;
    h.say(GENERAL, "上限は 100 件だって").await;
    h.say(RANDOM, "別のチャンネルの話 $5 らしい").await;
    let worker = h.worker.clone();
    wait_until(|| worker.started() == 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        h.worker.started(),
        2,
        "同じチャンネルは最短間隔内なので補足しない"
    );
    let channels: Vec<_> = h
        .worker
        .tasks
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.audience.channel_id)
        .collect();
    assert_eq!(channels, vec![Some(GENERAL), Some(RANDOM)]);

    // 再起動後も worker_runs から数え直せる: 上限まで埋まっていれば、間隔が空いたチャンネルでも補足しない
    let run = |i: u32| NewWorkerRun {
        run_id: format!("old{i}"),
        goal: "g".into(),
        origin: Origin::Supplement,
        channel_id: 999,
        chunk_id: 1,
        trigger_message_id: None,
        fork_message_id: 0,
    };
    for i in 0..48 {
        h.store.insert_worker_run(GUILD, &run(i)).await.unwrap();
    }
    h.router.set(supplement_answers());
    h.say(777, "三つ目のチャンネル $1 らしい").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        h.worker.started(),
        2,
        "1 日の上限(50)に達したので補足しない"
    );
}

#[tokio::test]
async fn empty_reply_with_router_reaction_reacts_and_records_it_in_layer3() {
    let mut h = Harness::new().await;
    h.router.set(with_answer(
        answers("reply_only", 0.0, 0.0),
        "reaction",
        reaction("r0"),
    ));
    h.provider.script_text("");
    h.say(GENERAL, "ありがとう Noth").await;

    assert!(h.sent().is_empty());
    assert_eq!(reactions_of(&h), vec![(GENERAL, 1001, "👀".to_string())]);
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1].role, Role::Assistant);
    assert_eq!(turns[1].text, "[リアクション 👀]");
}

#[tokio::test]
async fn main_can_choose_the_reaction_and_unlisted_emoji_is_ignored() {
    let mut h = Harness::new().await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.provider.script_text("[React] 👀");
    h.say(GENERAL, "ありがとう Noth").await;
    assert_eq!(reactions_of(&h), vec![(GENERAL, 1001, "👀".to_string())]);

    h.provider.script_text("[React] 🎉");
    h.say(GENERAL, "どういたしまして Noth").await;
    assert_eq!(
        reactions_of(&h).len(),
        1,
        "許可されていない絵文字は付けない"
    );
    assert!(h.sent().is_empty(), "[React] の行は本文に出ない");
}

#[tokio::test]
async fn reply_with_react_line_posts_text_and_reacts() {
    let mut h = Harness::new().await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.provider.script_text("どういたしまして\n[React] 👀");
    h.say(GENERAL, "ありがとう Noth").await;
    assert_eq!(h.sent(), vec![(GENERAL, "どういたしまして".to_string())]);
    assert_eq!(reactions_of(&h).len(), 1);
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert_eq!(turns[1].text, "どういたしまして");
}

fn marker() -> BotMarker {
    BotMarker {
        author_name: "bot".into(),
        reply_to_author: None,
        chars: 3,
    }
}

/// 依頼を出して Join の直前まで進める。router は reply_anchor を返す設定
async fn request_with_anchor_router() -> Harness {
    let mut h = Harness::with_worker().await;
    h.router.set(with_delivery(spawn_answers(), "reply_anchor"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.say(GENERAL, "天気調べて").await;
    h
}

#[tokio::test]
async fn join_without_follow_up_does_not_call_router() {
    let mut h = request_with_anchor_router().await;
    let before = h.router_calls();
    h.process_next_job().await;
    assert_eq!(h.router_calls(), before);
    assert_eq!(h.sent().len(), 2);
}

#[tokio::test]
async fn failed_run_without_follow_up_is_still_reported() {
    let mut h = Harness::with_worker().await;
    h.worker.fail.store(true, Ordering::SeqCst);
    h.router.set(with_delivery(spawn_answers(), "abort"));
    h.provider.script_text("調べるね\n[Goal]\n天気");
    h.provider.script_text("調べられなかったよ");
    h.say(GENERAL, "天気調べて").await;
    let before = h.router_calls();
    h.process_next_job().await;
    assert_eq!(h.router_calls(), before);
    assert_eq!(h.sent().len(), 2);
    let run = &runs(&h.store, RunStatus::Failed).await[0];
    assert_eq!(run.delivery_style.as_deref(), Some("normal_report"));
}

#[tokio::test]
async fn silent_failed_run_posts_nothing() {
    let mut h = Harness::with_worker().await;
    h.worker.fail.store(true, Ordering::SeqCst);
    h.router
        .set(with_delivery(supplement_answers(), "normal_report"));
    h.say(GENERAL, "プランは月 $10 らしい").await;
    h.process_next_job().await;
    assert!(h.sent().is_empty());
    assert!(h.prompts().is_empty());
}

#[tokio::test]
async fn other_channel_human_does_not_count_but_requester_does() {
    let mut h = request_with_anchor_router().await;
    let before = h.router_calls();
    h.record_from(RANDOM, 2, "別の場所の別の人", None).await;
    h.process_next_job().await;
    assert_eq!(h.router_calls(), before);

    let mut h = request_with_anchor_router().await;
    let before = h.router_calls();
    h.record_from(RANDOM, 1, "さっきのいらなかった", None).await;
    h.process_next_job().await;
    assert_eq!(h.router_calls(), before + 1);
}

#[tokio::test]
async fn bot_markers_alone_are_not_follow_up() {
    let mut h = request_with_anchor_router().await;
    let before = h.router_calls();
    h.record_from(GENERAL, 9, "", Some(marker())).await;
    h.process_next_job().await;
    assert_eq!(h.router_calls(), before);
}

#[tokio::test]
async fn follow_up_pushed_out_of_the_router_window_is_still_counted() {
    let mut h = request_with_anchor_router().await;
    let before = h.router_calls();
    h.record_human(GENERAL, "話題を変えるね").await;
    for _ in 0..40 {
        h.record_from(RANDOM, 9, "", Some(marker())).await;
    }
    h.process_next_job().await;
    assert_eq!(h.router_calls(), before + 1);
}

const STALE_RUN: &str = "1792170173263-9";

/// 片付けの期限判定は最終更新を見るので、中身も含めて 30 日前にする
fn make_old(path: &std::path::Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            for e in std::fs::read_dir(&p).unwrap() {
                stack.push(e.unwrap().path());
            }
        }
        std::fs::File::open(&p).unwrap().set_modified(old).unwrap();
    }
}

fn seed(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    make_old(dir);
}

#[tokio::test]
async fn sweep_keeps_active_run_and_workspace_but_removes_stale_ones() {
    let mut h = Harness::setup(Setup {
        worker: true,
        worker_permits: 0,
        ..Setup::default()
    })
    .await;
    h.router.set(spawn_answers());
    h.provider
        .script_text("ちょっと調べるね\n[Goal]\n東京の明日の天気を調べる");
    h.say(GENERAL, "明日の東京の天気わかる?").await;
    let worker = h.worker.clone();
    wait_until(|| worker.started() == 1).await;
    let task = h.worker.tasks.lock().unwrap()[0].clone();

    // shell が作った下書きが、期限を過ぎていても動いている run のものなら消さない
    seed(&task.work_dir);
    let stale = h.home.join("work").join(STALE_RUN);
    seed(&stale);
    let ws_file = h.home.join("workspace").join("tool").join("old.txt");
    std::fs::create_dir_all(ws_file.parent().unwrap()).unwrap();
    std::fs::write(&ws_file, "x").unwrap();
    make_old(ws_file.parent().unwrap());

    h.actor.sweep_work_dirs().unwrap().await.unwrap();
    assert!(task.work_dir.join("a.txt").exists());
    assert!(!stale.exists());
    assert!(ws_file.exists());

    // 終わった run は、Join の前でも次の片付けで対象になる
    h.worker.release.add_permits(1);
    assert_eq!(h.process_next_job().await, "join");
    h.actor.sweep_work_dirs().unwrap().await.unwrap();
    assert!(!task.work_dir.exists());
    assert!(ws_file.exists());
}

#[tokio::test]
async fn empty_work_dir_is_removed_when_the_run_ends() {
    let mut h = Harness::setup(Setup {
        worker: true,
        worker_permits: 0,
        ..Setup::default()
    })
    .await;
    h.router.set(spawn_answers());
    h.provider
        .script_text("ちょっと調べるね\n[Goal]\n東京の明日の天気を調べる");
    h.say(GENERAL, "明日の東京の天気わかる?").await;
    let worker = h.worker.clone();
    wait_until(|| worker.started() == 1).await;
    let task = h.worker.tasks.lock().unwrap()[0].clone();
    // shell が cwd を作っただけで何も置かなかった run
    std::fs::create_dir_all(&task.work_dir).unwrap();
    h.worker.release.add_permits(1);
    wait_until(|| !task.work_dir.exists()).await;
}

#[tokio::test]
async fn startup_sweep_runs_in_the_actor_loop() {
    let store = Store::open_memory().await.unwrap();
    let channel = Arc::new(MockChannel::default());
    let (meter, meter_rx) = tokio::sync::mpsc::unbounded_channel();
    let modules = Modules {
        channel: channel.clone(),
        router: Arc::new(MockRouter::default()),
        provider: Arc::new(MeteredProvider::new(
            Arc::new(MockProvider::new()),
            "provider-mock",
            meter.clone(),
        )),
        worker: Some(Arc::new(MockWorker::new(1)) as Arc<dyn Worker>),
        compactor: None,
        tools: Vec::new(),
        commands: Vec::new(),
        meter: meter.clone(),
        meter_rx,
    };
    let cfg = config(80_000, toml::Table::new());
    let home = cfg.paths.home.clone();
    let stale = home.join("work").join(STALE_RUN);
    seed(&stale);
    let fresh = home.join("work").join("1792170173263-10");
    std::fs::create_dir_all(&fresh).unwrap();
    std::fs::write(fresh.join("a.txt"), "x").unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(noth_core::actor::run(cfg, modules, store, async move {
        let _ = stop_rx.await;
    }));
    wait_until(|| !stale.exists()).await;
    assert!(fresh.join("a.txt").exists());
    assert!(home.join("workspace").is_dir());
    stop_tx.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn restore_skips_rows_of_excluded_channels() {
    let store = Store::open_memory().await.unwrap();
    for (c, n, id) in [(GENERAL, "general", 1), (RANDOM, "random", 2)] {
        store
            .insert_message(&incoming(c, n, "x", id))
            .await
            .unwrap();
    }
    let ex = noth_core::exclusion::ChannelExclusions::new(GUILD);
    ex.attach(store.clone()).await.unwrap();
    ex.set_expanded([RANDOM]).await;
    let batches = noth_core::restore::restore(&store, GUILD).await.unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].channel_id, GENERAL);
}

#[tokio::test]
async fn promotion_waits_until_the_expansion_is_ready() {
    let mut h = Harness::new().await;
    let ex = noth_core::exclusion::ChannelExclusions::new(GUILD);
    ex.set_config([RANDOM]);
    ex.mark_expansion_pending();
    h.channel.exclusions.set(ex.clone()).unwrap();

    // 介入しない発言は未昇格バッファに残り、次に介入したときに Layer 3 へ昇格する
    h.router.set(answers("silent", 0.0, 0.0));
    h.say(GENERAL, "ただの雑談です").await;
    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(GENERAL, "Noth はどう思う?").await;
    assert_eq!(h.sent().len(), 1);
    let promoted = |turns: Vec<Turn>| turns.iter().any(|t| t.text.contains("ただの雑談"));
    assert!(!promoted(h.store.active_turns(GUILD).await.unwrap()));
    assert_eq!(h.store.buffered_messages(GUILD).await.unwrap().len(), 1);

    ex.set_expanded([RANDOM]).await;
    h.say(GENERAL, "もう一度 Noth に聞くね").await;
    assert!(promoted(h.store.active_turns(GUILD).await.unwrap()));
}

#[tokio::test]
async fn restore_waits_for_the_expansion_before_judging() {
    let store = Store::open_memory().await.unwrap();
    for (c, n, id) in [(GENERAL, "general", 1), (RANDOM, "random", 2)] {
        store
            .insert_message(&incoming(c, n, "x", id))
            .await
            .unwrap();
    }
    let ex = noth_core::exclusion::ChannelExclusions::new(GUILD);
    ex.attach(store.clone()).await.unwrap();
    ex.set_config([RANDOM]);
    ex.mark_expansion_pending();
    let channel = Arc::new(MockChannel::default());
    channel.exclusions.set(ex.clone()).unwrap();
    let router = Arc::new(MockRouter::default());
    router.set(answers("silent", 0.0, 0.0));
    let (meter, meter_rx) = tokio::sync::mpsc::unbounded_channel();
    let modules = Modules {
        channel: channel.clone(),
        router: router.clone(),
        provider: Arc::new(MeteredProvider::new(
            Arc::new(MockProvider::new()),
            "provider-mock",
            meter.clone(),
        )),
        worker: None,
        compactor: None,
        tools: Vec::new(),
        commands: Vec::new(),
        meter: meter.clone(),
        meter_rx,
    };
    let cfg = config(80_000, toml::Table::new());
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(noth_core::actor::run(
        cfg,
        modules,
        store.clone(),
        async move {
            let _ = stop_rx.await;
        },
    ));

    // 展開が済むまでは、どちらのチャンネルの発言も Judge に回らない
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(store.unjudged_messages(GUILD).await.unwrap().len(), 2);

    ex.set_expanded([RANDOM]).await;
    for _ in 0..500 {
        if store.unjudged_messages(GUILD).await.unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(store.unjudged_messages(GUILD).await.unwrap().is_empty());
    // 判定に回ったのは除外していない GENERAL の 1 件だけ
    assert_eq!(router.states.lock().unwrap().len(), 1);
    stop_tx.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn excluded_channel_never_reaches_judge_or_layer3() {
    let mut h = Harness::new().await;
    // 除外の前に判定まで済んでいた、未昇格の発言
    let old = h
        .store
        .insert_message(&incoming(RANDOM, "random", "除外前の発言", 1))
        .await
        .unwrap();
    let decision = h
        .store
        .insert_decision(GUILD, "judge", "m", &serde_json::json!({}), 1)
        .await
        .unwrap();
    let chunk = h
        .store
        .create_chunk(GUILD, RANDOM, Timestamp::now())
        .await
        .unwrap();
    h.store
        .mark_judged(&[old], decision, chunk, false)
        .await
        .unwrap();
    assert_eq!(h.store.buffered_messages(GUILD).await.unwrap().len(), 1);

    // 保存先を付けない共有状態。展開が済んだ後でもキューに残っていた分を、actor 側が落とす経路を通す
    let ex = noth_core::exclusion::ChannelExclusions::new(GUILD);
    ex.set_expanded([RANDOM]).await;
    h.channel.exclusions.set(ex).unwrap();

    h.router.set(answers("reply_only", 0.0, 0.0));
    h.say(RANDOM, "Noth、秘密の話").await;
    assert_eq!(h.router_calls(), 0);
    assert!(h.sent().is_empty());

    h.say(GENERAL, "Noth はどう思う?").await;
    assert_eq!(h.router_calls(), 1);
    assert_eq!(h.sent().len(), 1);
    let turns = h.store.active_turns(GUILD).await.unwrap();
    assert!(!turns.is_empty());
    for t in &turns {
        assert!(
            !t.text.contains("除外前の発言") && !t.text.contains("秘密"),
            "{}",
            t.text
        );
    }
}
