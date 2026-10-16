use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde_json::json;
use tokio::sync::{Semaphore, mpsc, oneshot};

use crate::config::{CompactorSettings, Config};
use crate::error::ErrorKind;
use crate::judge::{
    self, Action, ActionInput, DeliveryStyle, JudgeAnswers, JudgeBatch, PendingMessage,
};
use crate::layers::{self, Line};
use crate::markers::{self, Folded};
use crate::registry::Modules;
use crate::store::{MessageRow, NewWorkerRun, RunStatus, Store, WorkerRunRow};
use crate::traits::{
    Channel, Compactor, Completion, LayeredPrompt, Provider, Router, Worker, WorkerOutcome,
    WorkerTask,
};
use crate::types::*;
use crate::{Error, Result, bus, heartbeat, ingest, memory, progress, restore, supplement};

/// typing インジケータは Discord 側で約 10 秒で消えるので、それより短い間隔で再送する
const TYPING_INTERVAL: Duration = Duration::from_secs(8);
/// 委譲しなかった main の応答をログに出すときの先頭の文字数
const MISMATCH_HEAD_CHARS: usize = 60;
/// Core が依頼を作った(main が Goal を書かなかった)ときの Ack の代わり
const ACK_REACTION: &str = "👀";
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// アイドル判定の周期。idle_minutes は分単位なので、この程度の遅れは問題にならない
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// `[compactor]` を読めないときの keep_recent_turns。cli.md の既定値と同じ
const FALLBACK_KEEP_RECENT_TURNS: usize = 6;

/// provider の失敗を同じチャンネルへ続けて知らせない間隔。トリガーがバッファに残るので、次の発言でも同じ失敗が起きうる
const ERROR_POST_INTERVAL: Duration = Duration::from_secs(60);

const PROGRESS_FINAL_WAIT: Duration = Duration::from_secs(10);

static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// main actor が 1 つずつ処理する仕事(§2.1)
#[derive(Debug)]
pub enum Job {
    Judge(JudgeBatch),
    /// Worker の結果は worker_runs に保存済みで、actor はそこから読む
    Join {
        run_id: String,
    },
    /// アイドル時コンパクション。範囲は処理する時点の Layer 3 から決める
    Compact,
}

impl Job {
    fn label(&self) -> &'static str {
        match self {
            Job::Judge(_) => "Judge",
            Job::Join { .. } => "Join",
            Job::Compact => "Compact",
        }
    }
}

/// 常駐処理の本体。restore → Channel::run と Ingest と actor の起動 → `shutdown` 完了で終了する。
/// 終了時はキューに残った仕事と、Fork 済みの Worker の Join を処理し終えてから止まる(最長 worker.timeout_secs + 猶予)。
/// heartbeat の起動と終了時の disconnected 更新、計測イベントの保存もここで行う
pub async fn run(
    config: Config,
    mut modules: Modules,
    store: Store,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<()> {
    let (_, closed_rx) = mpsc::unbounded_channel();
    let meter_rx = std::mem::replace(&mut modules.meter_rx, closed_rx);
    let channel = modules.channel.clone();
    let guild = channel.guild_id();

    let members = match channel.members().await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "メンバー情報を取得できませんでした。メンバー情報なしで続行します");
            Vec::new()
        }
    };
    let debounce = ingest::debounce_from_config(&config);
    let hb_info = json!({ "modules": config.modules.enabled, "guild": guild });

    let (jobs_tx, mut jobs_rx) = mpsc::unbounded_channel::<Job>();
    let mut actor = Actor::new(config, modules, store.clone(), &members, jobs_tx.clone()).await?;
    let drain_limit = SHUTDOWN_GRACE + actor.worker_timeout();
    let batches = restore::restore(&store, guild).await?;
    for batch in batches {
        let _ = jobs_tx.send(Job::Judge(batch));
    }

    let (meter_stop, meter_stop_rx) = oneshot::channel::<()>();
    let meter_task = tokio::spawn(record_meter_events(
        store.clone(),
        guild,
        meter_rx,
        meter_stop_rx,
    ));

    let (bus_handle, bus_rx) = bus::channel(256);
    let heartbeat_task = heartbeat::spawn(store.clone(), guild, hb_info.clone(), channel.clone());
    let ingest_task = tokio::spawn(ingest::run(
        store.clone(),
        channel.clone(),
        bus_rx,
        debounce,
        jobs_tx,
    ));
    let mut channel_task = tokio::spawn(async move { channel.run(bus_handle).await });
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let mut actor_task = tokio::spawn(async move {
        let mut idle_check = tokio::time::interval(IDLE_CHECK_INTERVAL);
        idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut stopping = false;
        loop {
            tokio::select! {
                job = jobs_rx.recv() => {
                    // actor 自身が送信側を持つので、キューは閉じない
                    let Some(job) = job else { break };
                    let label = job.label();
                    if let Err(e) = actor.handle(job).await {
                        tracing::error!(error = %e, job = label, "仕事の処理に失敗しました");
                    }
                }
                _ = &mut stop_rx, if !stopping => stopping = true,
                _ = idle_check.tick(), if !stopping => {
                    if let Err(e) = actor.check_idle(Timestamp::now()).await {
                        tracing::warn!(error = %e, "アイドル判定に失敗しました");
                    }
                }
            }
            if stopping && actor.in_flight() == 0 && jobs_rx.is_empty() {
                break;
            }
        }
    });

    tokio::pin!(shutdown);
    let result = tokio::select! {
        _ = &mut shutdown => Ok(()),
        r = &mut channel_task => match r {
            Ok(Ok(())) => {
                tracing::warn!("Channel が終了しました");
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(e) => Err(Error::Other(format!("channel が停止しました: {e}"))),
        },
        r = &mut actor_task => Err(Error::Other(format!("actor task が停止しました: {r:?}"))),
    };

    channel_task.abort();
    // 新しい発言は受けない。デバウンス中の発言は未判定のまま残り、次の起動時に restore が Judge にする
    ingest_task.abort();
    let _ = stop_tx.send(());
    // 完了済みの JoinHandle をもう一度 await すると panic するので、select で終わった場合は待たない
    if !actor_task.is_finished()
        && tokio::time::timeout(drain_limit, &mut actor_task)
            .await
            .is_err()
    {
        tracing::warn!("処理中の仕事や Worker が終わらないため中断します");
        actor_task.abort();
    }
    // provider や tool が送信側を持ち続けるのでキューは閉じない。actor が止まった後に届いた分まで書いてから止める
    let _ = meter_stop.send(());
    if tokio::time::timeout(SHUTDOWN_GRACE, meter_task)
        .await
        .is_err()
    {
        tracing::warn!("計測イベントの保存が終わらないため中断します");
    }
    heartbeat_task.abort();
    // abort 直前に始まった connected の upsert が disconnected を上書きしないよう、終了を待つ
    let _ = heartbeat_task.await;
    if let Err(e) = store
        .upsert_heartbeat(guild, "disconnected", &hb_info)
        .await
    {
        tracing::warn!(error = %e, "heartbeat を更新できませんでした");
    }
    result
}

/// Fork した run の実行に要るもの
struct WorkerPool {
    worker: Arc<dyn Worker>,
    /// tokio の Semaphore は待ち順に許可を出すので、max_concurrent を超えた run は FIFO で待つ
    slots: Arc<Semaphore>,
    /// キュー待ちを含む。Ack は投稿済みなので、利用者が待たされる時間の上限として扱う
    timeout: Duration,
    show_progress: bool,
}

