use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use serde_json::json;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::judge::{self, Action, Binarized, JudgeBatch};
use crate::layers::{self, Line};
use crate::registry::Modules;
use crate::store::{MessageRow, Store};
use crate::text;
use crate::traits::{Channel, Provider, Router};
use crate::types::*;
use crate::{Error, Result, bus, heartbeat, ingest, restore};

/// typing インジケータは Discord 側で約 10 秒で消えるので、それより短い間隔で再送する
const TYPING_INTERVAL: Duration = Duration::from_secs(8);
const SUMMARY_CHARS: usize = 40;
/// 関連チャンク 1 つあたりの本文の上限。候補が多くても Layer 4 が膨らまないようにする
const RELEVANT_CHUNK_MAX_CHARS: usize = 6_000;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// 本線 actor が 1 つずつ処理する仕事。MVP は Judge だけ
#[derive(Debug)]
pub enum Job {
    Judge(JudgeBatch),
}

/// 常駐処理の本体。restore → Channel::run と Ingest と actor の起動 → `shutdown` 完了で終了する。
/// heartbeat の起動と終了時の disconnected 更新もここで行う
pub async fn run(
    config: Config,
    modules: Modules,
    store: Store,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<()> {
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

    let mut actor = Actor::new(config, modules, store.clone(), &members).await?;
    let batches = restore::restore(&store, guild).await?;

    let (jobs_tx, mut jobs_rx) = mpsc::unbounded_channel::<Job>();
    for batch in batches {
        let _ = jobs_tx.send(Job::Judge(batch));
    }

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
    let mut actor_task = tokio::spawn(async move {
        while let Some(job) = jobs_rx.recv().await {
            let Job::Judge(batch) = job;
            if let Err(e) = actor.handle_judge(batch).await {
                tracing::error!(error = %e, "Judge の処理に失敗しました");
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
    // ingest が持つ送信側が落ちると actor のキューが閉じ、キューに残った Judge をすべて処理してから止まる(SHUTDOWN_GRACE まで)
    ingest_task.abort();
    if tokio::time::timeout(SHUTDOWN_GRACE, &mut actor_task)
        .await
        .is_err()
    {
        tracing::warn!("処理中の Judge が終わらないため中断します");
        actor_task.abort();
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

/// Layer 0〜2 は起動時に確定した固定値。Layer 3 は常に Store の active_log を正とする
pub struct Actor {
    config: Config,
    guild: GuildId,
    channel: Arc<dyn Channel>,
    router: Arc<dyn Router>,
    provider: Arc<dyn Provider>,
    store: Store,
    worker_enabled: bool,
    compactor_enabled: bool,
    harness_rules: String,
    profile: String,
    digest: String,
    channel_names: HashMap<ChannelId, String>,
    /// チャンネルごとに、Noth が最後に応答した時刻とそのとき話しかけてきた人。再起動で消えてよい一時的な手がかり
    conversations: HashMap<ChannelId, (Timestamp, HashSet<String>)>,
}

impl Actor {
    pub async fn new(
        config: Config,
        modules: Modules,
        store: Store,
        members: &[Member],
    ) -> Result<Self> {
        let guild = modules.channel.guild_id();
        let digest = store.latest_digest(guild).await?.unwrap_or_default();
        Ok(Self {
            harness_rules: layers::harness_rules(modules.worker_enabled),
            profile: layers::profile_layer(&config, members),
            digest,
            config,
            guild,
            channel: modules.channel,
            router: modules.router,
            provider: modules.provider,
            store,
            worker_enabled: modules.worker_enabled,
            compactor_enabled: modules.compactor_enabled,
            channel_names: HashMap::new(),
            conversations: HashMap::new(),
        })
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

        let action = binarized.effective_action(self.worker_enabled);
        tracing::info!(channel = %batch.channel_name, action = action.as_str(), noise = binarized.is_noise, chunk_id, "judge");
        match action {
            Action::Silent => Ok(()),
            // worker が有効になるまで spawn_task は effective_action で reply_only に落ちている
            Action::ReplyOnly | Action::SpawnTask => {
                self.reply(&batch, chunk_id, &binarized.relevant).await
            }
        }
    }

    async fn reply(&mut self, batch: &JudgeBatch, chunk_id: i64, relevant: &[i64]) -> Result<()> {
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
        let prompt = layers::assemble(
            &self.harness_rules,
            &self.profile,
            &self.digest,
            active_log,
            tail,
        );
        let params = CompletionParams {
            max_tokens: self.config.context.max_tokens,
        };

        let typing = self.spawn_typing(batch.channel_id);
        let completion = self.provider.complete(&prompt, &params).await;
        typing.abort();

        let completion = match completion {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "本線の生成に失敗しました。応答せずバッファに残します");
                return Ok(());
            }
        };
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

        if !self.compactor_enabled {
            self.drop_oldest_chunk_if_over_limit(chunk_id).await?;
        }
        Ok(())
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

    /// compactor が無いときの上限対策。1 回の超過につき 1 チャンクだけ捨てる(パターン D)。
    /// 進行中の `current_chunk_id` は捨てず、残りのうち最後の発言が最も古いものを選ぶ
    async fn drop_oldest_chunk_if_over_limit(&self, current_chunk_id: i64) -> Result<()> {
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
        let Some((oldest, _)) = oldest else {
            return Ok(());
        };
        let ids: Vec<i64> = turns
            .iter()
            .filter(|t| t.chunk_id == Some(oldest))
            .filter_map(|t| t.id)
            .collect();
        tracing::warn!(
            chunk_id = oldest,
            turns = ids.len(),
            "Layer 3 がハード上限を超えたため最も古い TopicChunk を破棄します"
        );
        self.store.remove_turns(&ids).await
    }

    fn spawn_typing(&self, channel_id: ChannelId) -> tokio::task::JoinHandle<()> {
        let channel = self.channel.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = channel.typing(channel_id).await {
                    tracing::debug!(error = %e, "typing を送れませんでした");
                }
                tokio::time::sleep(TYPING_INTERVAL).await;
            }
        })
    }
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
