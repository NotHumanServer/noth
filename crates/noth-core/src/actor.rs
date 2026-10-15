use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use jiff::Timestamp;
use serde_json::json;
use tokio::sync::{Semaphore, mpsc, oneshot};

use crate::config::{CompactorSettings, Config};
use crate::judge::{self, Action, Binarized, DeliveryStyle, JudgeBatch};
use crate::layers::{self, Line};
use crate::registry::Modules;
use crate::store::{MessageRow, NewWorkerRun, RunStatus, Store};
use crate::text;
use crate::traits::{
    Channel, Compactor, Completion, LayeredPrompt, Provider, Router, Worker, WorkerOutcome,
    WorkerTask,
};
use crate::types::*;
use crate::{Error, Result, bus, heartbeat, ingest, progress, restore};

/// typing インジケータは Discord 側で約 10 秒で消えるので、それより短い間隔で再送する
const TYPING_INTERVAL: Duration = Duration::from_secs(8);
const SUMMARY_CHARS: usize = 40;
/// 関連チャンク 1 つあたりの本文の上限。候補が多くても Layer 4 が膨らまないようにする
const RELEVANT_CHUNK_MAX_CHARS: usize = 6_000;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// アイドル判定の周期。idle_minutes は分単位なので、この程度の遅れは問題にならない
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// `[compactor]` を読めないときの keep_recent_turns。cli.md の既定値と同じ
const FALLBACK_KEEP_RECENT_TURNS: usize = 6;

const PROGRESS_FINAL_WAIT: Duration = Duration::from_secs(10);

static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 本線 actor が 1 つずつ処理する仕事(§2.1)
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
    /// worker 有効時に起動時の 1 回だけ取る。呼び出しごとに変わると本線と Worker のキャッシュが外れる
    tools: Vec<ToolSpec>,
    /// Join を送るため。Worker の task にも複製して渡す
    jobs: mpsc::UnboundedSender<Job>,
    /// Fork してまだ Join を処理していない run の数。アイドル判定と終了待ちに使う
    in_flight: usize,
    /// キューに積んだ Compact が対象とするアイドル期間(その時点の人間の最終発言の行 ID)
    compact_queued: Option<i64>,
    /// コンパクションを済ませたアイドル期間。新しい人間の発言が来るまで次を積まない
    compacted_for: Option<i64>,
    harness_rules: String,
    profile: String,
    digest: String,
    channel_names: HashMap<ChannelId, String>,
    /// チャンネルごとに、Noth が最後に応答した時刻とそのとき話しかけてきた人。再起動で消えてよい一時的な手がかり
    conversations: HashMap<ChannelId, (Timestamp, HashSet<String>)>,
    typing_interval: Duration,
    progress_interval: Duration,
}

/// 本線が Worker に任せると決めた内容
struct TaskPlan {
    ack: String,
    goal: String,
    category: TaskCategory,
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
            harness_rules: layers::harness_rules(worker.is_some()),
            profile: layers::profile_layer(&config, members),
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
            compact_queued: None,
            compacted_for: None,
            channel_names: HashMap::new(),
            conversations: HashMap::new(),
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
        let turns = self.store.active_turns(guild).await?;
        // Layer 3・未昇格バッファ・今の会話の本文は本線に見えているか、このあと昇格して見えるので、関連チャンクの候補にしない。
        // バッファを漏らすと同じ本文が Layer 3 と Layer 4 に二重に載る
        let mut exclude: Vec<i64> = turns.iter().filter_map(|t| t.chunk_id).collect();
        exclude.extend(
            self.store
                .buffered_messages(guild)
                .await?
                .iter()
                .filter_map(|r| r.chunk_id),
        );
        exclude.extend(current.as_ref().map(|c| c.id));
        exclude.sort_unstable();
        exclude.dedup();

        let limit = self.config.router.candidate_chunks.min(
            self.router
                .max_questions()
                .saturating_sub(judge::FIXED_QUESTIONS),
        );
        let candidates = self.store.candidate_chunks(guild, limit, &exclude).await?;
        let first_row_id = batch
            .messages
            .iter()
            .map(|m| m.row_id)
            .min()
            .unwrap_or(i64::MAX);
        let recent = self
            .store
            .recent_messages(guild, first_row_id, self.config.router.recent_messages)
            .await?;
        for r in &recent {
            self.channel_names
                .entry(r.channel_id)
                .or_insert_with(|| r.channel_name.clone());
        }

