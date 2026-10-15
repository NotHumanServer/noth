//! 時刻はすべて UNIX ミリ秒の INTEGER、Discord の ID は SQLite に符号なしを持てないので i64 にキャストして保存する
use std::path::Path;
use std::str::FromStr;

use jiff::Timestamp;
use serde_json::Value;

use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};

use crate::history::{HistoryQuery, MessageHistory};
use crate::types::*;
use crate::{Error, Result};

#[derive(Clone)]
pub struct Store {
    pool: sqlx::SqlitePool,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

#[crate::async_trait]
impl MessageHistory for Store {
    async fn search(&self, q: &HistoryQuery) -> Result<Vec<MessageRow>> {
        self.search_messages(q).await
    }
}

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub id: i64,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub channel_name: String,
    pub message_id: MessageId,
    pub user_id: UserId,
    pub author_name: String,
    pub is_self: bool,
    pub mentions_self: bool,
    pub reply_to_self: bool,
    pub reply_to: Option<ReplyTo>,
    pub content: String,
    pub created_at: Timestamp,
    pub chunk_id: Option<i64>,
    /// 判定前は None
    pub is_noise: Option<bool>,
    pub decision_id: Option<i64>,
    pub promoted_turn_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ChunkRow {
    pub id: i64,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub started_at: Timestamp,
    pub ended_at: Timestamp,
    /// 候補用の機械要約(冒頭数件の発言をつないだもの)
    pub summary: String,
    pub compacted: bool,
}

#[derive(Debug, Clone)]
pub struct DecisionRow {
    pub id: i64,
    pub kind: String,
    pub answers: Value,
    pub latency_ms: i64,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone)]
pub struct HeartbeatRow {
    pub guild_id: GuildId,
    pub updated_at: Timestamp,
    /// connected / disconnected
    pub state: String,
    /// 有効な module など。`noth status` が表示する
    pub info: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Queued,
    Running,
    Finished,
    Failed,
    Aborted,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "finished" => Self::Finished,
            "failed" => Self::Failed,
            "aborted" => Self::Aborted,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct WorkerRunRow {
    pub run_id: String,
    pub status: RunStatus,
    pub task_category: TaskCategory,
    pub goal: String,
    /// queued の間は None。running になった時刻
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub channel_id: ChannelId,
    pub chunk_id: Option<i64>,
    /// reply_anchor のリプライ先。トリガー発言群の最後の Discord メッセージ
    pub trigger_message_id: Option<MessageId>,
    /// これより大きい messages.id が「Fork 以降に届いた発言」
    pub fork_message_id: i64,
    /// finished なら結論メモ、failed なら失敗理由
    pub conclusion: Option<String>,
    pub delivery_style: Option<String>,
}

/// `cost_summary` の集計キー
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostGroup {
    Source,
    Model,
    Purpose,
    /// ローカル時刻の日付(YYYY-MM-DD)
    Day,
    /// run_id の無い呼び出し(本線など)はキー `-` にまとめる
    Run,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostSummaryRow {
    pub key: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    /// トークン以外の課金単位ごとの数量の合計。単位名の順
    pub units: Vec<(String, u64)>,
    /// 単価の分かる分だけの合計
    pub amount_usd: f64,
    /// 従量課金なのに単価が分からず amount_usd に入っていない呼び出しの数
    pub unpriced_calls: u64,
}

#[derive(Debug, Clone)]
pub struct NewWorkerRun {
    pub run_id: String,
    pub goal: String,
    pub task_category: TaskCategory,
    pub channel_id: ChannelId,
    pub chunk_id: i64,
    pub trigger_message_id: Option<MessageId>,
    pub fork_message_id: i64,
}

/// 機械要約の上限。候補用なので短く保つ
const SUMMARY_MAX_CHARS: usize = 200;

const MESSAGE_COLUMNS: &str = "id, guild_id, channel_id, channel_name, message_id, user_id, author_name, is_self, mentions_self, reply_to_self, reply_to_message_id, reply_to_author, reply_to_is_self, reply_to_excerpt, content, created_at, chunk_id, is_noise, decision_id, promoted_turn_id";
const CHUNK_COLUMNS: &str = "id, guild_id, channel_id, started_at, ended_at, summary, compacted";
const RUN_COLUMNS: &str = "run_id, status, task_category, goal, started_at, finished_at, channel_id, chunk_id, trigger_message_id, fork_message_id, conclusion, delivery_style";

fn ts(ms: i64) -> Timestamp {
    // 範囲外の値は壊れた行なので epoch に倒す(読み出しで panic させない)
    Timestamp::from_millisecond(ms).unwrap_or(Timestamp::UNIX_EPOCH)
}

fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

fn message_row(r: &SqliteRow) -> Result<MessageRow> {
    Ok(MessageRow {
        id: r.try_get("id")?,
        guild_id: r.try_get::<i64, _>("guild_id")? as u64,
        channel_id: r.try_get::<i64, _>("channel_id")? as u64,
        channel_name: r.try_get("channel_name")?,
        message_id: r.try_get::<i64, _>("message_id")? as u64,
        user_id: r.try_get::<i64, _>("user_id")? as u64,
        author_name: r.try_get("author_name")?,
        is_self: r.try_get("is_self")?,
        mentions_self: r.try_get("mentions_self")?,
        reply_to_self: r.try_get("reply_to_self")?,
        reply_to: reply_to_of(r)?,
        content: r.try_get("content")?,
        created_at: ts(r.try_get("created_at")?),
        chunk_id: r.try_get("chunk_id")?,
        is_noise: r.try_get("is_noise")?,
        decision_id: r.try_get("decision_id")?,
        promoted_turn_id: r.try_get("promoted_turn_id")?,
    })
}

fn reply_to_of(r: &SqliteRow) -> Result<Option<ReplyTo>> {
    let Some(message_id) = r.try_get::<Option<i64>, _>("reply_to_message_id")? else {
        return Ok(None);
    };
    Ok(Some(ReplyTo {
        message_id: message_id as u64,
        author_name: r
            .try_get::<Option<String>, _>("reply_to_author")?
            .unwrap_or_default(),
        is_self: r
            .try_get::<Option<bool>, _>("reply_to_is_self")?
            .unwrap_or(false),
        excerpt: r
            .try_get::<Option<String>, _>("reply_to_excerpt")?
            .unwrap_or_default(),
    }))
}

fn chunk_row(r: &SqliteRow) -> Result<ChunkRow> {
    Ok(ChunkRow {
        id: r.try_get("id")?,
        guild_id: r.try_get::<i64, _>("guild_id")? as u64,
        channel_id: r.try_get::<i64, _>("channel_id")? as u64,
        started_at: ts(r.try_get("started_at")?),
        ended_at: ts(r.try_get("ended_at")?),
        summary: r.try_get("summary")?,
        compacted: r.try_get("compacted")?,
    })
}

fn run_row(r: &SqliteRow) -> Result<WorkerRunRow> {
    let status: String = r.try_get("status")?;
    let category: String = r.try_get("task_category")?;
    Ok(WorkerRunRow {
        run_id: r.try_get("run_id")?,
        status: RunStatus::parse(&status)
            .ok_or_else(|| Error::Other(format!("worker_runs: 未知の status {status:?}")))?,
        task_category: parse_category(&category).ok_or_else(|| {
            Error::Other(format!("worker_runs: 未知の task_category {category:?}"))
        })?,
        goal: r.try_get("goal")?,
        started_at: r.try_get::<Option<i64>, _>("started_at")?.map(ts),
        finished_at: r.try_get::<Option<i64>, _>("finished_at")?.map(ts),
        channel_id: r.try_get::<i64, _>("channel_id")? as u64,
        chunk_id: r.try_get("chunk_id")?,
        trigger_message_id: r
            .try_get::<Option<i64>, _>("trigger_message_id")?
            .map(|id| id as u64),
        fork_message_id: r.try_get("fork_message_id")?,
        conclusion: r.try_get("conclusion")?,
        delivery_style: r.try_get("delivery_style")?,
    })
}

pub fn category_str(c: TaskCategory) -> &'static str {
    match c {
        TaskCategory::None => "none",
        TaskCategory::WebResearch => "web_research",
        TaskCategory::FactCheck => "fact_check",
        TaskCategory::CodeOrTool => "code_or_tool",
    }
}

fn parse_category(s: &str) -> Option<TaskCategory> {
    Some(match s {
        "none" => TaskCategory::None,
        "web_research" => TaskCategory::WebResearch,
        "fact_check" => TaskCategory::FactCheck,
        "code_or_tool" => TaskCategory::CodeOrTool,
        _ => return None,
    })
}

fn turn_role(s: &str) -> Result<Role> {
    match s {
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        other => Err(Error::Other(format!("active_log: 未知の role {other:?}"))),
    }
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // 発言の全文が入るので、SQLite が umask 任せで作る前に 0600 で用意しておく
        #[cfg(unix)]
        create_private_file(path)?;
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        let store = Self::migrated(pool).await?;
        #[cfg(unix)]
        restrict_sidecar_files(path)?;
        Ok(store)
    }
    /// テスト用
    pub async fn open_memory() -> Result<Self> {
        // 接続ごとに別 DB になるので 1 本に固定する
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        Self::migrated(pool).await
    }