/// Layer 0〜1 は起動時に確定した固定値。Layer 2 は Compactor だけが更新し、Layer 3 は常に Store の active_log を正とする
pub struct Actor {
    config: Config,
    guild: GuildId,
    channel: Arc<dyn Channel>,
    router: Arc<dyn Router>,
    provider: Arc<dyn Provider>,
    store: Store,
    worker: Option<WorkerPool>,
    compaction: Option<(Arc<dyn Compactor>, CompactorSettings)>,
    /// compactor が無効でも緊急時の切り詰めで使う
    keep_recent_turns: usize,
    /// worker 有効時に起動時の 1 回だけ取る。呼び出しごとに変わると main と Worker のキャッシュが外れる
    tools: Vec<ToolSpec>,
    /// Join を送るため。Worker の task にも複製して渡す
    jobs: mpsc::UnboundedSender<Job>,
    /// Fork してまだ Join を処理していない run の数。アイドル判定と終了待ちに使う
    in_flight: usize,
    /// 黙った Worker が finished になってから Join を処理するまでの間も、同じチャンネルで重ねて起動しないため。再起動では restore が全 run を中止するので持ち越さない
    silent_pending: HashSet<ChannelId>,
    /// キューに積んだ Compact が対象とするアイドル期間(その時点の人間の最終発言の行 ID)
    compact_queued: Option<i64>,
    /// コンパクションを済ませたアイドル期間。新しい人間の発言が来るまで次を積まない
    compacted_for: Option<i64>,
    harness_rules: String,
    /// Compactor に渡す規約。Worker の節を含めない(要約に Ack や Goal を書かせない)。Layer 1 も入れないので main とキャッシュは共有しない
    compactor_rules: String,
    profile: String,
    /// Layer 1.5。起動時とコンパクション後だけ読み直す。[Memo] の追記では更新しない(Layer 1.5 以降のキャッシュが全て外れるため)
    memory: String,
    /// router の state に渡す。Layer 1 と同じく起動時に取った一覧
    members: Vec<Member>,
    digest: String,
    channel_names: HashMap<ChannelId, String>,
    /// チャンネルごとに、Noth が最後に応答した時刻とそのとき話しかけてきた人。再起動で消えてよい一時的な手がかり
    conversations: HashMap<ChannelId, (Timestamp, HashSet<String>)>,
    /// チャンネルと種別ごとの、provider 失敗を最後に投稿した時刻。Join の失敗が main の失敗に隠れないよう種別を分ける
    error_posted: HashMap<(ChannelId, FailureKind), Instant>,
    typing_interval: Duration,
    progress_interval: Duration,
}

/// provider 失敗の通知を出す場面
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FailureKind {
    Reply,
    Join,
}

/// main を呼ぶときの router の判定。委譲しなかったときに食い違いを記録するために渡す
#[derive(Debug, Clone, Copy)]
enum RouterVerdict {
    ReplyOnly,
    SpawnTask {
        /// 判定の JSON に needs_outside_work の確率が無ければ None
        probability: Option<f64>,
    },
}

/// router の回答のうち、main の応答の扱いに使うもの
struct RespondHints {
    /// main の返事が空だったときに付けるリアクション
    reaction: Option<String>,
    /// 依頼が Noth の前の発言への訂正でもあるか(Core が作る依頼文に反映する)
    corrects_noth: bool,
}

/// main が Worker に任せると決めた内容
struct TaskPlan {
    /// Discord に投稿する Ack。[Memo] 行を除いてある
    ack: String,
    /// Layer 3 に残す Ack。[Memo] 行を含む
    log_ack: String,
    memos: Vec<memory::Memo>,
    goal: String,
    origin: Origin,
}

/// main の応答から [Memo] 行を分けたもの
struct Reply {
    /// Discord に投稿する本文。空なら投稿しない
    post: String,
    /// Layer 3 に残す本文。[Memo] 行は含み、[React] 行は含まない
    log: String,
    memos: Vec<memory::Memo>,
    /// `[React]` で選ばれた、許可された絵文字
    reaction: Option<String>,
}

impl Actor {
    pub async fn new(
        config: Config,
        modules: Modules,
        store: Store,
        members: &[Member],
        jobs: mpsc::UnboundedSender<Job>,
    ) -> Result<Self> {
        let guild = modules.channel.guild_id();
        let digest = store.latest_digest(guild).await?.unwrap_or_default();
        let (worker, tools) = match modules.worker {
            Some(worker) => {
                let settings = config.worker_settings()?;
                let tools = worker.tool_specs();
                let pool = WorkerPool {
                    worker,
                    slots: Arc::new(Semaphore::new(settings.max_concurrent.max(1))),
                    timeout: Duration::from_secs(settings.timeout_secs),
                    show_progress: settings.show_progress,
                };
                (Some(pool), tools)
            }
            None => (None, Vec::new()),
        };
        let compaction = match modules.compactor {
            Some(c) => Some((c, config.compactor_settings()?)),
            None => None,
        };
        let keep_recent_turns = match &compaction {
            Some((_, settings)) => settings.keep_recent_turns,
            // 無効な module のセクションは `${VAR}` 未解決のことがあり、読めなくても起動は止めない
            None => config
                .compactor_settings()
                .map_or(FALLBACK_KEEP_RECENT_TURNS, |s| s.keep_recent_turns),
        };
        Ok(Self {
            harness_rules: layers::harness_rules(
                worker.is_some(),
                &tools,
                &config.memory,
                &config.router.reactions,
            ),
            compactor_rules: layers::harness_rules(false, &[], &config.memory, &[]),
            profile: layers::profile_layer(&config, members),
            memory: memory::layer_text(
                &memory::read(&config.paths.memory_file()),
                config.memory.memory_max_chars,
            ),
            members: members.to_vec(),
            digest,
            config,
            guild,
            channel: modules.channel,
            router: modules.router,
            provider: modules.provider,
            store,
            worker,
            compaction,
            keep_recent_turns,
            tools,
            jobs,
            in_flight: 0,
            silent_pending: HashSet::new(),
            compact_queued: None,
            compacted_for: None,
            channel_names: HashMap::new(),
            conversations: HashMap::new(),
            error_posted: HashMap::new(),
            typing_interval: TYPING_INTERVAL,
            progress_interval: progress::DEFAULT_INTERVAL,
        })
    }