        let state = judge::build_state(
            &batch,
            &recent,
            &candidates,
            &self.channel_names,
            &self.config.router,
        );
        let questions = judge::build_questions(&candidates);
        let candidate_ids: Vec<i64> = candidates.iter().map(|c| c.id).collect();

        let called_by_name = batch
            .messages
            .iter()
            .any(|m| text::contains_any_name(&m.content, &self.config.router.call_names));
        // Clef は Noth の応答直後の返事を Noth 宛てと見なさないことが多いので、名前で呼ばれたのと同じ扱いにする
        let in_conversation = self.continues_conversation(&batch);
        let (mut binarized, kind, answers, latency_ms) =
            match self.router.decide(&state, &questions).await {
                Ok(decision) => (
                    judge::binarize(
                        &decision,
                        &candidate_ids,
                        &self.config.router.thresholds,
                        called_by_name || in_conversation,
                    ),
                    "judge",
                    judge::answers_json(&decision),
                    decision.latency.as_millis() as i64,
                ),
                Err(e) => {
                    tracing::warn!(error = %e, "router の判定に失敗しました。静観として扱います");
                    let mut safe = Binarized::safe();
                    // 名指しへの無応答は故障に見えるので、router が落ちていても応答する
                    if called_by_name {
                        safe.answer_direct_call();
                    }
                    (safe, "judge_error", json!({ "error": e.to_string() }), 0)
                }
            };
        if batch
            .messages
            .iter()
            .any(|m| m.mentions_self || m.reply_to_self)
        {
            binarized.answer_direct_call();
        }
        let decision_id = self
            .store
            .insert_decision(
                guild,
                kind,
                &self.router.model_label(),
                &answers,
                latency_ms,
            )
            .await?;

        let summary = summarize(&first.content);
        // ノイズの本文は要約にも新しいチャンクの種にもしない。チャンクが無いときだけは割り当て先が要るので作る
        let chunk_id = match current {
            Some(c) if binarized.is_noise => {
                self.store.touch_chunk(c.id, last.at, None).await?;
                c.id
            }
            Some(c) if !binarized.is_topic_shift => {
                self.store
                    .touch_chunk(c.id, last.at, Some(&summary))
                    .await?;
                c.id
            }
            _ => {
                self.store
                    .create_chunk(guild, batch.channel_id, first.at, &summary)
                    .await?
            }
        };
        let row_ids: Vec<i64> = batch.messages.iter().map(|m| m.row_id).collect();
        self.store
            .mark_judged(&row_ids, decision_id, chunk_id, binarized.is_noise)
            .await?;

