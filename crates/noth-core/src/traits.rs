use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;

use crate::Result;
use crate::bus::BusHandle;
use crate::types::*;

/// heartbeat と `noth status` に出す接続状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Disconnected,
}

impl ConnectionState {
    /// heartbeat テーブルに保存する値
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
        }
    }
}

#[async_trait]
pub trait Channel: Send + Sync {
    /// 発言を記録するテーブルのキー。MVP は1 guild
    fn guild_id(&self) -> GuildId;
    /// 接続し、受信した発言を Event Bus に流す。allowed_users 以外と Bot の発言はここで捨てる
    async fn run(&self, bus: BusHandle) -> Result<()>;
    /// 2,000 文字を超えれば分割し、最後の MessageId を返す。allowed_mentions は常に空
    async fn send(
        &self,
        channel_id: ChannelId,
        text: &str,
        reply_to: Option<MessageId>,
    ) -> Result<MessageId>;
    async fn typing(&self, channel_id: ChannelId) -> Result<()>;
    /// Layer 1 に固定するメンバー情報。起動時に1回だけ呼ぶ
    async fn members(&self) -> Result<Vec<Member>>;
    /// `noth doctor` の疎通確認。結果の説明を返す
    async fn health_check(&self) -> Result<String>;
    /// 現在の接続状態。heartbeat が周期ごとに読む。接続の概念がない実装は常に Connected でよい
    fn connection_state(&self) -> ConnectionState {
        ConnectionState::Connected
    }
}

#[derive(Debug, Clone)]
pub enum Question {
    /// yes / no。回答は yes である確率
    Noul { instructions: String },
    /// 選択肢 (名前, 説明) から1つ
    Choice {
        instructions: String,
        criteria: Vec<(String, String)>,
    },
    /// 段階別の評価基準。レベルは 2〜10
    Score {
        instructions: String,
        levels: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Noul {
        p: f32,
    },
    Choice {
        choice: String,
        probabilities: HashMap<String, f32>,
        confidence: f32,
    },
    Score {
        score: f32,
        probabilities: HashMap<u8, f32>,
        confidence: f32,
    },
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub answers: HashMap<String, Answer>,
    pub latency: Duration,
}

#[async_trait]
pub trait Router: Send + Sync {
    async fn decide(
        &self,
        state: &RouterState,
        questions: &[(String, Question)],
    ) -> Result<Decision>;
    /// 1リクエストに載せられる質問数。候補チャンクの数を決めるのに使う
    fn max_questions(&self) -> usize;
    /// `decisions.model` に記録する識別子(例: `jev/jev-latest`)。モデルごとの判定傾向を比べるため
    fn model_label(&self) -> String;
    async fn health_check(&self) -> Result<String>;
}

#[derive(Debug, Clone, Default)]
pub struct LayeredPrompt {
    pub harness_rules: String,
    pub profile: String,
    pub episodic_digest: String,
    pub active_log: Vec<Turn>,
    pub ephemeral_tail: Vec<Turn>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// 429 / 5xx は内部で retry_max 回まで再試行し、尽きたら Err を返す
    async fn complete(
        &self,
        prompt: &LayeredPrompt,
        params: &CompletionParams,
    ) -> Result<Completion>;
    /// MVP では呼ばない
    async fn keep_alive(&self, _prefix: &LayeredPrompt) -> Result<()> {
        Ok(())
    }
    async fn health_check(&self) -> Result<String>;
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn call(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;
}

#[async_trait]
pub trait Compactor: Send + Sync {
    async fn digest(&self, turns: &[Turn]) -> Result<String>;
}