    async fn migrated(pool: sqlx::SqlitePool) -> Result<Self> {
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    // --- messages ---
    pub async fn insert_message(&self, msg: &IncomingMessage) -> Result<i64> {
        let r = sqlx::query(
            "INSERT INTO messages (guild_id, channel_id, channel_name, message_id, user_id, author_name, is_self, mentions_self, reply_to_self, content, created_at, reply_to_message_id, reply_to_author, reply_to_is_self, reply_to_excerpt)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(msg.guild_id as i64)
        .bind(msg.channel_id as i64)
        .bind(&msg.channel_name)
        .bind(msg.message_id as i64)
        .bind(msg.user_id as i64)
        .bind(&msg.author_name)
        .bind(msg.is_self)
        .bind(msg.mentions_self)
        .bind(msg.reply_to_self)
        .bind(&msg.content)
        .bind(msg.timestamp.as_millisecond())
        .bind(msg.reply_to.as_ref().map(|r| r.message_id as i64))
        .bind(msg.reply_to.as_ref().map(|r| r.author_name.as_str()))
        .bind(msg.reply_to.as_ref().map(|r| r.is_self))
        .bind(msg.reply_to.as_ref().map(|r| r.excerpt.as_str()))
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }
    pub async fn mark_judged(
        &self,
        ids: &[i64],
        decision_id: i64,
        chunk_id: i64,
        is_noise: bool,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for id in ids {
            sqlx::query(
                "UPDATE messages SET decision_id = ?, chunk_id = ?, is_noise = ? WHERE id = ?",
            )
            .bind(decision_id)
            .bind(chunk_id)
            .bind(is_noise)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn mark_promoted(&self, ids: &[i64], turn_id: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        promote_rows(&mut tx, ids, turn_id).await?;
        tx.commit().await?;
        Ok(())
    }
    /// 全チャンネルの `before_id` より前の直近 `limit` 件を古い順で。
    /// キュー待ちの間に届いた判定対象より新しい発言を router に見せないために上限を取る
    pub async fn recent_messages(
        &self,
        guild: GuildId,
        before_id: i64,
        limit: usize,
    ) -> Result<Vec<MessageRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE guild_id = ? AND id < ? ORDER BY id DESC LIMIT ?"
        )))
        .bind(guild as i64)
        .bind(before_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        let mut out = rows.iter().map(message_row).collect::<Result<Vec<_>>>()?;
        out.reverse();
        Ok(out)
    }
    pub async fn messages_after(
        &self,
        guild: GuildId,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<MessageRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE guild_id = ? AND id > ? ORDER BY id ASC LIMIT ?"
        )))
        .bind(guild as i64)
        .bind(after_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(message_row).collect()
    }
    pub async fn message(&self, id: i64) -> Result<Option<MessageRow>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE id = ?"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(message_row).transpose()
    }
    /// 新しい順。文字列の絞り込みは SQLite の lower() に頼るので、大文字小文字を区別しないのは ASCII だけ。SQLite 側の lower() に合わせ、引数も ASCII だけ小文字にする
    pub async fn search_messages(&self, q: &HistoryQuery) -> Result<Vec<MessageRow>> {
        let lower = |s: &Option<String>| s.as_ref().map(|s| s.to_ascii_lowercase());
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE guild_id = ?
               AND (? IS NULL OR channel_id = ?)
               AND (? IS NULL OR lower(channel_name) = ?)
               AND (? IS NULL OR instr(lower(content), ?) > 0)
               AND (? IS NULL OR instr(lower(author_name), ?) > 0)
               AND (? IS NULL OR created_at >= ?)
               AND (? IS NULL OR created_at < ?)
             ORDER BY created_at DESC, id DESC LIMIT ?"
        )))
        .bind(q.guild_id as i64)
        .bind(q.channel_id.map(|c| c as i64))
        .bind(q.channel_id.map(|c| c as i64))
        .bind(lower(&q.channel_name))
        .bind(lower(&q.channel_name))
        .bind(lower(&q.text))
        .bind(lower(&q.text))
        .bind(lower(&q.author))
        .bind(lower(&q.author))
        .bind(q.since.map(|t| t.as_millisecond()))
        .bind(q.since.map(|t| t.as_millisecond()))
        .bind(q.until.map(|t| t.as_millisecond()))
        .bind(q.until.map(|t| t.as_millisecond()))
        .bind(q.limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(message_row).collect()
    }
    /// 発言が 1 件も無ければ 0
    pub async fn max_message_id(&self, guild: GuildId) -> Result<i64> {
        Ok(
            sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(id) FROM messages WHERE guild_id = ?")
                .bind(guild as i64)
                .fetch_one(&self.pool)
                .await?
                .unwrap_or(0),
        )
    }
    /// アイドル判定に使う、Noth 以外の最後の発言の (行 ID, 時刻)
    pub async fn last_human_message(&self, guild: GuildId) -> Result<Option<(i64, Timestamp)>> {
        let row = sqlx::query(
            "SELECT id, created_at FROM messages WHERE guild_id = ? AND is_self = 0 ORDER BY id DESC LIMIT 1",
        )
        .bind(guild as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| -> Result<(i64, Timestamp)> {
            Ok((r.try_get("id")?, ts(r.try_get("created_at")?)))
        })
        .transpose()
    }
    /// Noth 自身の発言は判定対象にしない
    pub async fn unjudged_messages(&self, guild: GuildId) -> Result<Vec<MessageRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE guild_id = ? AND decision_id IS NULL AND is_self = 0 ORDER BY id ASC"
        )))
        .bind(guild as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(message_row).collect()
    }
    pub async fn buffered_messages(&self, guild: GuildId) -> Result<Vec<MessageRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages
             WHERE guild_id = ? AND decision_id IS NOT NULL AND is_noise = 0 AND promoted_turn_id IS NULL AND is_self = 0
             ORDER BY id ASC"
        )))
        .bind(guild as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(message_row).collect()
    }
    pub async fn chunk_messages(&self, chunk_id: i64) -> Result<Vec<MessageRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE chunk_id = ? ORDER BY id ASC"
        )))
        .bind(chunk_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(message_row).collect()
    }

    // --- topic_chunks ---
    pub async fn create_chunk(
        &self,
        guild: GuildId,
        channel: ChannelId,
        started_at: Timestamp,
        summary: &str,
    ) -> Result<i64> {
        let ms = started_at.as_millisecond();
        let r = sqlx::query(
            "INSERT INTO topic_chunks (guild_id, channel_id, started_at, ended_at, summary) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(guild as i64)
        .bind(channel as i64)
        .bind(ms)
        .bind(ms)
        .bind(truncate_chars(summary, SUMMARY_MAX_CHARS))
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }
    pub async fn current_chunk(
        &self,
        guild: GuildId,
        channel: ChannelId,
    ) -> Result<Option<ChunkRow>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {CHUNK_COLUMNS} FROM topic_chunks WHERE guild_id = ? AND channel_id = ? ORDER BY id DESC LIMIT 1"
        )))
        .bind(guild as i64)
        .bind(channel as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(chunk_row).transpose()
    }
    /// 読んでから書くと deferred トランザクションの昇格で SQLITE_BUSY になりうるので、1 文で済ませる
    pub async fn touch_chunk(
        &self,
        chunk_id: i64,
        ended_at: Timestamp,
        append_summary: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE topic_chunks SET ended_at = ?1,
               summary = CASE
                 WHEN ?2 IS NOT NULL AND ?2 <> '' AND length(summary) < ?3
                 THEN substr(CASE WHEN summary = '' THEN ?2 ELSE summary || ' / ' || ?2 END, 1, ?3)
                 ELSE summary
               END
             WHERE id = ?4",
        )
        .bind(ended_at.as_millisecond())
        .bind(append_summary)
        .bind(SUMMARY_MAX_CHARS as i64)
        .bind(chunk_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn candidate_chunks(
        &self,
        guild: GuildId,
        limit: usize,
        exclude: &[i64],
    ) -> Result<Vec<ChunkRow>> {
        let mut sql = format!("SELECT {CHUNK_COLUMNS} FROM topic_chunks WHERE guild_id = ?");
        if !exclude.is_empty() {
            let marks = vec!["?"; exclude.len()].join(", ");
            sql.push_str(&format!(" AND id NOT IN ({marks})"));
        }
        sql.push_str(" ORDER BY ended_at DESC, id DESC LIMIT ?");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(guild as i64);
        for id in exclude {
            q = q.bind(*id);
        }
        let rows = q.bind(limit as i64).fetch_all(&self.pool).await?;
        rows.iter().map(chunk_row).collect()
    }
    pub async fn chunk(&self, chunk_id: i64) -> Result<Option<ChunkRow>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {CHUNK_COLUMNS} FROM topic_chunks WHERE id = ?"
        )))
        .bind(chunk_id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(chunk_row).transpose()
    }

    // --- decisions ---
    pub async fn insert_decision(
        &self,
        guild: GuildId,
        kind: &str,
        model: &str,
        answers: &Value,
        latency_ms: i64,
    ) -> Result<i64> {
        let r = sqlx::query("INSERT INTO decisions (guild_id, kind, model, answers, latency_ms, created_at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(guild as i64)
            .bind(kind)
            .bind(model)
            .bind(serde_json::to_string(answers)?)
            .bind(latency_ms)
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
        Ok(r.last_insert_rowid())
    }
    pub async fn latest_decision(&self, guild: GuildId) -> Result<Option<DecisionRow>> {
        let row = sqlx::query(
            "SELECT id, kind, answers, latency_ms, created_at FROM decisions WHERE guild_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(guild as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| -> Result<DecisionRow> {
            Ok(DecisionRow {
                id: r.try_get("id")?,
                kind: r.try_get("kind")?,
                answers: serde_json::from_str(&r.try_get::<String, _>("answers")?)?,
                latency_ms: r.try_get("latency_ms")?,
                created_at: ts(r.try_get("created_at")?),
            })
        })
        .transpose()
    }

    // --- active_log(Layer 3)---
    pub async fn append_turn(&self, guild: GuildId, turn: &Turn) -> Result<i64> {
        insert_turn(&self.pool, guild, turn).await
    }
    /// 未昇格バッファのブロックを Layer 3 へ移す。途中で落ちても二重昇格や取りこぼしが起きないよう 1 トランザクションにする
    pub async fn promote_block(&self, guild: GuildId, turn: &Turn, ids: &[i64]) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        let turn_id = insert_turn(&mut *tx, guild, turn).await?;
        promote_rows(&mut tx, ids, turn_id).await?;
        tx.commit().await?;
        Ok(turn_id)
    }
    /// 応答の投稿後に Layer 3 へ残す一式。trigger だけ残って response が無い状態を作らないよう 1 トランザクションにする
    pub async fn record_reply(
        &self,
        guild: GuildId,
        trigger: &Turn,
        response: &Turn,
        ids: &[i64],
        usage: &Usage,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let trigger_id = insert_turn(&mut *tx, guild, trigger).await?;
        let response_id = insert_turn(&mut *tx, guild, response).await?;
        promote_rows(&mut tx, ids, trigger_id).await?;
        write_usage(&mut *tx, response_id, usage).await?;
        tx.commit().await?;
        Ok(())
    }
    /// spawn_task の Ack + Goal を Layer 3 に残し、run を queued で登録する。
    /// Worker の文脈にトリガーと Goal を含めるため Fork より前に呼ぶ。run だけ残って Goal が無い状態を作らないよう 1 トランザクションにする
    pub async fn record_task(
        &self,
        guild: GuildId,
        trigger: &Turn,
        response: &Turn,
        ids: &[i64],
        usage: &Usage,
        run: &NewWorkerRun,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let trigger_id = insert_turn(&mut *tx, guild, trigger).await?;
        let response_id = insert_turn(&mut *tx, guild, response).await?;
        promote_rows(&mut tx, ids, trigger_id).await?;
        write_usage(&mut *tx, response_id, usage).await?;
        insert_run(&mut *tx, guild, run).await?;
        tx.commit().await?;
        Ok(())
    }
    /// Layer 3 の範囲を Layer 2 の要約に置き換える。要約だけ増えてターンが残る(またはその逆の)状態を作らないよう 1 トランザクションにする
    pub async fn replace_with_digest(
        &self,
        guild: GuildId,
        digest: &str,
        turn_ids: &[i64],
        compacted_chunks: &[i64],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO digests (guild_id, text, created_at) VALUES (?, ?, ?)")
            .bind(guild as i64)
            .bind(digest)
            .bind(now_ms())
            .execute(&mut *tx)
            .await?;
        for id in turn_ids {
            sqlx::query("UPDATE active_log SET removed = 1 WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        for id in compacted_chunks {
            sqlx::query("UPDATE topic_chunks SET compacted = 1 WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    /// 削除されていないターンを追記順で
    pub async fn active_turns(&self, guild: GuildId) -> Result<Vec<Turn>> {
        let rows = sqlx::query("SELECT id, role, kind, text, chunk_id FROM active_log WHERE guild_id = ? AND removed = 0 ORDER BY id ASC")
            .bind(guild as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| {
                let kind: String = r.try_get("kind")?;
                Ok(Turn {
                    id: Some(r.try_get("id")?),
                    role: turn_role(&r.try_get::<String, _>("role")?)?,
                    kind: TurnKind::parse(&kind)
                        .ok_or_else(|| Error::Other(format!("active_log: 未知の kind {kind:?}")))?,
                    text: r.try_get("text")?,
                    chunk_id: r.try_get("chunk_id")?,
                })
            })
            .collect()
    }
    pub async fn remove_turns(&self, ids: &[i64]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for id in ids {
            sqlx::query("UPDATE active_log SET removed = 1 WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    /// そのターンを Layer 3 末尾にしたリクエストの usage
    pub async fn set_turn_usage(&self, turn_id: i64, usage: &Usage) -> Result<()> {
        write_usage(&self.pool, turn_id, usage).await
    }
    pub async fn last_usage(&self, guild: GuildId) -> Result<Option<Usage>> {
        let row = sqlx::query(
            "SELECT input_tokens, cached_tokens, output_tokens FROM active_log
             WHERE guild_id = ? AND output_tokens IS NOT NULL ORDER BY id DESC LIMIT 1",
        )
        .bind(guild as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| -> Result<Usage> {
            Ok(Usage {
                input_tokens: r.try_get::<i64, _>("input_tokens")? as u64,
                cached_tokens: r.try_get::<i64, _>("cached_tokens")? as u64,
                output_tokens: r.try_get::<i64, _>("output_tokens")? as u64,
            })
        })
        .transpose()
    }

    // --- digests(Layer 2)---
    pub async fn latest_digest(&self, guild: GuildId) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT text FROM digests WHERE guild_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(guild as i64)
        .fetch_optional(&self.pool)
        .await?)
    }
    pub async fn insert_digest(&self, guild: GuildId, text: &str) -> Result<i64> {
        let r = sqlx::query("INSERT INTO digests (guild_id, text, created_at) VALUES (?, ?, ?)")
            .bind(guild as i64)
            .bind(text)
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
        Ok(r.last_insert_rowid())
    }

    // --- memory_versions ---
    /// `content` は書き換える前の MEMORY.md
    pub async fn insert_memory_version(
        &self,
        guild: GuildId,
        reason: &str,
        content: &str,
    ) -> Result<i64> {
        let r = sqlx::query(
            "INSERT INTO memory_versions (guild_id, reason, content, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(guild as i64)
        .bind(reason)
        .bind(content)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }
    /// 新しい順の (reason, content)
    pub async fn memory_versions(&self, guild: GuildId) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query(
            "SELECT reason, content FROM memory_versions WHERE guild_id = ? ORDER BY id DESC",
        )
        .bind(guild as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| Ok((r.try_get("reason")?, r.try_get("content")?)))
            .collect()
    }

    // --- worker_runs ---
    pub async fn abort_pending_runs(&self, guild: GuildId) -> Result<u64> {
        let r = sqlx::query(
            "UPDATE worker_runs SET status = 'aborted', finished_at = ? WHERE guild_id = ? AND status IN ('queued', 'running')",
        )
        .bind(now_ms())
        .bind(guild as i64)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }
    pub async fn active_runs(&self, guild: GuildId) -> Result<Vec<WorkerRunRow>> {
        self.worker_runs(guild, &[RunStatus::Queued, RunStatus::Running])
            .await
    }
    /// `statuses` のいずれかに当たる run を登録順で。`noth status` が状態ごとに一覧するため
    pub async fn worker_runs(
        &self,
        guild: GuildId,
        statuses: &[RunStatus],
    ) -> Result<Vec<WorkerRunRow>> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; statuses.len()].join(", ");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {RUN_COLUMNS} FROM worker_runs WHERE guild_id = ? AND status IN ({marks}) ORDER BY rowid ASC"
        )))
        .bind(guild as i64);
        for s in statuses {
            q = q.bind(s.as_str());
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.iter().map(run_row).collect()
    }
    pub async fn worker_run(&self, run_id: &str) -> Result<Option<WorkerRunRow>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {RUN_COLUMNS} FROM worker_runs WHERE run_id = ?"
        )))
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(run_row).transpose()
    }
    pub async fn insert_worker_run(&self, guild: GuildId, run: &NewWorkerRun) -> Result<()> {
        insert_run(&self.pool, guild, run).await
    }
    /// 再起動で aborted にされた run を後から動かさないよう、queued のときだけ進める。進めたら true
    pub async fn start_run(&self, run_id: &str) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE worker_runs SET status = 'running', started_at = ? WHERE run_id = ? AND status = 'queued'",
        )
        .bind(now_ms())
        .bind(run_id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }
    /// `status` は Finished か Failed。`conclusion` は結論メモか失敗理由
    pub async fn finish_run(
        &self,
        run_id: &str,
        status: RunStatus,
        conclusion: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE worker_runs SET status = ?, conclusion = ?, finished_at = ? WHERE run_id = ?",
        )
        .bind(status.as_str())
        .bind(conclusion)
        .bind(now_ms())
        .bind(run_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn set_delivery_style(&self, run_id: &str, style: &str) -> Result<()> {
        sqlx::query("UPDATE worker_runs SET delivery_style = ? WHERE run_id = ?")
            .bind(style)
            .bind(run_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // --- costs ---
    pub async fn record_cost(&self, guild: GuildId, r: &CostRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO costs (guild_id, created_at, source, model, purpose, run_id, input_tokens, cache_read_tokens, cache_write_tokens, output_tokens, unit, quantity, amount_usd, billing)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(guild as i64)
        .bind(now_ms())
        .bind(&r.source)
        .bind(&r.model)
        .bind(r.purpose.as_str())
        .bind(&r.run_id)
        .bind(r.input_tokens as i64)
        .bind(r.cache_read_tokens as i64)
        .bind(r.cache_write_tokens as i64)
        .bind(r.output_tokens as i64)
        .bind(r.unit.as_ref().map(|(u, _)| u.as_str()))
        .bind(r.unit.as_ref().map(|(_, q)| *q as i64))
        .bind(r.amount_usd)
        .bind(r.billing.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn upsert_rate_limit(&self, guild: GuildId, r: &RateLimitSnapshot) -> Result<()> {
        sqlx::query(
            "INSERT INTO rate_limits (guild_id, source, bucket, \"limit\", remaining, reset_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(guild_id, source, bucket) DO UPDATE SET \"limit\" = excluded.\"limit\", remaining = excluded.remaining, reset_at = excluded.reset_at, updated_at = excluded.updated_at",
        )
        .bind(guild as i64)
        .bind(&r.source)
        .bind(&r.bucket)
        .bind(r.limit as i64)
        .bind(r.remaining as i64)
        .bind(r.reset_at)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    /// `since_ms` 以降の記録を集計する。キーの昇順、Day だけは新しい日付から
    pub async fn cost_summary(
        &self,
        guild: GuildId,
        since_ms: i64,
        group: CostGroup,
    ) -> Result<Vec<CostSummaryRow>> {
        let rows = sqlx::query(
            "SELECT created_at, source, model, purpose, run_id, input_tokens, cache_read_tokens, cache_write_tokens, output_tokens, unit, quantity, amount_usd, billing
             FROM costs WHERE guild_id = ? AND created_at >= ? ORDER BY id ASC",
        )
        .bind(guild as i64)
        .bind(since_ms)
        .fetch_all(&self.pool)
        .await?;
        let mut groups: std::collections::BTreeMap<
            String,
            (CostSummaryRow, std::collections::BTreeMap<String, u64>),
        > = std::collections::BTreeMap::new();
        for r in &rows {
            // Day はローカル日付で切るため SQL ではなくここでキーを作る
            let key = match group {
                CostGroup::Source => r.try_get::<String, _>("source")?,
                CostGroup::Model => r.try_get::<String, _>("model")?,
                CostGroup::Purpose => r.try_get::<String, _>("purpose")?,
                CostGroup::Day => ts(r.try_get("created_at")?)
                    .to_zoned(jiff::tz::TimeZone::system())
                    .strftime("%Y-%m-%d")
                    .to_string(),
                CostGroup::Run => r
                    .try_get::<Option<String>, _>("run_id")?
                    .unwrap_or_else(|| "-".into()),
            };
            let (row, units) = groups.entry(key.clone()).or_insert_with(|| {
                (
                    CostSummaryRow {
                        key,
                        ..Default::default()
                    },
                    Default::default(),
                )
            });
            row.calls += 1;
            row.input_tokens += r.try_get::<i64, _>("input_tokens")? as u64;
            row.cache_read_tokens += r.try_get::<i64, _>("cache_read_tokens")? as u64;
            row.cache_write_tokens += r.try_get::<i64, _>("cache_write_tokens")? as u64;
            row.output_tokens += r.try_get::<i64, _>("output_tokens")? as u64;
            if let Some(unit) = r.try_get::<Option<String>, _>("unit")? {
                let quantity = r.try_get::<Option<i64>, _>("quantity")?.unwrap_or(0) as u64;
                *units.entry(unit).or_default() += quantity;
            }
            match r.try_get::<Option<f64>, _>("amount_usd")? {
                Some(amount) => row.amount_usd += amount,
                None if r.try_get::<String, _>("billing")? == Billing::Metered.as_str() => {
                    row.unpriced_calls += 1
                }
                None => {}
            }
        }
        let mut out: Vec<CostSummaryRow> = groups
            .into_values()
            .map(|(mut row, units)| {
                row.units = units.into_iter().collect();
                row
            })
            .collect();
        if group == CostGroup::Day {
            out.reverse();
        }
        Ok(out)
    }
    /// 区分ごとの最新の値と、その更新時刻(UNIX ミリ秒)
    pub async fn rate_limits(&self, guild: GuildId) -> Result<Vec<(RateLimitSnapshot, i64)>> {
        let rows = sqlx::query(
            "SELECT source, bucket, \"limit\", remaining, reset_at, updated_at FROM rate_limits WHERE guild_id = ? ORDER BY source, bucket",
        )
        .bind(guild as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok((
                    RateLimitSnapshot {
                        source: r.try_get("source")?,
                        bucket: r.try_get("bucket")?,
                        limit: r.try_get::<i64, _>("limit")? as u64,
                        remaining: r.try_get::<i64, _>("remaining")? as u64,
                        reset_at: r.try_get("reset_at")?,
                    },
                    r.try_get("updated_at")?,
                ))
            })
            .collect()
    }

    // --- heartbeat ---
    pub async fn upsert_heartbeat(&self, guild: GuildId, state: &str, info: &Value) -> Result<()> {
        sqlx::query(
            "INSERT INTO heartbeat (guild_id, updated_at, state, info) VALUES (?, ?, ?, ?)
             ON CONFLICT(guild_id) DO UPDATE SET updated_at = excluded.updated_at, state = excluded.state, info = excluded.info",
        )
        .bind(guild as i64)
        .bind(now_ms())
        .bind(state)
        .bind(serde_json::to_string(info)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    /// `noth status` は guild を知らずに読むので guild を問わず最新のものを返す
    pub async fn read_heartbeat(&self) -> Result<Option<HeartbeatRow>> {
        let row = sqlx::query("SELECT guild_id, updated_at, state, info FROM heartbeat ORDER BY updated_at DESC LIMIT 1")
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| -> Result<HeartbeatRow> {
            Ok(HeartbeatRow {
                guild_id: r.try_get::<i64, _>("guild_id")? as u64,
                updated_at: ts(r.try_get("updated_at")?),
                state: r.try_get("state")?,
                info: serde_json::from_str(&r.try_get::<String, _>("info")?)?,
            })
        })
        .transpose()
    }
}

async fn insert_turn<'e>(
    exec: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    guild: GuildId,
    turn: &Turn,
) -> Result<i64> {
    let r = sqlx::query("INSERT INTO active_log (guild_id, role, kind, text, chunk_id, created_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(guild as i64)
        .bind(role_str(turn.role))
        .bind(turn.kind.as_str())
        .bind(&turn.text)
        .bind(turn.chunk_id)
        .bind(now_ms())
        .execute(exec)
        .await?;
    Ok(r.last_insert_rowid())
}

async fn insert_run<'e>(
    exec: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    guild: GuildId,
    run: &NewWorkerRun,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO worker_runs (run_id, guild_id, goal, task_category, trigger_message_id, channel_id, status, chunk_id, fork_message_id)
         VALUES (?, ?, ?, ?, ?, ?, 'queued', ?, ?)",
    )
    .bind(&run.run_id)
    .bind(guild as i64)
    .bind(&run.goal)
    .bind(category_str(run.task_category))
    .bind(run.trigger_message_id.map(|id| id as i64))
    .bind(run.channel_id as i64)
    .bind(run.chunk_id)
    .bind(run.fork_message_id)
    .execute(exec)
    .await?;
    Ok(())
}

async fn promote_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ids: &[i64],
    turn_id: i64,
) -> Result<()> {
    for id in ids {
        sqlx::query("UPDATE messages SET promoted_turn_id = ? WHERE id = ?")
            .bind(turn_id)
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn write_usage<'e>(
    exec: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    turn_id: i64,
    usage: &Usage,
) -> Result<()> {
    sqlx::query(
        "UPDATE active_log SET input_tokens = ?, cached_tokens = ?, output_tokens = ? WHERE id = ?",
    )
    .bind(usage.input_tokens as i64)
    .bind(usage.cached_tokens as i64)
    .bind(usage.output_tokens as i64)
    .bind(turn_id)
    .execute(exec)
    .await?;
    Ok(())
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    Ok(())
}

/// -wal / -shm は接続後に作られるので、既存の DB ファイルが 0644 だった場合も含めてここで締める
#[cfg(unix)]
fn restrict_sidecar_files(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let file = std::path::PathBuf::from(name);
        if file.exists() {
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const G: GuildId = 1;
    const C: ChannelId = 10;

    fn incoming(message_id: u64, is_self: bool, content: &str) -> IncomingMessage {
        IncomingMessage {
            guild_id: G,
            channel_id: C,
            channel_name: "general".into(),
            message_id,
            user_id: if is_self { 99 } else { 5 },
            author_name: "alice".into(),
            content: content.into(),
            timestamp: Timestamp::from_millisecond(1_700_000_000_000 + message_id as i64).unwrap(),
            is_self,
            mentions_self: false,
            reply_to_self: false,
            reply_to: None,
        }
    }

    async fn setup_chunk(s: &Store) -> (i64, i64) {
        let d = s
            .insert_decision(G, "test", "m", &json!({}), 1)
            .await
            .unwrap();
        let c = s
            .create_chunk(G, C, Timestamp::from_millisecond(1000).unwrap(), "")
            .await
            .unwrap();
        (d, c)
    }

    #[tokio::test]
    async fn messages_roundtrip_and_queries() {
        let s = Store::open_memory().await.unwrap();
        let a = s.insert_message(&incoming(1, false, "a")).await.unwrap();
        let b = s.insert_message(&incoming(2, false, "b")).await.unwrap();
        let me = s.insert_message(&incoming(3, true, "self")).await.unwrap();
        let c = s.insert_message(&incoming(4, false, "c")).await.unwrap();

        let recent = s.recent_messages(G, i64::MAX, 3).await.unwrap();
        assert_eq!(
            recent.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![b, me, c]
        );
        assert_eq!(recent[0].message_id, 2);
        assert_eq!(recent[0].created_at.as_millisecond(), 1_700_000_000_002);
        assert!(recent[1].is_self);
        assert_eq!(recent[0].is_noise, None);
        assert!(s.recent_messages(2, i64::MAX, 10).await.unwrap().is_empty());
        // before_id 以降(自身を含む)は返さない
        let before_c = s.recent_messages(G, c, 10).await.unwrap();
        assert_eq!(
            before_c.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![a, b, me]
        );

        let after = s.messages_after(G, b, 10).await.unwrap();
        assert_eq!(after.iter().map(|m| m.id).collect::<Vec<_>>(), vec![me, c]);

        let unjudged = s.unjudged_messages(G).await.unwrap();
        assert_eq!(
            unjudged.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![a, b, c]
        );

        let (d, ch) = setup_chunk(&s).await;
        s.mark_judged(&[a], d, ch, false).await.unwrap();
        s.mark_judged(&[b], d, ch, true).await.unwrap();
        let unjudged = s.unjudged_messages(G).await.unwrap();
        assert_eq!(unjudged.iter().map(|m| m.id).collect::<Vec<_>>(), vec![c]);

        // ノイズ・未判定・自分の発言はバッファに入らない
        let buf = s.buffered_messages(G).await.unwrap();
        assert_eq!(buf.iter().map(|m| m.id).collect::<Vec<_>>(), vec![a]);
        assert_eq!(buf[0].chunk_id, Some(ch));
        assert_eq!(buf[0].is_noise, Some(false));

        s.mark_promoted(&[a], 7).await.unwrap();
        assert!(s.buffered_messages(G).await.unwrap().is_empty());

        let in_chunk = s.chunk_messages(ch).await.unwrap();
        assert_eq!(
            in_chunk.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![a, b]
        );
        assert_eq!(in_chunk[0].promoted_turn_id, Some(7));
    }

    #[tokio::test]
    async fn chunks() {
        let s = Store::open_memory().await.unwrap();
        assert!(s.current_chunk(G, C).await.unwrap().is_none());
        let t = |ms| Timestamp::from_millisecond(ms).unwrap();
        let c1 = s.create_chunk(G, C, t(1000), "first").await.unwrap();
        let c2 = s.create_chunk(G, C, t(2000), "second").await.unwrap();
        let c3 = s.create_chunk(G, 11, t(3000), "other").await.unwrap();

        assert_eq!(s.current_chunk(G, C).await.unwrap().unwrap().id, c2);
        assert_eq!(s.chunk(c1).await.unwrap().unwrap().summary, "first");
        assert!(s.chunk(999).await.unwrap().is_none());

        let ids = |v: Vec<ChunkRow>| v.into_iter().map(|c| c.id).collect::<Vec<_>>();
        assert_eq!(
            ids(s.candidate_chunks(G, 10, &[]).await.unwrap()),
            vec![c3, c2, c1]
        );
        assert_eq!(
            ids(s.candidate_chunks(G, 10, &[c3]).await.unwrap()),
            vec![c2, c1]
        );
        assert_eq!(
            ids(s.candidate_chunks(G, 1, &[c3]).await.unwrap()),
            vec![c2]
        );

        // touch で ended_at が進むと並びも変わる
        s.touch_chunk(c1, t(9000), Some("more")).await.unwrap();
        let got = s.chunk(c1).await.unwrap().unwrap();
        assert_eq!(got.summary, "first / more");
        assert_eq!(got.ended_at.as_millisecond(), 9000);
        assert_eq!(
            ids(s.candidate_chunks(G, 10, &[]).await.unwrap()),
            vec![c1, c3, c2]
        );

        s.touch_chunk(c1, t(9001), None).await.unwrap();
        assert_eq!(s.chunk(c1).await.unwrap().unwrap().summary, "first / more");

        let long = "あ".repeat(190);
        let c4 = s.create_chunk(G, 12, t(1), &long).await.unwrap();
        s.touch_chunk(c4, t(2), Some(&"い".repeat(50)))
            .await
            .unwrap();
        assert_eq!(
            s.chunk(c4).await.unwrap().unwrap().summary.chars().count(),
            200
        );
        s.touch_chunk(c4, t(3), Some("again")).await.unwrap();
        assert_eq!(
            s.chunk(c4).await.unwrap().unwrap().summary.chars().count(),
            200
        );
    }

    #[tokio::test]
    async fn decisions() {
        let s = Store::open_memory().await.unwrap();
        assert!(s.latest_decision(G).await.unwrap().is_none());
        s.insert_decision(G, "systemone", "m", &json!({"a": 1}), 10)
            .await
            .unwrap();
        let id = s
            .insert_decision(G, "mention", "m", &json!({"b": [1, 2]}), 20)
            .await
            .unwrap();
        let d = s.latest_decision(G).await.unwrap().unwrap();
        assert_eq!(d.id, id);
        assert_eq!(d.kind, "mention");
        assert_eq!(d.answers, json!({"b": [1, 2]}));
        assert_eq!(d.latency_ms, 20);
    }

    #[tokio::test]
    async fn active_log_and_usage() {
        let s = Store::open_memory().await.unwrap();
        let turn = |text: &str, role, kind| Turn {
            id: None,
            role,
            kind,
            text: text.into(),
            chunk_id: Some(3),
        };
        let t1 = s
            .append_turn(G, &turn("hi", Role::User, TurnKind::Trigger))
            .await
            .unwrap();
        let t2 = s
            .append_turn(G, &turn("yo", Role::Assistant, TurnKind::Response))
            .await
            .unwrap();
        let t3 = s
            .append_turn(G, &turn("more", Role::User, TurnKind::Buffer))
            .await
            .unwrap();

        assert!(s.last_usage(G).await.unwrap().is_none());
        let u = Usage {
            input_tokens: 100,
            cached_tokens: 40,
            output_tokens: 7,
        };
        s.set_turn_usage(t2, &u).await.unwrap();
        // usage 未設定の新しいターンがあっても、入っている最新のものを返す
        assert_eq!(s.last_usage(G).await.unwrap(), Some(u));
        assert!(s.last_usage(2).await.unwrap().is_none());

        let turns = s.active_turns(G).await.unwrap();
        assert_eq!(turns.len(), 3);
        assert_eq!(
            turns[1],
            Turn {
                id: Some(t2),
                role: Role::Assistant,
                kind: TurnKind::Response,
                text: "yo".into(),
                chunk_id: Some(3)
            }
        );

        s.remove_turns(&[t1, t3]).await.unwrap();
        let turns = s.active_turns(G).await.unwrap();
        assert_eq!(
            turns.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![Some(t2)]
        );
    }

    #[tokio::test]
    async fn digests() {
        let s = Store::open_memory().await.unwrap();
        assert!(s.latest_digest(G).await.unwrap().is_none());
        s.insert_digest(G, "old").await.unwrap();
        s.insert_digest(G, "new").await.unwrap();
        assert_eq!(s.latest_digest(G).await.unwrap().as_deref(), Some("new"));
    }

    fn new_run(run_id: &str) -> NewWorkerRun {
        NewWorkerRun {
            run_id: run_id.into(),
            goal: format!("goal {run_id}"),
            task_category: TaskCategory::WebResearch,
            channel_id: C,
            chunk_id: 3,
            trigger_message_id: Some(42),
            fork_message_id: 7,
        }
    }

    #[tokio::test]
    async fn worker_runs() {
        let s = Store::open_memory().await.unwrap();
        for id in ["r1", "r2", "r3", "r4"] {
            s.insert_worker_run(G, &new_run(id)).await.unwrap();
        }
        assert!(s.start_run("r2").await.unwrap());
        assert!(s.start_run("r3").await.unwrap());
        s.finish_run("r3", RunStatus::Finished, "memo")
            .await
            .unwrap();
        assert!(s.start_run("r4").await.unwrap());
        s.finish_run("r4", RunStatus::Failed, "timeout")
            .await
            .unwrap();
        // running からはもう一度 start できない
        assert!(!s.start_run("r2").await.unwrap());

        let r3 = s.worker_run("r3").await.unwrap().unwrap();
        assert_eq!(r3.status, RunStatus::Finished);
        assert_eq!(r3.conclusion.as_deref(), Some("memo"));
        assert_eq!(r3.task_category, TaskCategory::WebResearch);
        assert_eq!(r3.trigger_message_id, Some(42));
        assert_eq!(r3.fork_message_id, 7);
        assert_eq!(r3.chunk_id, Some(3));
        assert!(r3.started_at.is_some() && r3.finished_at.is_some());
        s.set_delivery_style("r3", "abort").await.unwrap();
        assert_eq!(
            s.worker_run("r3")
                .await
                .unwrap()
                .unwrap()
                .delivery_style
                .as_deref(),
            Some("abort")
        );
        assert!(s.worker_run("nope").await.unwrap().is_none());

        let ids = |v: Vec<WorkerRunRow>| v.into_iter().map(|r| r.run_id).collect::<Vec<_>>();
        assert_eq!(ids(s.active_runs(G).await.unwrap()), vec!["r1", "r2"]);
        assert!(s.active_runs(G).await.unwrap()[0].started_at.is_none());
        assert_eq!(
            ids(s
                .worker_runs(G, &[RunStatus::Finished, RunStatus::Failed])
                .await
                .unwrap()),
            vec!["r3", "r4"]
        );
        assert!(s.worker_runs(G, &[]).await.unwrap().is_empty());

        assert_eq!(s.abort_pending_runs(G).await.unwrap(), 2);
        assert!(s.active_runs(G).await.unwrap().is_empty());
        assert_eq!(s.abort_pending_runs(G).await.unwrap(), 0);
        // aborted にされた queued の run は後から running にならない
        assert!(!s.start_run("r1").await.unwrap());
    }

    #[tokio::test]
    async fn record_task_and_digest_replacement() {
        let s = Store::open_memory().await.unwrap();
        let a = s.insert_message(&incoming(1, false, "a")).await.unwrap();
        s.insert_message(&incoming(2, true, "self")).await.unwrap();
        assert_eq!(s.max_message_id(G).await.unwrap(), a + 1);
        assert_eq!(s.max_message_id(2).await.unwrap(), 0);
        assert_eq!(s.last_human_message(G).await.unwrap().unwrap().0, a);
        assert_eq!(s.message(a).await.unwrap().unwrap().message_id, 1);

        let turn = |text: &str, role, kind| Turn {
            id: None,
            role,
            kind,
            text: text.into(),
            chunk_id: Some(3),
        };
        s.record_task(
            G,
            &turn("t", Role::User, TurnKind::Trigger),
            &turn("ack\n[Goal]\ngoal", Role::Assistant, TurnKind::Response),
            &[a],
            &Usage::default(),
            &new_run("r1"),
        )
        .await
        .unwrap();
        let turns = s.active_turns(G).await.unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(
            s.message(a).await.unwrap().unwrap().promoted_turn_id,
            turns[0].id
        );
        assert_eq!(
            s.worker_run("r1").await.unwrap().unwrap().status,
            RunStatus::Queued
        );

        let c = s
            .create_chunk(G, C, Timestamp::from_millisecond(1).unwrap(), "")
            .await
            .unwrap();
        s.replace_with_digest(G, "digest", &[turns[0].id.unwrap()], &[c])
            .await
            .unwrap();
        assert_eq!(s.latest_digest(G).await.unwrap().as_deref(), Some("digest"));
        assert_eq!(s.active_turns(G).await.unwrap().len(), 1);
        assert!(s.chunk(c).await.unwrap().unwrap().compacted);
    }

    fn cost(source: &str, model: &str, purpose: Purpose, amount: Option<f64>) -> CostRecord {
        CostRecord {
            source: source.into(),
            model: model.into(),
            purpose,
            run_id: None,
            input_tokens: 10,
            cache_read_tokens: 5,
            cache_write_tokens: 1,
            output_tokens: 2,
            unit: None,
            amount_usd: amount,
            billing: Billing::Metered,
        }
    }

    #[tokio::test]
    async fn cost_summary_groups_and_counts_unpriced() {
        let s = Store::open_memory().await.unwrap();
        s.record_cost(
            G,
            &cost("provider-anthropic", "opus", Purpose::Main, Some(0.5)),
        )
        .await
        .unwrap();
        s.record_cost(G, &cost("provider-anthropic", "opus", Purpose::Join, None))
            .await
            .unwrap();
        let mut agent = cost("tool-web-agent", "agent", Purpose::Worker, Some(0.25));
        agent.run_id = Some("r1".into());
        agent.unit = Some(("step".into(), 3));
        s.record_cost(G, &agent).await.unwrap();
        agent.unit = Some(("step".into(), 4));
        agent.amount_usd = None;
        s.record_cost(G, &agent).await.unwrap();
        let mut free = cost("tool-web-search", "search", Purpose::Worker, None);
        free.billing = Billing::Free;
        free.unit = Some(("request".into(), 1));
        s.record_cost(G, &free).await.unwrap();
        // 他の guild は混ぜない
        s.record_cost(
            2,
            &cost("provider-anthropic", "opus", Purpose::Main, Some(9.0)),
        )
        .await
        .unwrap();

        let by_source = s.cost_summary(G, 0, CostGroup::Source).await.unwrap();
        let keys: Vec<&str> = by_source.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["provider-anthropic", "tool-web-agent", "tool-web-search"]
        );
        let anthropic = &by_source[0];
        assert_eq!(anthropic.calls, 2);
        assert_eq!(anthropic.input_tokens, 20);
        assert_eq!(anthropic.cache_read_tokens, 10);
        assert_eq!(anthropic.cache_write_tokens, 2);
        assert_eq!(anthropic.output_tokens, 4);
        assert_eq!(anthropic.amount_usd, 0.5);
        assert_eq!(anthropic.unpriced_calls, 1);
        assert_eq!(by_source[1].units, vec![("step".to_string(), 7)]);
        assert_eq!(by_source[1].unpriced_calls, 1);
        // 無料の呼び出しは単価不明に数えない
        assert_eq!(by_source[2].unpriced_calls, 0);

        let by_purpose = s.cost_summary(G, 0, CostGroup::Purpose).await.unwrap();
        let keys: Vec<&str> = by_purpose.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["join", "main", "worker"]);
        let by_run = s.cost_summary(G, 0, CostGroup::Run).await.unwrap();
        let keys: Vec<&str> = by_run.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["-", "r1"]);
        let by_model = s.cost_summary(G, 0, CostGroup::Model).await.unwrap();
        assert_eq!(by_model.len(), 3);

        let today = Timestamp::now()
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%d")
            .to_string();
        let by_day = s.cost_summary(G, 0, CostGroup::Day).await.unwrap();
        assert_eq!(by_day.len(), 1);
        assert_eq!(by_day[0].key, today);
        assert_eq!(by_day[0].calls, 5);

        assert!(
            s.cost_summary(G, now_ms() + 60_000, CostGroup::Source)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn cost_summary_day_is_newest_first() {
        let s = Store::open_memory().await.unwrap();
        for (ms, model) in [(1_700_000_000_000_i64, "old"), (1_800_000_000_000, "new")] {
            sqlx::query("INSERT INTO costs (guild_id, created_at, source, model, purpose, billing) VALUES (?, ?, 's', ?, 'main', 'metered')")
                .bind(G as i64)
                .bind(ms)
                .bind(model)
                .execute(&s.pool)
                .await
                .unwrap();
        }
        let by_day = s.cost_summary(G, 0, CostGroup::Day).await.unwrap();
        assert_eq!(by_day.len(), 2);
        assert!(by_day[0].key > by_day[1].key);
        assert_eq!(by_day[0].unpriced_calls, 1);
    }

    #[tokio::test]
    async fn rate_limits_keep_latest_per_bucket() {
        let s = Store::open_memory().await.unwrap();
        let snap = |bucket: &str, remaining| RateLimitSnapshot {
            source: "tool-web-search".into(),
            bucket: bucket.into(),
            limit: 100,
            remaining,
            reset_at: Some(1_800_000_000),
        };
        s.upsert_rate_limit(G, &snap("search", 90)).await.unwrap();
        s.upsert_rate_limit(G, &snap("search", 80)).await.unwrap();
        s.upsert_rate_limit(G, &snap("fetch", 50)).await.unwrap();
        let got = s.rate_limits(G).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, snap("fetch", 50));
        assert_eq!(got[1].0, snap("search", 80));
        assert!(got[1].1 > 0);
        assert!(s.rate_limits(2).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn heartbeat() {
        let s = Store::open_memory().await.unwrap();
        assert!(s.read_heartbeat().await.unwrap().is_none());
        s.upsert_heartbeat(G, "connected", &json!({"modules": ["a"]}))
            .await
            .unwrap();
        s.upsert_heartbeat(G, "disconnected", &json!({}))
            .await
            .unwrap();
        let h = s.read_heartbeat().await.unwrap().unwrap();
        assert_eq!(h.guild_id, G);
        assert_eq!(h.state, "disconnected");
        assert_eq!(h.info, json!({}));
    }

    #[tokio::test]
    async fn open_creates_parent_dirs() {
        let dir = std::env::temp_dir().join(format!("noth-store-test-{}", std::process::id()));
        let path = dir.join("nested").join("noth.sqlite");
        let s = Store::open(&path).await.unwrap();
        s.insert_digest(G, "x").await.unwrap();
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["", "-wal", "-shm"] {
                let mut name = path.as_os_str().to_os_string();
                name.push(suffix);
                if let Ok(meta) = std::fs::metadata(&name) {
                    assert_eq!(meta.permissions().mode() & 0o777, 0o600, "{name:?}");
                }
            }
        }
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn memory_versions_newest_first() {
        let s = Store::open_memory().await.unwrap();
        s.insert_memory_version(G, "memo", "v1").await.unwrap();
        s.insert_memory_version(G, "dreaming", "v2").await.unwrap();
        let got = s.memory_versions(G).await.unwrap();
        assert_eq!(
            got,
            vec![
                ("dreaming".to_string(), "v2".to_string()),
                ("memo".to_string(), "v1".to_string())
            ]
        );
        assert!(s.memory_versions(G + 1).await.unwrap().is_empty());
    }

    fn query() -> HistoryQuery {
        HistoryQuery {
            guild_id: G,
            channel_id: None,
            channel_name: None,
            text: None,
            author: None,
            since: None,
            until: None,
            limit: 10,
        }
    }

    async fn search_ids(s: &Store, q: &HistoryQuery) -> Vec<u64> {
        s.search_messages(q)
            .await
            .unwrap()
            .iter()
            .map(|m| m.message_id)
            .collect()
    }

    #[tokio::test]
    async fn search_messages_filters_and_orders_newest_first() {
        let s = Store::open_memory().await.unwrap();
        s.insert_message(&incoming(1, false, "Rust の話"))
            .await
            .unwrap();
        let mut other = incoming(2, false, "晩ごはん");
        other.channel_id = 11;
        other.channel_name = "雑談".into();
        other.author_name = "Bob".into();
        s.insert_message(&other).await.unwrap();
        s.insert_message(&incoming(3, true, "rust は速い"))
            .await
            .unwrap();
        let mut foreign = incoming(4, false, "rust");
        foreign.guild_id = G + 1;
        s.insert_message(&foreign).await.unwrap();

        assert_eq!(search_ids(&s, &query()).await, vec![3, 2, 1]);
        let text = HistoryQuery {
            text: Some("RUST".into()),
            ..query()
        };
        assert_eq!(search_ids(&s, &text).await, vec![3, 1]);
        let by_id = HistoryQuery {
            channel_id: Some(11),
            ..query()
        };
        assert_eq!(search_ids(&s, &by_id).await, vec![2]);
        let by_name = HistoryQuery {
            channel_name: Some("GENERAL".into()),
            ..query()
        };
        assert_eq!(search_ids(&s, &by_name).await, vec![3, 1]);
        let author = HistoryQuery {
            author: Some("bo".into()),
            ..query()
        };
        assert_eq!(search_ids(&s, &author).await, vec![2]);
        let ts = |id: i64| Timestamp::from_millisecond(1_700_000_000_000 + id).unwrap();
        let range = HistoryQuery {
            since: Some(ts(2)),
            until: Some(ts(3)),
            ..query()
        };
        assert_eq!(search_ids(&s, &range).await, vec![2]);
        let limited = HistoryQuery {
            limit: 1,
            ..query()
        };
        assert_eq!(search_ids(&s, &limited).await, vec![3]);
    }

    #[tokio::test]
    async fn search_messages_folds_only_ascii_case() {
        let s = Store::open_memory().await.unwrap();
        s.insert_message(&incoming(1, false, "École の話"))
            .await
            .unwrap();
        let ids = |text: &str| {
            let q = HistoryQuery {
                text: Some(text.into()),
                ..query()
            };
            let s = s.clone();
            async move { search_ids(&s, &q).await }
        };
        assert_eq!(ids("École").await, vec![1]);
        assert_eq!(ids("cole").await, vec![1]);
        assert_eq!(ids("COLE").await, vec![1]);
        assert!(ids("école").await.is_empty());
    }

    #[tokio::test]
    async fn reply_to_round_trips() {
        let s = Store::open_memory().await.unwrap();
        let mut m = incoming(1, false, "返信");
        m.reply_to = Some(ReplyTo {
            message_id: 42,
            author_name: "bob".into(),
            is_self: true,
            excerpt: "元".into(),
        });
        s.insert_message(&m).await.unwrap();
        s.insert_message(&incoming(2, false, "通常")).await.unwrap();
        let rows = s.unjudged_messages(G).await.unwrap();
        assert_eq!(rows[0].reply_to, m.reply_to);
        assert_eq!(rows[1].reply_to, None);
    }
}