        let action = binarized.effective_action(self.worker.is_some());
        tracing::info!(channel = %batch.channel_name, action = action.as_str(), noise = binarized.is_noise, chunk_id, "judge");
        match action {
            Action::Silent => Ok(()),
            Action::ReplyOnly | Action::SpawnTask => {
                self.respond(
                    &batch,
                    chunk_id,
                    &binarized.relevant,
                    binarized.task_category,
                )
                .await
            }
        }
    }

    /// パターン B と C。spawn_task かどうかは router の判定ではなく本線の応答の形で決める
    async fn respond(
        &mut self,
        batch: &JudgeBatch,
        chunk_id: i64,
        relevant: &[i64],
        category: TaskCategory,
    ) -> Result<()> {
        let guild = self.guild;
        let trigger_ids: HashSet<i64> = batch.messages.iter().map(|m| m.row_id).collect();

        self.promote_buffer(&trigger_ids).await?;

        let trigger_lines: Vec<Line> = batch
            .messages
            .iter()
            .map(|m| Line {
                channel: batch.channel_name.clone(),
                author: m.author_name.clone(),
                at: m.at,
                content: m.content.clone(),
            })
            .collect();
        let trigger = layers::trigger_turn(chunk_id, &trigger_lines);

        let mut tail = Vec::new();
        for id in relevant {
            let rows: Vec<MessageRow> = self
                .store
                .chunk_messages(*id)
                .await?
                .into_iter()
                .filter(|r| r.is_noise != Some(true))
                .collect();
            if rows.is_empty() {
                continue;
            }
            tail.push(layers::relevant_chunk_turn(
                *id,
                &limit_chars(rows_to_lines(&rows), RELEVANT_CHUNK_MAX_CHARS),
            ));
        }
        tail.push(trigger.clone());

        let active_log = self.store.active_turns(guild).await?;
        let prompt = self.prompt(active_log, tail);
        let completion = match self
            .complete_with_typing(batch.channel_id, &prompt, Purpose::Main, None)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "本線の生成に失敗しました。応答せずバッファに残します");
                return Ok(());
            }
        };

        if let Some(plan) = self.task_plan(&completion, category) {
            return self
                .start_task(batch, chunk_id, trigger, plan, &completion.usage)
                .await;
        }
        let Some(text) = completion
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            tracing::info!("本線の応答が空でした。投稿せずバッファに残します");
            return Ok(());
        };
        if let Err(e) = self.channel.send(batch.channel_id, text, None).await {
            tracing::warn!(error = %e, "応答を投稿できませんでした。バッファに残します");
            return Ok(());
        }

        self.note_conversation(batch);
        let row_ids: Vec<i64> = batch.messages.iter().map(|m| m.row_id).collect();
        self.store
            .record_reply(
                guild,
                &trigger,
                &layers::response_turn(chunk_id, text),
                &row_ids,
                &completion.usage,
            )
            .await?;
        self.enforce_hard_limit(chunk_id).await
    }

    /// 本線の応答が Worker への委譲かどうか。router が reply_only と判定していても、本線が `[Goal]` を書いたか
    /// tool を呼んだなら委譲する。そのまま投稿すると Goal が Discord に漏れ、tool_calls は投稿できる本文が無いため
    fn task_plan(&self, completion: &Completion, category: TaskCategory) -> Option<TaskPlan> {
        self.worker.as_ref()?;
        if !completion.tool_calls.is_empty() {
            return Some(TaskPlan {
                ack: layers::FIXED_ACK.to_string(),
                goal: layers::goal_from_tool_calls(&completion.tool_calls),
                category: category_for_tools(&completion.tool_calls, category),
            });
        }
        // `[Goal]` 行が無いのは、本線が調べなくても答えられると判断したか書式を守らなかったとき。
        // Goal の無い Worker には任せる内容が無いので、spawn_task でも普通の返事として投稿する
        let (ack, goal) = layers::split_ack_goal(completion.text.as_deref()?)?;
        Some(TaskPlan {
            ack,
            goal,
            // router が作業不要と見ていた場合は shell を許さない側に倒す(security.md §3)
            category: match category {
                TaskCategory::None => TaskCategory::WebResearch,
                c => c,
            },
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
        let ack_message_id = match self.channel.send(batch.channel_id, &plan.ack, None).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "Ack を投稿できませんでした。Worker を起動せずバッファに残します");
                return Ok(());
            }
        };
        self.note_conversation(batch);

        let row_ids: Vec<i64> = batch.messages.iter().map(|m| m.row_id).collect();
        let trigger_message_id = match row_ids.iter().max() {
            Some(id) => self.store.message(*id).await?.map(|m| m.message_id),
            None => None,
        };
        let run = NewWorkerRun {
            run_id: next_run_id(),
            goal: plan.goal.clone(),
            task_category: plan.category,
            channel_id: batch.channel_id,
            chunk_id,
            trigger_message_id,
            fork_message_id: self.store.max_message_id(guild).await?,
        };
        // 進捗の追記は Discord 上の見た目だけで、Layer 3 には Ack + Goal だけを残す。
        // 進捗は tool 実行の生ログに当たるので本線に入れず(設計原則 2)、編集のたびに Layer 3 を書き換えてキャッシュを外さないため
        self.store
            .record_task(
                guild,
                &trigger,
                &layers::task_response_turn(chunk_id, &plan.ack, &plan.goal),
                &row_ids,
                usage,
                &run,
            )
            .await?;
        tracing::info!(run_id = %run.run_id, category = crate::store::category_str(run.task_category), "Worker を起動します");
        // Worker の base も本線と同じ上限に収めるため、Fork より前に緊急コンパクションを済ませる
        self.enforce_hard_limit(chunk_id).await?;

        let base = self.prompt(self.store.active_turns(guild).await?, Vec::new());
        self.fork(run, base, ack_message_id, plan.ack);
        Ok(())
    }

    /// 本線の Layer 0〜3 と tool 定義を複製して Worker をバックグラウンドで動かし、終わったら Join を積む
    fn fork(
        &mut self,
        run: NewWorkerRun,
        base: LayeredPrompt,
        ack_message_id: MessageId,
        ack: String,
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
        // Ack の後に黙り込むと止まったように見えるので、キュー待ちの間も含めて Worker が終わるまで入力中を出し続ける
        let typing = AbortOnDrop(self.spawn_typing(run.channel_id).abort_handle());
        // 2,000 文字を超えた Ack は分割投稿されていて、最後の断片に Ack 全体を書き戻せないので進捗を出さない
        let show_progress = pool.show_progress && ack.chars().count() <= progress::MAX_CHARS;
        let (progress_tx, progress) = if show_progress {
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
            task_category: run.task_category,
            base,
            progress: progress_tx,
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

        let since_fork: Vec<MessageRow> = self
            .store
            .recent_messages(guild, i64::MAX, self.config.router.recent_messages)
            .await?
            .into_iter()
            .filter(|r| r.id > run.fork_message_id)
            .collect();
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
            .decide(&state, &judge::join_questions())
            .await
        {
            Ok(decision) => (
                judge::binarize_delivery(&decision, self.config.router.thresholds.min_confidence),
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
                Some(text) => match self.channel.send(run.channel_id, text, reply_to).await {
                    Ok(_) => Some(text.to_string()),
                    Err(e) => {
                        tracing::warn!(error = %e, run_id, "報告を投稿できませんでした");
                        None
                    }
                },
                None => {
                    tracing::info!(run_id, "報告の応答が空でした");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, run_id, "報告の生成に失敗しました");
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
            // 報告できなくても結論メモだけは Layer 3 に残す。Ack で待たせているので、次に本線が話すときに
            // 結果を踏まえられるようにするため。投稿していない応答は残さない(言っていないことを言ったと思わせない)
            // usage の無い追記なので、上限の判定は次に usage を書いたときに回す
            _ => {
                self.store.append_turn(guild, &conclusion).await?;
                return Ok(());
            }
        }
        self.enforce_hard_limit(chunk_id).await
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
        let episode = match compactor.digest(&self.harness_rules, range).await {
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
        Ok(())
    }

    fn prompt(&self, active_log: Vec<Turn>, tail: Vec<Turn>) -> LayeredPrompt {
        layers::assemble(
            &self.harness_rules,
            &self.profile,
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

    fn continues_conversation(&self, batch: &JudgeBatch) -> bool {
        let Some((replied_at, authors)) = self.conversations.get(&batch.channel_id) else {
            return false;
        };
        let window = self.config.router.conversation_window_secs as i64;
        batch.messages.iter().any(|m| {
            authors.contains(&m.author_name) && m.at.as_second() - replied_at.as_second() <= window
        })
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
            match compactor.digest(&self.harness_rules, &target).await {
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

/// shell を許す code_or_tool は、router もそう判定し、かつ本線が shell だけを呼んだときに限る。
/// 本線の tool 選びだけで shell が通るようにしないため。1 つの run は Web か shell のどちらかしか使えないので(approval.rs)、それ以外は web_research
fn category_for_tools(calls: &[ToolCall], router_category: TaskCategory) -> TaskCategory {
    if router_category == TaskCategory::CodeOrTool
        && calls.iter().all(|c| c.name == crate::approval::SHELL_TOOL)
    {
        TaskCategory::CodeOrTool
    } else {
        TaskCategory::WebResearch
    }
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

fn rows_to_lines(rows: &[MessageRow]) -> Vec<Line> {
    rows.iter()
        .map(|r| Line {
            channel: r.channel_name.clone(),
            author: r.author_name.clone(),
            at: r.created_at,
            content: r.content.clone(),
        })
        .collect()
}

/// 古い行から落として本文の合計を上限に収める
fn limit_chars(mut lines: Vec<Line>, max: usize) -> Vec<Line> {
    let mut total: usize = lines.iter().map(|l| l.content.chars().count()).sum();
    while total > max && lines.len() > 1 {
        total -= lines.remove(0).content.chars().count();
    }
    lines
}

fn summarize(content: &str) -> String {
    let flat: String = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s: String = flat.chars().take(SUMMARY_CHARS).collect();
    if flat.chars().count() > SUMMARY_CHARS {
        s.push('…');
    }
    s
}
