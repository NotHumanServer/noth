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
    /// 投稿済みの発言を書き換える。Worker の進捗を Ack に追記するのに使う。
    /// 編集できない Channel は既定のまま Err を返し、Core は進捗表示をあきらめる
    async fn edit(
        &self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _text: &str,
    ) -> Result<()> {
        Err(crate::Error::Other(
            "この Channel は発言の編集に対応していません".into(),
        ))
    }
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
    /// worker 有効時は本線にも Worker と同一の定義を渡す(caching.md §2)
    pub tools: Vec<ToolSpec>,
    /// Worker の tool 往復。ephemeral_tail の後ろに置く。本線では常に空
    pub tool_exchange: Vec<ExchangeItem>,
    /// Worker の呼び出し。末尾のキャッシュ境界を Layer 3 末尾ではなくコンテキスト末尾に置く(caching.md §3)
    pub cache_to_end: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    /// tool_calls があるとき、`ExchangeItem::Assistant::raw` に入れて次の呼び出しで返す provider 固有の content
    pub raw_assistant: Option<serde_json::Value>,
    /// provider が自分の単価表で計算した費用。purpose と run_id は Core の計測が上書きする
    pub cost: Option<CostRecord>,
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
    /// 出力の切り詰めは呼び出し側(Worker)が行う。tool 自身の失敗は Err ではなく is_error の出力で返してよい
    async fn call(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;
    /// `noth doctor` の疎通確認。外部 API を使わない tool は既定のままでよい
    async fn health_check(&self) -> Result<String> {
        Ok("ok".into())
    }
    /// `noth cost` に出す課金先アカウントの状態(残高など)。外部の課金が無い tool は None
    async fn account_status(&self) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Worker が tool を呼ぶ直前に Core へ知らせる。Discord の Ack を編集して進み具合を見せるため
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerProgress {
    pub tool: String,
    /// 利用者に見せる短い説明(検索語、URL など)。tool の出力は含めない
    pub summary: String,
}

/// Fork 時に Core が渡す。`base` は本線の Layer 0〜3 と tool 定義をそのまま複製したもの
#[derive(Debug, Clone)]
pub struct WorkerTask {
    pub run_id: String,
    pub goal: String,
    pub task_category: TaskCategory,
    pub base: LayeredPrompt,
    pub work_dir: std::path::PathBuf,
    /// None なら進捗を送らない。送信に失敗しても Worker は続ける
    pub progress: Option<tokio::sync::mpsc::UnboundedSender<WorkerProgress>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerOutcome {
    /// 結論メモ(数百トークン)。途中の tool 出力は含めない
    Concluded(String),
    /// API エラー・反復上限など。Core が `[内部調査失敗 <run_id>: <理由>]` にする。timeout は Core が判定する
    Failed(String),
}

#[async_trait]
pub trait Worker: Send + Sync {
    /// 本線にも同じものを渡す。順序も含めて呼び出しごとに同一であること(キャッシュ共有のため)
    fn tool_specs(&self) -> Vec<ToolSpec>;
    async fn run(&self, task: WorkerTask) -> WorkerOutcome;
}

#[async_trait]
pub trait Compactor: Send + Sync {
    /// Layer 3 の範囲を三人称のエピソード要約 1 つに変換する。Layer 2 への統合と上限の管理は Core が行う。
    /// Profile は渡さない。キャラクターの口調が要約に混ざるのを防ぐため
    async fn digest(&self, harness_rules: &str, turns: &[Turn]) -> Result<String>;
}
