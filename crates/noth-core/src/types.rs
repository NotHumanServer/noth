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
    pub reply_to: Option<ReplyTo>,
    /// Noth 以外の Bot の発言の目印。Some のとき `content` は空で、判定の起動もデバウンスの更新もしない
    #[serde(default)]
    pub bot_marker: Option<BotMarker>,
}

impl IncomingMessage {
    /// Bot どうしの応酬を起こさないため、目印と Noth 自身の発言は判定を起動しない
    pub fn triggers_judge(&self) -> bool {
        !self.is_self && self.bot_marker.is_none()
    }
}

/// Noth 以外の Bot の発言を、本文を持たずに残す目印。Bot の本文は長く、外部の指示が紛れうるため
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotMarker {
    /// 表示名。改行を除き、長さは Channel で制限済み
    pub author_name: String,
    /// リプライ先の表示名。`allowed_users` でなければ `<user>`。リプライでなければ None
    pub reply_to_author: Option<String>,
    /// `content` に embed のタイトル・説明・フィールドを足した文字数
    pub chars: usize,
}

impl BotMarker {
    /// Layer 3 / Layer 4 の発言行に入れる文字列。`message_id` は tool-discord で本文を読むための手がかり
    pub fn render(&self, message_id: MessageId) -> String {
        let reply = match &self.reply_to_author {
            Some(a) => format!("{a} への返信、"),
            None => String::new(),
        };
        format!(
            "[Bot {} が発言({reply}約{}字。embed を含む) message_id: {message_id}]",
            self.author_name,
            approx_chars(self.chars),
        )
    }
}

/// 桁を丸めて「約」と読める字数にする。小さい数はそのまま
pub(crate) fn approx_chars(chars: usize) -> String {
    let rounded = match chars {
        0..100 => chars,
        100..1000 => chars.div_ceil(10) * 10,
        _ => chars.div_ceil(100) * 100,
    };
    let s = rounded.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Discord のリプライ先。router の state と主 LLM の発言行に出す
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyTo {
    pub message_id: MessageId,
    pub author_name: String,
    pub is_self: bool,
    /// 本文の先頭 60 文字
    pub excerpt: String,
}

impl ReplyTo {
    pub const EXCERPT_CHARS: usize = 60;

    pub fn excerpt_of(content: &str) -> String {
        content.chars().take(Self::EXCERPT_CHARS).collect()
    }
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

/// router に渡す `state`(context-layers.md §4.1)。キーは `noth`(省略可)、`members`(省略可)、`trigger`、`recent`。
/// `trigger[]` の `in_conversation_window` は、Noth が応答してから `conversation_window_secs` 以内の、話していた相手の発言であることを示す(決定的な noth_request ではなく、router への合図)。
/// `recent` の各要素は `{channel_id, channel, author, at, is_self, reply_to?}` に、通常の発言なら `content` を、
/// Bot の目印なら `content` の代わりに `bot_marker: {chars, count, reply_to?}` を持つ(同じチャンネルで同じ Bot の連続した目印は 1 つにまとめ、`count` は元の件数)。まとめた後の目印が 20 を超えると古い方を落とし、代わりに `{channel, at, omitted_bot_messages}` の要素を 1 つ入れる。関連チャンクの候補(`candidates`)は含めない
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterState(pub serde_json::Value);

#[derive(Debug, Clone, Default)]
pub struct CompletionParams {
    pub max_tokens: u32,
    /// コストの集計単位。provider は使わず、Core の計測が記録に使う
    pub purpose: Purpose,
    pub run_id: Option<String>,
}

/// 何のための呼び出しか。`noth cost --by purpose` の集計キー
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    #[default]
    Main,
    Join,
    Worker,
    Compaction,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Main => "main",
            Purpose::Join => "join",
            Purpose::Worker => "worker",
            Purpose::Compaction => "compaction",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    /// 使った分だけ課金される(API キー)
    Metered,
    /// 定額プランの枠を消費する(Codex など)。金額は記録しない
    Subscription,
    /// 無料の呼び出し(TinyFish の Search / Fetch など)。回数の把握のために記録する
    Free,
}

impl Billing {
    pub fn as_str(self) -> &'static str {
        match self {
            Billing::Metered => "metered",
            Billing::Subscription => "subscription",
            Billing::Free => "free",
        }
    }
}