    pub async fn handle(&mut self, job: Job) -> Result<()> {
        match job {
            Job::Judge(batch) => self.handle_judge(batch).await,
            Job::Join { run_id } => self.handle_join(&run_id).await,
            Job::Compact => self.handle_compact().await,
        }
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// テストで typing の再送を短い間隔で観測するため
    pub fn set_typing_interval(&mut self, interval: Duration) {
        self.typing_interval = interval;
    }

    /// テストで進捗表示の間引きを短い間隔で観測するため
    pub fn set_progress_interval(&mut self, interval: Duration) {
        self.progress_interval = interval;
    }

    /// worker が無効なら 0
    pub fn worker_timeout(&self) -> Duration {
        self.worker.as_ref().map_or(Duration::ZERO, |w| w.timeout)
    }

    /// 判定 → TopicChunk 割り当て → (介入するなら)応答と昇格を 1 ユニットとして行う
    pub async fn handle_judge(&mut self, batch: JudgeBatch) -> Result<()> {
        let (Some(first), Some(last)) = (batch.messages.first(), batch.messages.last()) else {
            return Ok(());
        };
        let guild = self.guild;
        self.channel_names
            .insert(batch.channel_id, batch.channel_name.clone());

        let current = self.store.current_chunk(guild, batch.channel_id).await?;
        // バッチの途中に挟まった Bot の目印も router に見せるため、最後のトリガー行より前を取る。
        // トリガー自身は state から除くので、その分を多めに取る
        let last_row_id = batch
            .messages
            .iter()
            .map(|m| m.row_id)
            .max()
            .unwrap_or(i64::MAX);
        let recent = self
            .store
            .recent_messages(
                guild,
                last_row_id,
                self.config.router.recent_messages + batch.messages.len(),
            )
            .await?;

        // 手で直した内容がすぐ判定に効くよう、state 用には毎回ディスクから読む(router の入力はキャッシュに影響しない)
        let memory_text = memory::read(&self.config.paths.memory_file());
        // id の無い小見出しは誰のものか決められないので渡さない
        let member_memory: Vec<(UserId, String)> = memory::member_sections(&memory_text)
            .into_iter()
            .filter_map(|s| Some((s.user_id?, s.body)))
            .filter(|(_, body)| !body.is_empty())
            .collect();
        let roster = judge::Roster {
            noth: &memory::section(&memory_text, memory::HEADING_NOTH),
            members: &self.members,
            member_memory: &member_memory,
        };
        let state = judge::build_state(&batch, &recent, &roster, &self.config.router, &|m| {
            self.in_conversation_window(batch.channel_id, m)
        });
        let questions =
            judge::build_questions(&self.config.router.questions, &self.config.router.reactions);

        // 決定的に noth_request とするのはメンションと Noth の発言へのリプライだけ。
        // 名前での呼びかけと続きの窓は router への合図で、前後の文脈から router が選ぶ
        // (開発の話題で名前が出るたびに割り込まないため)
        let cued = batch
            .messages
            .iter()
            .any(|m| m.mentions_self || m.reply_to_self);
        let (answers, kind, answers_json, latency_ms) =
            match self.router.decide(&state, &questions).await {
                Ok(decision) => (
                    judge::binarize(
                        &decision,
                        &self.config.router.reactions,
                        &self.config.router.thresholds,
                    ),
                    "judge",
                    judge::answers_json(&decision),
                    decision.latency.as_millis() as i64,
                ),
                Err(e) => {
                    tracing::warn!(error = %e, "router の判定に失敗しました。静観として扱います");
                    (
                        JudgeAnswers::safe(),
                        "judge_error",
                        json!({ "error": e.to_string() }),
                        0,
                    )
                }
            };
        let decision_id = self
            .store
            .insert_decision(
                guild,
                kind,
                &self.router.model_label(),
                &answers_json,
                latency_ms,
            )
            .await?;

        let is_noise = answers.is_noise_for(cued);
        // ノイズの本文は要約にも新しいチャンクの種にもしない。チャンクが無いときだけは割り当て先が要るので作る
        let chunk_id = match current {
            Some(c) if is_noise => {
                self.store.touch_chunk(c.id, last.at).await?;
                c.id
            }
            Some(c) if !answers.is_topic_shift => {
                self.store.touch_chunk(c.id, last.at).await?;
                c.id
            }
            _ => {
                self.store
                    .create_chunk(guild, batch.channel_id, first.at)
                    .await?
            }
        };
        let row_ids: Vec<i64> = batch.messages.iter().map(|m| m.row_id).collect();
        self.store
            .mark_judged(&row_ids, decision_id, chunk_id, is_noise)
            .await?;

        let worker_enabled = self.worker.is_some();
        // 費用のかかる補足の候補にだけ DB を引く
        let supplement_allowed = if worker_enabled && answers.worth_supplement {
            self.supplement_allowed(batch.channel_id, Timestamp::now())
                .await?
        } else {
            true
        };
        let action = judge::decide_action(&ActionInput {
            answers: &answers,
            cued,
            noise: is_noise,
            worker_enabled,
            supplement_allowed,
        });
        tracing::info!(channel = %batch.channel_name, action = action.as_str(), addressee = answers.addressee.as_str(), noise = is_noise, chunk_id, "judge");
        match action {
            Action::Silent => Ok(()),
            // 発言しないので Layer 3 には何も足さず、発言群は静観と同じくバッファに残す
            Action::React => {
                let emoji = answers.reaction.as_deref().unwrap_or_default();
                if let Err(e) = self
                    .channel
                    .react(batch.channel_id, last.message_id, emoji)
                    .await
                {
                    tracing::warn!(error = %e, emoji, "リアクションを付けられませんでした");
                }
                Ok(())
            }
            Action::SilentSupplement | Action::SilentCorrection => {
                let origin = action.origin().unwrap_or(Origin::Supplement);
                // 同じ話題への補足と訂正が重なって、同じ指摘を二重に投稿しないため
                if self.silent_run_active(batch.channel_id).await? {
                    tracing::debug!(
                        channel = %batch.channel_name,
                        origin = origin.as_str(),
                        "黙って起動した Worker が動いているため、新しく起動しません"
                    );
                    return Ok(());
                }
                self.start_silent(&batch, chunk_id, origin).await
            }
            Action::ReplyOnly | Action::SpawnTask => {
                let verdict = if action == Action::SpawnTask {
                    RouterVerdict::SpawnTask {
                        probability: answers_json[judge::KEY_NEEDS_OUTSIDE_WORK]["p"].as_f64(),
                    }
                } else {
                    RouterVerdict::ReplyOnly
                };
                let hints = RespondHints {
                    reaction: answers.reaction.clone(),
                    corrects_noth: answers.corrects_noth,
                };
                self.respond(&batch, chunk_id, verdict, hints).await
            }
        }
    }

    /// そのチャンネルで、補足か訂正の Worker が動いているか
    async fn silent_run_active(&self, channel_id: ChannelId) -> Result<bool> {
        if self.silent_pending.contains(&channel_id) {
            return Ok(true);
        }
        Ok(self
            .store
            .active_runs(self.guild)
            .await?
            .iter()
            .any(|r| r.channel_id == channel_id && r.origin != Origin::Request))
    }

    /// 補足の最短間隔(チャンネルごと)と 1 日の上限(guild 全体、ローカル時刻の日付)に収まるか。
    /// 再起動しても数え直せるよう worker_runs から数える
    async fn supplement_allowed(&self, channel_id: ChannelId, now: Timestamp) -> Result<bool> {
        let router = &self.config.router;
        let day_start = now
            .to_zoned(jiff::tz::TimeZone::system())
            .start_of_day()
            .map(|z| z.timestamp())
            .unwrap_or(now);
        let (last, today) = self
            .store
            .supplement_usage(self.guild, channel_id, day_start)
            .await?;
        if today >= router.supplement_max_per_day {
            tracing::info!(today, "補足の 1 日の上限に達したため静観します");
            return Ok(false);
        }
        if let Some(last) = last
            && now.as_second() - last.as_second() < router.supplement_min_interval_secs as i64
        {
            tracing::info!(channel_id, "補足の最短間隔に満たないため静観します");
            return Ok(false);
        }
        Ok(true)
    }

    /// パターン B と C。Worker に回すかどうかは main の応答の形で決める(`[Goal]` か tool 呼び出し)。
    /// `verdict` が `SpawnTask` で main が `[Goal]` を書かなかったときは、返事を投稿せず Core が依頼を作って起動する
    async fn respond(
        &mut self,
        batch: &JudgeBatch,
        chunk_id: i64,
        verdict: RouterVerdict,
        hints: RespondHints,
    ) -> Result<()> {
        let RespondHints {
            reaction: fallback_reaction,
            corrects_noth,
        } = hints;
        let guild = self.guild;
        let trigger_ids: HashSet<i64> = batch.messages.iter().map(|m| m.row_id).collect();

        self.promote_buffer(&trigger_ids).await?;

        let trigger_lines = trigger_lines(batch);
        let trigger = layers::trigger_turn(chunk_id, &trigger_lines);

        let tail = vec![
            trigger.clone(),
            layers::channel_turn(
                chunk_id,
                &batch.channel_name,
                batch.channel_id,
                matches!(verdict, RouterVerdict::SpawnTask { .. }),
            ),
        ];

        let active_log = self.store.active_turns(guild).await?;
        let prompt = self.prompt(active_log, tail);
        let completion = match self
            .complete_with_typing(batch.channel_id, &prompt, Purpose::Main, None)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "main の生成に失敗しました。バッファに残します");
                self.report_provider_failure(batch.channel_id, FailureKind::Reply, &e)
                    .await;
                return Ok(());
            }
        };

        tracing::info!(
            tools = self.tools.len(),
            tool_calls = completion.tool_calls.len(),
            has_text = completion.text.is_some(),
            chunk_id,
            "main の応答"
        );
        if let Some(plan) = self.task_plan(&completion) {
            return self
                .start_task(batch, chunk_id, trigger, plan, &completion.usage)
                .await;
        }
        if let RouterVerdict::SpawnTask { probability } = verdict {
            let reply = completion.text.as_deref().unwrap_or_default();
            tracing::info!(
                needs_outside_work_p = ?probability,
                reply = %reply.chars().take(MISMATCH_HEAD_CHARS).collect::<String>(),
                chunk_id,
                "main が [Goal] を書かなかったため、返事を投稿せず Core が Worker を起動します"
            );
            let plan = TaskPlan {
                ack: String::new(),
                log_ack: String::new(),
                // 捨てた返事の [Memo] は、言ってもいないことを覚えさせないため記録しない
                memos: Vec::new(),
                goal: layers::core_goal(
                    &batch.channel_name,
                    batch.channel_id,
                    &trigger_lines,
                    corrects_noth,
                ),
                origin: Origin::Request,
            };
            return self
                .start_task(batch, chunk_id, trigger, plan, &completion.usage)
                .await;
        }
        let text = completion
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or_default();
        let reply = self.split_reply(text);
        let silent_reply = reply.post.is_empty();
        let emoji = reply
            .reaction
            .clone()
            .or_else(|| fallback_reaction.filter(|_| silent_reply));
        if silent_reply && reply.memos.is_empty() && emoji.is_none() {
            tracing::info!("main の応答が空でした。投稿せずバッファに残します");
            return Ok(());
        }
        if !silent_reply
            && let Err(e) = self.channel.send(batch.channel_id, &reply.post, None).await
        {
            tracing::warn!(error = %e, "応答を投稿できませんでした。バッファに残します");
            return Ok(());
        }
        let mut reacted = None;
        if let Some(emoji) = emoji
            && let Some(message_id) = batch.messages.last().map(|m| m.message_id)
        {
            match self
                .channel
                .react(batch.channel_id, message_id, &emoji)
                .await
            {
                Ok(()) => reacted = Some(emoji),
                Err(e) => {
                    tracing::warn!(error = %e, emoji, "リアクションを付けられませんでした")
                }
            }
        }
        if silent_reply && reacted.is_none() && reply.memos.is_empty() {
            tracing::info!("リアクションだけの応答を付けられなかったため、バッファに残します");
            return Ok(());
        }

        // [Memo] だけの応答は何も投稿しないが、覚えた事実を main が次のターンから知っているよう Layer 3 には残す
        if !silent_reply {
            self.note_conversation(batch);
        }
        let response = match (&reacted, silent_reply) {
            (Some(emoji), true) => layers::reaction_response_turn(chunk_id, emoji, &reply.log),
            _ => layers::response_turn(chunk_id, &reply.log),
        };
        let row_ids: Vec<i64> = batch.messages.iter().map(|m| m.row_id).collect();
        self.store
            .record_reply(guild, &trigger, &response, &row_ids, &completion.usage)
            .await?;
        self.record_memos(&reply.memos).await;
        self.enforce_hard_limit(chunk_id).await
    }

    /// `[React]` の行を外したうえで、`memo` が有効なら Memo の行も分ける。`memo` が無効なら Memo には手を付けない
    fn split_reply(&self, text: &str) -> Reply {
        let (without_react, reaction) =
            layers::extract_reaction(text, &self.config.router.reactions);
        if !self.config.memory.memo {
            return Reply {
                post: without_react.clone(),
                log: without_react,
                memos: Vec::new(),
                reaction,
            };
        }
        let (kept, memos) = memory::extract_memos(&without_react, &self.members);
        Reply {
            post: kept.trim().to_string(),
            log: without_react,
            memos: memory::normalize_memos(memos, self.config.memory.memo_max_per_reply),
            reaction,
        }
    }

    /// 追記に失敗しても会話は止めない。覚え損ねるだけで、応答は投稿済み
    async fn record_memos(&self, memos: &[memory::Memo]) {
        if memos.is_empty() {
            return;
        }
        match memory::append_memos(
            &self.store,
            self.guild,
            &self.config.paths.memory_file(),
            self.config.memory.memory_max_chars,
            &self.members,
            memos,
            Timestamp::now(),
        )
        .await
        {
            Ok(0) => tracing::info!("MEMORY.md には追記しませんでした(重複、または上限超過)"),
            Ok(n) => tracing::info!(memos = n, "MEMORY.md に追記しました"),
            Err(e) => tracing::warn!(error = %e, "MEMORY.md に追記できませんでした"),
        }
    }

    /// main の応答が Worker への委譲かどうか。router が reply_only と判定していても、main が `[Goal]` を書いたか
    /// tool を呼んだなら委譲する。そのまま投稿すると Goal が Discord に漏れ、tool_calls は投稿できる本文が無いため
    fn task_plan(&self, completion: &Completion) -> Option<TaskPlan> {
        self.worker.as_ref()?;
        if !completion.tool_calls.is_empty() {
            return Some(TaskPlan {
                ack: layers::FIXED_ACK.to_string(),
                log_ack: layers::FIXED_ACK.to_string(),
                memos: Vec::new(),
                goal: layers::goal_from_tool_calls(&completion.tool_calls),
                origin: Origin::Request,
            });
        }
        // `[Goal]` 行が無いのは、main が調べなくても答えられると判断したか書式を守らなかったとき。
        // Goal の無い Worker には任せる内容が無いので、spawn_task でも普通の返事として投稿する
        let (log_ack, goal) = layers::split_ack_goal(completion.text.as_deref()?)?;
        let reply = self.split_reply(&log_ack);
        Some(TaskPlan {
            // Ack が [Memo] だけなら、何も言わずに作業を始めないよう定型 Ack にする
            ack: if reply.post.is_empty() {
                layers::FIXED_ACK.to_string()
            } else {
                reply.post
            },
            log_ack: reply.log,
            memos: reply.memos,
            goal,
            origin: Origin::Request,
        })
    }

    /// パターン C の Ack と昇格と Fork。Ack を投稿できなければ Worker も起動せず、トリガーはバッファに残す
    async fn start_task(
        &mut self,
        batch: &JudgeBatch,
        chunk_id: i64,
        trigger: Turn,
        plan: TaskPlan,
        usage: &Usage,
    ) -> Result<()> {
        let guild = self.guild;
        // DB の失敗で Ack 投稿後に中断し、Queued のまま残る run を作らないよう、Ack より前に済ませる
        let row_ids: Vec<i64> = batch.messages.iter().map(|m| m.row_id).collect();
        let audience = self
            .audience(&row_ids, batch.channel_id, &batch.channel_name)
            .await?;
        // Ack が空なのは Core が依頼を作ったとき。何も言わずに作業を始めると無視されたように見えるので、👀 だけ付ける
        let progress_target = if plan.ack.is_empty() {
            if let Some(last) = batch.messages.last()
                && let Err(e) = self
                    .channel
                    .react(batch.channel_id, last.message_id, ACK_REACTION)
                    .await
            {
                tracing::warn!(error = %e, "👀 を付けられませんでした。Worker は起動します");
            }
            None
        } else {
            match self.channel.send(batch.channel_id, &plan.ack, None).await {
                Ok(id) => Some((id, plan.ack.clone())),
                Err(e) => {
                    tracing::warn!(error = %e, "Ack を投稿できませんでした。Worker を起動せずバッファに残します");
                    return Ok(());
                }
            }
        };
        self.note_conversation(batch);

        let trigger_message_id = match row_ids.iter().max() {
            Some(id) => self.store.message(*id).await?.map(|m| m.message_id),
            None => None,
        };
        let run = NewWorkerRun {
            run_id: next_run_id(),
            goal: plan.goal.clone(),
            origin: plan.origin,
            channel_id: batch.channel_id,
            chunk_id,
            trigger_message_id,
            fork_message_id: self.store.max_message_id(guild).await?,
        };
        // 進捗の追記は Discord 上の見た目だけで、Layer 3 には Ack + Goal だけを残す。
        // 進捗は tool 実行の生ログに当たるので main に入れず(設計原則 2)、編集のたびに Layer 3 を書き換えてキャッシュを外さないため
        self.store
            .record_task(
                guild,
                &trigger,
                &layers::task_response_turn(chunk_id, &plan.log_ack, &plan.goal),
                &row_ids,
                usage,
                &run,
            )
            .await?;
        self.record_memos(&plan.memos).await;
        tracing::info!(run_id = %run.run_id, origin = run.origin.as_str(), "Worker を起動します");
        // Worker の base も main と同じ上限に収めるため、Fork より前に緊急コンパクションを済ませる
        self.enforce_hard_limit(chunk_id).await?;

        let base = self.prompt(self.store.active_turns(guild).await?, Vec::new());
        self.fork(run, base, progress_target, audience);
        Ok(())
    }

    /// パターン C'。Ack もリアクションも出さずに Worker を起動する。トリガーはバッファに残り、結果が出たときに一緒に Layer 3 へ入る
    async fn start_silent(
        &mut self,
        batch: &JudgeBatch,
        chunk_id: i64,
        origin: Origin,
    ) -> Result<()> {
        let guild = self.guild;
        let row_ids: Vec<i64> = batch.messages.iter().map(|m| m.row_id).collect();
        let audience = self
            .audience(&row_ids, batch.channel_id, &batch.channel_name)
            .await?;
        let run = NewWorkerRun {
            run_id: next_run_id(),
            goal: layers::silent_goal(origin, &trigger_lines(batch)),
            origin,
            channel_id: batch.channel_id,
            chunk_id,
            trigger_message_id: batch.messages.last().map(|m| m.message_id),
            fork_message_id: self.store.max_message_id(guild).await?,
        };
        self.store.insert_worker_run(guild, &run).await?;
        self.silent_pending.insert(batch.channel_id);
        tracing::info!(run_id = %run.run_id, origin = origin.as_str(), "黙って Worker を起動します");
        let base = self.prompt(self.store.active_turns(guild).await?, Vec::new());
        self.fork(run, base, None, audience);
        Ok(())
    }

    /// トリガー発言群の発言者。tool が閲覧権限を確かめる相手になる
    async fn audience(
        &self,
        row_ids: &[i64],
        channel_id: ChannelId,
        channel_name: &str,
    ) -> Result<Audience> {
        let mut requesters = Vec::new();
        for id in row_ids {
            let Some(m) = self.store.message(*id).await? else {
                // 発言者を特定できない行があれば、一部の人だけで権限を判断しないよう依頼者を空にして閉じる
                tracing::warn!(
                    row_id = id,
                    "トリガー発言が見つからないため、依頼者を特定できません"
                );
                return Ok(Audience {
                    requesters: Vec::new(),
                    channel_id: Some(channel_id),
                    channel_name: Some(channel_name.to_string()),
                });
            };
            if !m.is_self && !requesters.contains(&m.user_id) {
                requesters.push(m.user_id);
            }
        }
        Ok(Audience {
            requesters,
            channel_id: Some(channel_id),
            channel_name: Some(channel_name.to_string()),
        })
    }

    /// main の Layer 0〜3 と tool 定義を複製して Worker をバックグラウンドで動かし、終わったら Join を積む
    fn fork(
        &mut self,
        run: NewWorkerRun,
        base: LayeredPrompt,
        progress_target: Option<(MessageId, String)>,
        audience: Audience,
    ) {
        let Some(pool) = &self.worker else {
            return;
        };
        self.in_flight += 1;
        let worker = pool.worker.clone();
        let slots = pool.slots.clone();
        let timeout = pool.timeout;
        let store = self.store.clone();
        let jobs = self.jobs.clone();
        // Ack の後に黙り込むと止まったように見えるので、キュー待ちの間も含めて Worker が終わるまで入力中を出し続ける。
        // 黙って起動した Worker は、人どうしの会話に気配を出さないため出さない
        let typing = (run.origin == Origin::Request)
            .then(|| AbortOnDrop(self.spawn_typing(run.channel_id).abort_handle()));
        // 2,000 文字を超えた Ack は分割投稿されていて、最後の断片に Ack 全体を書き戻せないので進捗を出さない
        let progress_target = progress_target
            .filter(|(_, ack)| pool.show_progress && ack.chars().count() <= progress::MAX_CHARS);
        let (progress_tx, progress) = if let Some((ack_message_id, ack)) = progress_target {
            let (tx, rx) = mpsc::unbounded_channel();
            let (done_tx, done_rx) = oneshot::channel();
            let handle = progress::spawn(
                self.channel.clone(),
                run.channel_id,
                ack_message_id,
                ack,
                self.progress_interval,
                rx,
                done_rx,
            );
            (
                Some(tx),
                Some((done_tx, AbortOnDrop(handle.abort_handle()), handle)),
            )
        } else {
            (None, None)
        };
        let task = WorkerTask {
            work_dir: self.config.paths.work_dir(&run.run_id),
            run_id: run.run_id,
            goal: run.goal,
            origin: run.origin,
            base,
            progress: progress_tx,
            history: Some(std::sync::Arc::new(store.clone())),
            audience,
        };
        tokio::spawn(async move {
            let run_id = task.run_id.clone();
            // キュー待ちも含めて timeout_secs で打ち切る
            let (status, text) =
                match tokio::time::timeout(timeout, execute(worker, slots, &store, task)).await {
                    Ok(WorkerOutcome::Concluded(memo)) => (RunStatus::Finished, memo),
                    Ok(WorkerOutcome::Failed(reason)) => (RunStatus::Failed, reason),
                    Err(_) => (
                        RunStatus::Failed,
                        format!("{} 秒以内に終わりませんでした", timeout.as_secs()),
                    ),
                };
            drop(typing);
            if let Some((done, _guard, handle)) = progress {
                let _ = done.send(status == RunStatus::Finished);
                // 結びの編集を報告より先に済ませる。Discord が詰まっていても Join を遅らせすぎないよう待ち時間を区切る
                let _ = tokio::time::timeout(PROGRESS_FINAL_WAIT, handle).await;
            }
            tracing::info!(run_id = %run_id, status = status.as_str(), "Worker が終了しました");
            if let Err(e) = store.finish_run(&run_id, status, &text).await {
                tracing::error!(error = %e, run_id = %run_id, "Worker の結果を SQLite に保存できなかったため、この調査の結果は Discord に報告されません");
            }
            let _ = jobs.send(Job::Join { run_id });
        });
    }

    /// パターン C の Join と報告(§4.3)
    async fn handle_join(&mut self, run_id: &str) -> Result<()> {
        self.in_flight = self.in_flight.saturating_sub(1);
        let guild = self.guild;
        let Some(run) = self.store.worker_run(run_id).await? else {
            tracing::warn!(run_id, "Join の run が見つかりません");
            return Ok(());
        };
        if run.origin != Origin::Request {
            self.silent_pending.remove(&run.channel_id);
        }
        let memo = run.conclusion.clone().unwrap_or_default();
        let outcome = match run.status {
            RunStatus::Finished => Ok(memo.as_str()),
            RunStatus::Failed => Err(memo.as_str()),
            other => {
                tracing::warn!(
                    run_id,
                    status = other.as_str(),
                    "結果が保存されていないため報告しません"
                );
                return Ok(());
            }
        };
        let Some(chunk_id) = run.chunk_id else {
            tracing::warn!(run_id, "TopicChunk の無い run は報告しません");
            return Ok(());
        };
        if run.origin != Origin::Request {
            return self.handle_silent_join(&run, chunk_id, outcome).await;
        }

        let since_fork: Vec<MessageRow> = self
            .store
            .recent_messages(guild, i64::MAX, self.config.router.recent_messages)
            .await?
            .into_iter()
            .filter(|r| r.id > run.fork_message_id)
            .collect();
        // 後続の人間の発言が無ければ話題は移っていないので、router の reply_anchor や abort を採らない
        let style = if !self.has_human_follow_up(&run).await? {
            tracing::info!(
                run_id,
                "トリガー後に人間の発言が無いため router を呼びません"
            );
            DeliveryStyle::NormalReport
        } else {
            let channel_name = self
                .channel_names
                .get(&run.channel_id)
                .cloned()
                .unwrap_or_else(|| run.channel_id.to_string());
            let state = judge::build_join_state(
                &judge::JoinInput {
                    run_id,
                    goal: &run.goal,
                    channel: &channel_name,
                    concluded: outcome.is_ok(),
                    memo: &memo,
                    since_fork: &since_fork,
                },
                &self.config.router,
            );
            let (style, kind, answers, latency_ms) = match self
                .router
                .decide(&state, &judge::join_questions(Origin::Request))
                .await
            {
                Ok(decision) => (
                    judge::binarize_delivery(
                        &decision,
                        self.config.router.thresholds.min_confidence,
                    ),
                    "join",
                    judge::answers_json(&decision),
                    decision.latency.as_millis() as i64,
                ),
                Err(e) => {
                    tracing::warn!(error = %e, run_id, "router の合流判定に失敗しました。報告しません");
                    (
                        DeliveryStyle::Abort,
                        "join_error",
                        json!({ "error": e.to_string() }),
                        0,
                    )
                }
            };
            self.store
                .insert_decision(
                    guild,
                    kind,
                    &self.router.model_label(),
                    &answers,
                    latency_ms,
                )
                .await?;
            style
        };
        self.store
            .set_delivery_style(run_id, style.as_str())
            .await?;
        tracing::info!(run_id, delivery_style = style.as_str(), "join");
        if style == DeliveryStyle::Abort {
            return Ok(());
        }

        self.promote_buffer(&HashSet::new()).await?;
        let conclusion = layers::conclusion_turn(chunk_id, run_id, outcome);
        let mut active_log = self.store.active_turns(guild).await?;
        active_log.push(conclusion.clone());
        let prompt = self.prompt(
            active_log,
            vec![layers::delivery_instruction_turn(chunk_id, style)],
        );
        let completion = self
            .complete_with_typing(run.channel_id, &prompt, Purpose::Join, Some(run_id))
            .await;
        let reply_to = run
            .trigger_message_id
            .filter(|_| style == DeliveryStyle::ReplyAnchor);
        let posted = match &completion {
            Ok(c) => match c.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                Some(text) => {
                    let reply = self.split_reply(text);
                    if reply.post.is_empty() {
                        // 投稿していない応答は Layer 3 に残さない。覚えた事実だけは書く
                        self.record_memos(&reply.memos).await;
                        tracing::info!(run_id, "報告の応答が空でした");
                        None
                    } else {
                        match self
                            .channel
                            .send(run.channel_id, &reply.post, reply_to)
                            .await
                        {
                            Ok(_) => {
                                self.record_memos(&reply.memos).await;
                                Some(text.to_string())
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, run_id, "報告を投稿できませんでした");
                                // Join は再試行しないので、覚えた事実だけは残す
                                self.record_memos(&reply.memos).await;
                                None
                            }
                        }
                    }
                }
                None => {
                    tracing::info!(run_id, "報告の応答が空でした");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, run_id, "報告の生成に失敗しました");
                self.report_provider_failure(run.channel_id, FailureKind::Join, e)
                    .await;
                None
            }
        };
        match (posted, completion) {
            (Some(text), Ok(c)) => {
                self.store
                    .record_reply(
                        guild,
                        &conclusion,
                        &layers::response_turn(chunk_id, &text),
                        &[],
                        &c.usage,
                    )
                    .await?;
            }
            // 報告できなくても結論メモだけは Layer 3 に残す。Ack で待たせているので、次に main が話すときに
            // 結果を踏まえられるようにするため。投稿していない応答は残さない(言っていないことを言ったと思わせない)
            // usage の無い追記なので、上限の判定は次に usage を書いたときに回す
            _ => {
                self.store.append_turn(guild, &conclusion).await?;
                return Ok(());
            }
        }
        self.enforce_hard_limit(chunk_id).await
    }

    /// 補足・訂正の Join。依頼ではないので Ack で待たせた人はいない。
    /// 結論は投稿の要否にかかわらず Layer 3 に入れる(調べた結果を捨てないため)。
    /// 「要」のときだけ、合流判定で人どうしの解決を確かめ、続けるなら main が短い補足を書き、トリガーへのリプライで投稿する。
    /// Worker の文面は投稿しない(外部の入力を読んだ Worker の出力をそのまま人に見せないため)
    async fn handle_silent_join(
        &mut self,
        run: &WorkerRunRow,
        chunk_id: i64,
        outcome: std::result::Result<&str, &str>,
    ) -> Result<()> {
        let guild = self.guild;
        let run_id = run.run_id.as_str();
        let memo = match outcome {
            Ok(memo) => memo,
            Err(reason) => {
                // 黙って起動したので、利用者は結果を待っていない。失敗を Layer 3 にも投稿にも出さない
                tracing::warn!(
                    run_id,
                    origin = run.origin.as_str(),
                    reason,
                    "黙って起動した Worker が失敗しました"
                );
                return Ok(());
            }
        };
        let conclusion = layers::conclusion_turn(chunk_id, run_id, Ok(memo));
        if !supplement::needed(memo) {
            tracing::info!(run_id, origin = run.origin.as_str(), "補足は不要でした");
            return self.keep_conclusion_only(&conclusion, chunk_id).await;
        }
        if !self.silent_report_allowed(run, memo).await? {
            return self.keep_conclusion_only(&conclusion, chunk_id).await;
        }

        self.promote_buffer(&HashSet::new()).await?;
        let mut active_log = self.store.active_turns(guild).await?;
        active_log.push(conclusion.clone());
        let prompt = self.prompt(
            active_log,
            vec![layers::silent_report_turn(chunk_id, run.origin)],
        );
        // 黙って起動したので入力中の表示は出さず、失敗も利用者に知らせない
        let params = CompletionParams {
            max_tokens: self.config.context.max_tokens,
            purpose: Purpose::Join,
            run_id: Some(run_id.to_string()),
        };
        let completion = match self.provider.complete(&prompt, &params).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, run_id, "補足の生成に失敗しました。投稿しません");
                return self.keep_conclusion_only(&conclusion, chunk_id).await;
            }
        };
        let reply = self.split_reply(completion.text.as_deref().unwrap_or_default().trim());
        self.record_memos(&reply.memos).await;
        if reply.post.is_empty() {
            tracing::info!(run_id, "main の補足が空だったため投稿しません");
            return self.keep_conclusion_only(&conclusion, chunk_id).await;
        }
        // 補足の返事から Worker は起動できない。投稿すると [Goal] の行がそのまま人に見える
        if reply
            .post
            .lines()
            .any(|l| l.trim_start().starts_with(layers::GOAL_MARKER))
        {
            tracing::warn!(
                run_id,
                "main の補足に [Goal] が含まれていたため投稿しません"
            );
            return self.keep_conclusion_only(&conclusion, chunk_id).await;
        }
        let Some(reply_to) = run.trigger_message_id else {
            tracing::warn!(
                run_id,
                "リプライ先のトリガー発言が無いため補足を投稿しません"
            );
            return self.keep_conclusion_only(&conclusion, chunk_id).await;
        };
        if let Err(e) = self
            .channel
            .send(run.channel_id, &reply.post, Some(reply_to))
            .await
        {
            tracing::warn!(error = %e, run_id, "補足を投稿できませんでした");
            return self.keep_conclusion_only(&conclusion, chunk_id).await;
        }
        tracing::info!(run_id, origin = run.origin.as_str(), "補足を投稿しました");
        // 言ったことは Layer 3 に残す。次に main が話すとき、同じ指摘を繰り返さないため
        self.store
            .record_reply(
                guild,
                &conclusion,
                &layers::response_turn(chunk_id, &reply.log),
                &[],
                &completion.usage,
            )
            .await?;
        self.enforce_hard_limit(chunk_id).await
    }

    /// 投稿しない補足・訂正でも、調べた結果は次に main が話すときのために Layer 3 に残す
    async fn keep_conclusion_only(&mut self, conclusion: &Turn, chunk_id: i64) -> Result<()> {
        self.promote_buffer(&HashSet::new()).await?;
        self.store.append_turn(self.guild, conclusion).await?;
        self.enforce_hard_limit(chunk_id).await
    }

    /// トリガーの後に、同じチャンネルの人間の発言か、依頼者本人の発言(別チャンネルでも)があるか。
    /// 依頼者が別の場所で「いらなかった」と言った場合も router に尋ねて abort できるようにするため
    async fn has_human_follow_up(&self, run: &WorkerRunRow) -> Result<bool> {
        let requester = match run.trigger_message_id {
            Some(id) => self.store.author_of_message(self.guild, id).await?,
            None => None,
        };
        self.store
            .has_human_message_after(self.guild, run.fork_message_id, run.channel_id, requester)
            .await
    }

    /// 調べている間に人どうしで解決していないかを、合流判定と同じ仕組みで router に尋ねる。
    /// 判定に失敗したら投稿しない側に倒す
    async fn silent_report_allowed(&self, run: &WorkerRunRow, memo: &str) -> Result<bool> {
        let guild = self.guild;
        let run_id = run.run_id.as_str();
        let since_fork: Vec<MessageRow> = self
            .store
            .recent_messages(guild, i64::MAX, self.config.router.recent_messages)
            .await?
            .into_iter()
            .filter(|r| r.id > run.fork_message_id)
            .collect();
        // 人どうしで解決したかを尋ねる問いなので、人間の発言が無ければ答えは決まっている
        if !self.has_human_follow_up(run).await? {
            tracing::info!(
                run_id,
                "トリガー後に人間の発言が無いため router を呼びません"
            );
            self.store
                .set_delivery_style(run_id, DeliveryStyle::NormalReport.as_str())
                .await?;
            return Ok(true);
        }
        let channel_name = self
            .channel_names
            .get(&run.channel_id)
            .cloned()
            .unwrap_or_else(|| run.channel_id.to_string());
        let state = judge::build_join_state(
            &judge::JoinInput {
                run_id,
                goal: &run.goal,
                channel: &channel_name,
                concluded: true,
                memo,
                since_fork: &since_fork,
            },
            &self.config.router,
        );
        let (style, kind, answers, latency_ms) = match self
            .router
            .decide(&state, &judge::join_questions(run.origin))
            .await
        {
            Ok(decision) => (
                judge::binarize_delivery(&decision, self.config.router.thresholds.min_confidence),
                "join",
                judge::answers_json(&decision),
                decision.latency.as_millis() as i64,
            ),
            Err(e) => {
                tracing::warn!(error = %e, run_id, "router の合流判定に失敗しました。投稿しません");
                (
                    DeliveryStyle::Abort,
                    "join_error",
                    json!({ "error": e.to_string() }),
                    0,
                )
            }
        };
        self.store
            .insert_decision(
                guild,
                kind,
                &self.router.model_label(),
                &answers,
                latency_ms,
            )
            .await?;
        self.store
            .set_delivery_style(run_id, style.as_str())
            .await?;
        tracing::info!(run_id, delivery_style = style.as_str(), "silent join");
        Ok(style != DeliveryStyle::Abort)
    }

    /// 人間の発言が idle_minutes 途切れ、Worker も動いていなければ Compact を 1 回だけ積む。
    /// 同じアイドル期間には積み直さず、人間の新しい発言が来たら次の期間として扱う
    pub async fn check_idle(&mut self, now: Timestamp) -> Result<()> {
        let Some((_, settings)) = &self.compaction else {
            return Ok(());
        };
        let idle_ms = settings.idle_minutes as i64 * 60_000;
        if self.in_flight > 0 || self.compact_queued.is_some() {
            return Ok(());
        }
        let Some((last_id, last_at)) = self.store.last_human_message(self.guild).await? else {
            return Ok(());
        };
        if self.compacted_for == Some(last_id)
            || now.as_millisecond() - last_at.as_millisecond() < idle_ms
        {
            return Ok(());
        }
        self.compact_queued = Some(last_id);
        let _ = self.jobs.send(Job::Compact);
        Ok(())
    }

    /// パターン D の通常トリガー。直近 keep_recent_turns を残して Layer 3 を Layer 2 のエピソードに置き換える
    async fn handle_compact(&mut self) -> Result<()> {
        let period = self.compact_queued.take();
        let Some((compactor, settings)) = self.compaction.clone() else {
            return Ok(());
        };
        // 積んだ後に Fork された Worker があれば完了を待つ(§5 D)。次の周期の check_idle が積み直す
        if self.in_flight > 0 {
            return Ok(());
        }
        // 積んでから処理するまでに会話が再開していたら、進行中の会話を要約で消さないよう見送る。
        // compacted_for は更新しないので、次にアイドルになったときに改めて積まれる
        let last = self
            .store
            .last_human_message(self.guild)
            .await?
            .map(|(id, _)| id);
        if last != period {
            return Ok(());
        }
        // 失敗しても同じアイドル期間には再試行しない。要約 API の障害中に周期ごとに呼び続けないため
        self.compacted_for = period;

        let turns = self.store.active_turns(self.guild).await?;
        let cut = compaction_cut(&turns, settings.keep_recent_turns);
        if cut == 0 {
            return Ok(());
        }
        let (range, kept) = turns.split_at(cut);
        let episode = match compactor.digest(&self.compactor_rules, range).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "Layer 3 の要約に失敗しました。ターンはそのまま残します");
                return Ok(());
            }
        };
        let kept_chunks: HashSet<i64> = kept.iter().filter_map(|t| t.chunk_id).collect();
        let mut finished: Vec<i64> = range
            .iter()
            .filter_map(|t| t.chunk_id)
            .filter(|c| !kept_chunks.contains(c))
            .collect();
        finished.sort_unstable();
        finished.dedup();
        let ids: Vec<i64> = range.iter().filter_map(|t| t.id).collect();
        self.merge_episode(&episode, settings.digest_max_chars, &ids, &finished)
            .await?;
        tracing::info!(
            turns = ids.len(),
            "アイドル時コンパクションで Layer 3 を要約しました"
        );
        Ok(())
    }

    async fn merge_episode(
        &mut self,
        episode: &str,
        max_chars: usize,
        turn_ids: &[i64],
        compacted_chunks: &[i64],
    ) -> Result<()> {
        let block = layers::episode_block(Timestamp::now(), episode);
        let digest = layers::merge_digest(&self.digest, &block, max_chars);
        self.store
            .replace_with_digest(self.guild, &digest, turn_ids, compacted_chunks)
            .await?;
        self.digest = digest;
        // キャッシュはコンパクションで切れるので、この機会に Layer 1.5 も読み直す
        self.memory = memory::layer_text(
            &memory::read(&self.config.paths.memory_file()),
            self.config.memory.memory_max_chars,
        );
        Ok(())
    }

    fn prompt(&self, active_log: Vec<Turn>, tail: Vec<Turn>) -> LayeredPrompt {
        layers::assemble(
            &self.harness_rules,
            &self.profile,
            &self.memory,
            &self.digest,
            active_log,
            tail,
            &self.tools,
        )
    }

    async fn complete_with_typing(
        &self,
        channel_id: ChannelId,
        prompt: &LayeredPrompt,
        purpose: Purpose,
        run_id: Option<&str>,
    ) -> Result<Completion> {
        let params = CompletionParams {
            max_tokens: self.config.context.max_tokens,
            purpose,
            run_id: run_id.map(str::to_string),
        };
        let typing = self.spawn_typing(channel_id);
        let completion = self.provider.complete(prompt, &params).await;
        typing.abort();
        completion
    }

    /// 利用者に失敗を知らせる。Layer 3 には残さない(言っていないことや失敗の記録を main に覚えさせない)。
    /// 投稿の失敗は握りつぶす。知らせる手段がもう無い
    async fn report_provider_failure(
        &mut self,
        channel_id: ChannelId,
        kind: FailureKind,
        error: &Error,
    ) {
        let now = Instant::now();
        self.error_posted
            .retain(|_, at| now.duration_since(*at) < ERROR_POST_INTERVAL);
        if self.error_posted.contains_key(&(channel_id, kind)) {
            return;
        }
        self.error_posted.insert((channel_id, kind), now);
        let what = match kind {
            FailureKind::Reply => "応答を生成できませんでした",
            FailureKind::Join => "調査結果の報告を生成できませんでした",
        };
        let label = self.provider.label();
        let (cause, retry) = match error.kind() {
            ErrorKind::Unavailable => (
                format!("{label} が応答しませんでした"),
                "。少し待ってからもう一度話しかけてください",
            ),
            ErrorKind::Auth => (
                format!("{label} の認証に失敗しました。管理者に確認してもらってください"),
                "",
            ),
            ErrorKind::Quota => (format!("{label} の利用上限に達しました"), ""),
            ErrorKind::Other => (format!("{label} でエラーが発生しました"), ""),
        };
        let text = format!("⚠️ {what}({cause}){retry}");
        if let Err(e) = self.channel.send(channel_id, &text, None).await {
            tracing::warn!(error = %e, "失敗の通知を投稿できませんでした");
        }
    }

    fn note_conversation(&mut self, batch: &JudgeBatch) {
        self.conversations.insert(
            batch.channel_id,
            (
                Timestamp::now(),
                batch
                    .messages
                    .iter()
                    .map(|m| m.author_name.clone())
                    .collect(),
            ),
        );
    }

    /// Noth が応答してから窓の間の、話していた相手の発言。決定的な noth_request ではなく router への合図にする
    fn in_conversation_window(&self, channel_id: ChannelId, m: &PendingMessage) -> bool {
        let Some((replied_at, authors)) = self.conversations.get(&channel_id) else {
            return false;
        };
        let window = self.config.router.conversation_window_secs as i64;
        // 応答より前の発言は続きではない
        let elapsed = m.at.as_second() - replied_at.as_second();
        authors.contains(&m.author_name) && (0..=window).contains(&elapsed)
    }

    /// 未昇格バッファを TopicChunk ごとに 1 ブロックにして Layer 3 の末尾へ追記する
    async fn promote_buffer(&self, exclude: &HashSet<i64>) -> Result<()> {
        let rows: Vec<MessageRow> = self
            .store
            .buffered_messages(self.guild)
            .await?
            .into_iter()
            .filter(|r| !exclude.contains(&r.id))
            .collect();

        let mut groups: Vec<(i64, Vec<MessageRow>)> = Vec::new();
        for row in rows {
            let Some(chunk_id) = row.chunk_id else {
                continue;
            };
            match groups.iter_mut().find(|(c, _)| *c == chunk_id) {
                Some((_, g)) => g.push(row),
                None => groups.push((chunk_id, vec![row])),
            }
        }
        for (chunk_id, rows) in groups {
            let turn = layers::buffer_turn(chunk_id, &rows_to_lines(&rows));
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            self.store.promote_block(self.guild, &turn, &ids).await?;
        }
        Ok(())
    }

    /// パターン D の緊急トリガー。1 回の超過につき 1 チャンクだけ、compactor があれば要約して Layer 2 へ退避し、
    /// 無ければ破棄する。進行中の `current_chunk_id` は対象にせず、残りのうち最後の発言が最も古いものを選ぶ。
    /// 他のチャンクが無ければ、進行中のチャンクの直近 keep_recent_turns より前を対象にする。
    /// 1 つの話題が長く続くと、チャンク単位では毎ターン上限を超えたままになるため
    async fn enforce_hard_limit(&mut self, current_chunk_id: i64) -> Result<()> {
        let over = self
            .store
            .last_usage(self.guild)
            .await?
            .is_some_and(|u| u.input_tokens > self.config.context.prompt_hard_limit_tokens);
        if !over {
            return Ok(());
        }
        let turns = self.store.active_turns(self.guild).await?;
        let mut present: Vec<i64> = turns.iter().filter_map(|t| t.chunk_id).collect();
        present.sort_unstable();
        present.dedup();
        let mut oldest: Option<(i64, Timestamp)> = None;
        for id in present.into_iter().filter(|id| *id != current_chunk_id) {
            let Some(chunk) = self.store.chunk(id).await? else {
                continue;
            };
            if oldest.is_none_or(|(_, at)| chunk.ended_at < at) {
                oldest = Some((id, chunk.ended_at));
            }
        }
        let (target, compacted): (Vec<Turn>, Vec<i64>) = match oldest {
            Some((oldest, _)) => (
                turns
                    .into_iter()
                    .filter(|t| t.chunk_id == Some(oldest))
                    .collect(),
                vec![oldest],
            ),
            None => {
                let cut = compaction_cut(&turns, self.keep_recent_turns);
                if cut == 0 {
                    return Ok(());
                }
                let mut turns = turns;
                turns.truncate(cut);
                (turns, Vec::new())
            }
        };
        let ids: Vec<i64> = target.iter().filter_map(|t| t.id).collect();

        if let Some((compactor, settings)) = self.compaction.clone() {
            match compactor.digest(&self.compactor_rules, &target).await {
                Ok(episode) => {
                    self.merge_episode(&episode, settings.digest_max_chars, &ids, &compacted)
                        .await?;
                    tracing::warn!(
                        chunk_ids = ?compacted,
                        turns = ids.len(),
                        "Layer 3 がハード上限を超えたため古いターンを要約して Layer 2 へ退避しました"
                    );
                    return Ok(());
                }
                // 上限を超えたままだと以降の生成が失敗し続けるので、要約できなくても破棄して上限超過の回避を優先する
                Err(e) => {
                    tracing::warn!(error = %e, "緊急コンパクションの要約に失敗しました");
                }
            }
        }
        tracing::warn!(
            chunk_ids = ?compacted,
            turns = ids.len(),
            "Layer 3 がハード上限を超えたため古いターンを破棄します"
        );
        self.store.remove_turns(&ids).await
    }

    fn spawn_typing(&self, channel_id: ChannelId) -> tokio::task::JoinHandle<()> {
        let channel = self.channel.clone();
        let interval = self.typing_interval;
        tokio::spawn(async move {
            loop {
                if let Err(e) = channel.typing(channel_id).await {
                    tracing::debug!(error = %e, "typing を送れませんでした");
                }
                tokio::time::sleep(interval).await;
            }
        })
    }
}

