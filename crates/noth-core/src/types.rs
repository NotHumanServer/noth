use jiff::Timestamp;
use serde::{Deserialize, Serialize};

pub type GuildId = u64;
pub type ChannelId = u64;
pub type MessageId = u64;
pub type UserId = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub user_id: UserId,
    pub display_name: String,
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingMessage {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    /// router の state に入れるチャンネル名。取れなければ ID の文字列
    pub channel_name: String,
    pub message_id: MessageId,
    pub user_id: UserId,
    pub author_name: String,
    /// `content` のみ。メンションは Channel で表示名か `<user>` に置換済み
    pub content: String,
    pub timestamp: Timestamp,
    pub is_self: bool,
    /// Noth 宛てのメンションを含むか。router-mention が使う
    pub mentions_self: bool,
    /// Noth の発言へのリプライか。引用本文は読まず、参照先の author だけを見る
    pub reply_to_self: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnKind {
    Buffer,
    Trigger,
    Response,
    Conclusion,
    RelevantChunk,
    /// Layer 4 にだけ入る指示(delivery_style など)。Layer 3 には昇格しない
    Instruction,
}

impl TurnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnKind::Buffer => "buffer",
            TurnKind::Trigger => "trigger",
            TurnKind::Response => "response",
            TurnKind::Conclusion => "conclusion",
            TurnKind::RelevantChunk => "relevant_chunk",
            TurnKind::Instruction => "instruction",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "buffer" => TurnKind::Buffer,
            "trigger" => TurnKind::Trigger,
            "response" => TurnKind::Response,
            "conclusion" => TurnKind::Conclusion,
            "relevant_chunk" => TurnKind::RelevantChunk,
            "instruction" => TurnKind::Instruction,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// `active_log` の行 ID。未保存なら None
    pub id: Option<i64>,
    pub role: Role,
    pub kind: TurnKind,
    pub text: String,
    pub chunk_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterState(pub serde_json::Value);

#[derive(Debug, Clone)]
pub struct CompletionParams {
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    /// Gemini の functionResponse は call id ではなく関数名で対応づけるため持つ
    pub name: String,
    pub output: String,
    pub is_error: bool,
}

/// Worker の tool 往復。Layer 4 の後ろに積み、本線の Layer 3 には入れない
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExchangeItem {
    Assistant {
        text: Option<String>,
        calls: Vec<ToolCall>,
        /// provider が返した assistant の生の content。thinking ブロックや thought signature を
        /// 次の呼び出しでそのまま返さないと API に弾かれるので、生成した provider がそのまま使う
        raw: Option<serde_json::Value>,
    },
    ToolResults(Vec<ToolResult>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCategory {
    None,
    WebResearch,
    FactCheck,
    CodeOrTool,
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub run_id: String,
    pub task_category: TaskCategory,
    pub work_dir: std::path::PathBuf,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}