/// 1 回の課金対象の呼び出し。金額は単価表を持つ module が計算し、Core は保存と集計だけを行う
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostRecord {
    /// module 名(`provider-anthropic`、`tool-web-agent` など)
    pub source: String,
    /// LLM ならモデル ID、tool なら API 名(`search`、`agent` など)
    pub model: String,
    pub purpose: Purpose,
    pub run_id: Option<String>,
    /// キャッシュの読み書きを除いた入力トークン
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// 思考のトークンを含む
    pub output_tokens: u64,
    /// トークン以外の課金単位(`step`、`request`、`url` など)と数量。LLM では None
    pub unit: Option<(String, u64)>,
    /// None は単価が分からない。トークン数や数量だけは集計できる
    pub amount_usd: Option<f64>,
    pub billing: Billing,
}

/// 外部 API が返したレート制限の残り。`noth cost` が最新の値を表示する
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitSnapshot {
    pub source: String,
    /// 同じ module 内の区分(`search`、`fetch` など)
    pub bucket: String,
    pub limit: u64,
    pub remaining: u64,
    /// UNIX 秒
    pub reset_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MeterEvent {
    Cost(CostRecord),
    RateLimit(RateLimitSnapshot),
}

/// 送れなくても呼び出し自体は続ける。計測の失敗で会話や調査を止めないため
pub type MeterSink = tokio::sync::mpsc::UnboundedSender<MeterEvent>;

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

/// Worker を起こした経緯。run の記録と、tool・Worker の指示文での扱いの区別に使う
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Noth への依頼(Ack を出して調べる)
    Request,
    /// 人どうしの発言の事実を黙って確かめる。投稿は必要なときだけ
    Supplement,
    /// Noth の誤りの指摘を受けて黙って調べ直す。投稿は必要なときだけ
    Correction,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Request => "request",
            Origin::Supplement => "supplement",
            Origin::Correction => "correction",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "request" => Origin::Request,
            "supplement" => Origin::Supplement,
            "correction" => Origin::Correction,
            _ => return None,
        })
    }
}

/// Worker を起動させた人と場所。tool が「依頼した人に見えるものだけ」を返すために使う
#[derive(Debug, Clone, Default)]
pub struct Audience {
    /// トリガー発言群の発言者(重複なし、Noth 自身を除く)。空なら誰の権限も確認できない
    pub requesters: Vec<UserId>,
    /// トリガー発言のチャンネル。依頼した人が発言した場所なので、権限を再計算せず読んでよい
    pub channel_id: Option<ChannelId>,
    /// `channel_id` の表示名。Worker の指示文でトリガーの場所を人にも分かる形で示す
    pub channel_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub run_id: String,
    pub origin: Origin,
    pub work_dir: std::path::PathBuf,
    /// 有料 API の課金やレート制限の残りを Core に知らせる。None なら記録しない
    pub meter: Option<MeterSink>,
    /// 記録済みの発言の検索窓口。None なら検索できない
    pub history: Option<std::sync::Arc<dyn crate::history::MessageHistory>>,
    pub audience: Audience,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_marker_renders_like_the_design_example() {
        let m = BotMarker {
            author_name: "秘書ちゃん".into(),
            reply_to_author: Some("くらず".into()),
            chars: 1180,
        };
        assert_eq!(
            m.render(123456789012345678),
            "[Bot 秘書ちゃん が発言(くらず への返信、約1,200字。embed を含む) message_id: 123456789012345678]"
        );
        let m = BotMarker {
            reply_to_author: None,
            chars: 42,
            ..m
        };
        assert!(m.render(1).contains("(約42字。"));
    }

    #[test]
    fn origin_round_trips() {
        for o in [Origin::Request, Origin::Supplement, Origin::Correction] {
            assert_eq!(Origin::parse(o.as_str()), Some(o));
        }
        assert_eq!(Origin::parse("web_research"), None);
    }
}