/// 計測の保存に失敗しても会話や調査は止めない。`stop` を受けたら、それまでに届いた分を書いて終わる
async fn record_meter_events(
    store: Store,
    guild: GuildId,
    mut rx: mpsc::UnboundedReceiver<MeterEvent>,
    mut stop: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Some(event) => record_meter_event(&store, guild, &event).await,
                None => return,
            },
            _ = &mut stop => {
                while let Ok(event) = rx.try_recv() {
                    record_meter_event(&store, guild, &event).await;
                }
                return;
            }
        }
    }
}

async fn record_meter_event(store: &Store, guild: GuildId, event: &MeterEvent) {
    let result = match event {
        MeterEvent::Cost(r) => store.record_cost(guild, r).await,
        MeterEvent::RateLimit(r) => store.upsert_rate_limit(guild, r).await,
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, "費用の記録を SQLite に保存できませんでした。`noth cost` の集計から漏れます");
    }
}

/// semaphore の空きを待ってから running にし、Worker を動かす。timeout で落とされたときは待ちごと捨てられる
async fn execute(
    worker: Arc<dyn Worker>,
    slots: Arc<Semaphore>,
    store: &Store,
    task: WorkerTask,
) -> WorkerOutcome {
    let Ok(_permit) = slots.acquire_owned().await else {
        return WorkerOutcome::Failed("Worker の実行枠を確保できませんでした".into());
    };
    match store.start_run(&task.run_id).await {
        Ok(true) => {}
        Ok(false) => {
            return WorkerOutcome::Failed("run が実行待ちの状態ではありませんでした".into());
        }
        Err(e) => {
            tracing::warn!(error = %e, run_id = %task.run_id, "run を running にできませんでした。そのまま実行します");
        }
    }
    if let Err(e) = create_work_dir(&task.work_dir) {
        return WorkerOutcome::Failed(format!("作業ディレクトリを作れませんでした: {e}"));
    }
    // Worker の panic で Join が積まれず in_flight が減らなくなるのを防ぐため、別 task で動かして JoinError を拾う
    let mut handle = tokio::spawn(async move { worker.run(task).await });
    // timeout でこの future ごと捨てられたときに Worker が裏で動き続けないよう、drop で止める
    let _abort = AbortOnDrop(handle.abort_handle());
    match (&mut handle).await {
        Ok(outcome) => outcome,
        Err(e) => WorkerOutcome::Failed(format!("Worker が異常終了しました: {e}")),
    }
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// tool-shell の cwd になる。他のユーザーに作業内容を読ませないよう 0700 にする
fn create_work_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// 新しい crate を足さずに一意にするため、起動以来の連番を時刻に添える。作業ディレクトリ名にも使う
fn next_run_id() -> String {
    format!(
        "{}-{}",
        Timestamp::now().as_millisecond(),
        RUN_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// 直近 `keep` ターンを残す境界。残す側が assistant から始まると Layer 3 の先頭が応答になり provider によっては弾かれるので、
/// 境界を前へずらして直前の user ターンも残す。設定より少なく残すことはない
fn compaction_cut(turns: &[Turn], keep: usize) -> usize {
    let mut cut = turns.len().saturating_sub(keep);
    while cut > 0 && cut < turns.len() && turns[cut].role == Role::Assistant {
        cut -= 1;
    }
    cut
}

fn trigger_lines(batch: &JudgeBatch) -> Vec<Line> {
    batch
        .messages
        .iter()
        .map(|m| Line {
            channel: batch.channel_name.clone(),
            author: m.author_name.clone(),
            at: m.at,
            content: m.content.clone(),
            reply_to: m.reply_to.clone(),
            is_bot_marker: false,
        })
        .collect()
}

fn rows_to_lines(rows: &[MessageRow]) -> Vec<Line> {
    markers::fold(rows, markers::MAX_MARKER_RUNS)
        .into_iter()
        .map(|item| match item {
            Folded::Row(r) => Line {
                channel: r.channel_name.clone(),
                author: r.author_name.clone(),
                at: r.created_at,
                content: r.content.clone(),
                reply_to: r.reply_to.clone(),
                is_bot_marker: false,
            },
            Folded::Markers(run) => Line {
                channel: run.channel_name.clone(),
                author: run.author_name.clone(),
                at: run.at,
                content: run.render(),
                reply_to: None,
                is_bot_marker: true,
            },
            Folded::Omitted {
                count,
                channel_name,
                at,
            } => Line {
                channel: channel_name,
                author: String::new(),
                at,
                content: Folded::render_omitted(count),
                reply_to: None,
                is_bot_marker: true,
            },
        })
        .collect()
}
