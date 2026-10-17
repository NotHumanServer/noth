//! 操作用 API(`/_fake/...`)。CLI はこれだけを叩く。request/response の仕様は docs/fakecord-control-api.md
//!
//! リクエスト・レスポンスの型はここが正本で、CLI も同じ型を使ってよい

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::state::{
    AppState, BotAction, BotActionKind, CHANNEL_CATEGORY, CHANNEL_PUBLIC_THREAD, CHANNEL_TEXT,
    Channel, INTERACTION_COMMAND, INTERACTION_COMPONENT, Id, Interaction, InteractionState, Member,
    ROLE_ALLOWED, ROLE_MASTER, Reaction, User, World,
};

/// エラー時の本文。ステータスは 400(入力不正)か 404(対象なし)
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

/// `POST /_fake/send`
#[derive(Debug, Serialize, Deserialize)]
pub struct SendRequest {
    /// チャンネル名(`#` 付きでも可)または ID。スレッドも指定できる
    pub channel: String,
    /// 発言するユーザー名または ID。省略時は kurazu
    #[serde(rename = "as", default)]
    pub as_user: Option<String>,
    pub content: String,
    /// 返信先メッセージ。指定すると message_reference を付け、kind を 19 にする
    #[serde(default)]
    pub reply_to: Option<Id>,
}

/// send のレスポンス。cursor は送信直前の bot_actions 末尾 seq で、wait-reply の since に渡せる
#[derive(Debug, Serialize, Deserialize)]
pub struct SendResponse {
    /// 作られたメッセージ(Discord の JSON 形)
    pub message: Value,
    pub cursor: u64,
}

/// `POST /_fake/channels`
#[derive(Debug, Serialize, Deserialize)]
pub struct ChannelCreateRequest {
    pub name: String,
    /// 親カテゴリの名前または ID
    #[serde(default)]
    pub parent: Option<String>,
}

/// `POST /_fake/categories`
#[derive(Debug, Serialize, Deserialize)]
pub struct CategoryCreateRequest {
    pub name: String,
}

/// `PATCH /_fake/channels/{channel}`
#[derive(Debug, Serialize, Deserialize)]
pub struct ChannelRenameRequest {
    pub name: String,
}

/// チャンネル・カテゴリ・スレッドを作った/改名したときの共通レスポンス
#[derive(Debug, Serialize, Deserialize)]
pub struct ChannelResponse {
    pub channel: Value,
}

/// `POST /_fake/threads`
#[derive(Debug, Serialize, Deserialize)]
pub struct ThreadCreateRequest {
    /// 親テキストチャンネルの名前または ID
    pub channel: String,
    pub name: String,
    /// 指定するとそのメッセージから作るスレッドになる(開始メッセージ扱い)。省略時は単独のスレッド
    #[serde(default)]
    pub message_id: Option<Id>,
    /// 作成者。省略時は kurazu
    #[serde(rename = "as", default)]
    pub as_user: Option<String>,
}

/// `POST /_fake/thread-list-sync`
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ThreadListSyncRequest {
    /// 親テキストチャンネルの名前または ID。指定するとそのチャンネルのスレッドだけを同期する。
    /// 省略時は guild 内の全アクティブスレッド
    #[serde(default)]
    pub channel: Option<String>,
}

/// スラッシュコマンドのオプション 1 つ
#[derive(Debug, Serialize, Deserialize)]
pub struct SlashOption {
    pub name: String,
    /// 文字列・数値・真偽値のいずれか(JSON のまま Discord の option.value に入れる)
    pub value: Value,
}

/// `POST /_fake/slash`
#[derive(Debug, Serialize, Deserialize)]
pub struct SlashRequest {
    /// コマンド名(例: `noth`)
    pub name: String,
    /// サブコマンド(グループ)の並び。例: `["ignore", "add"]`
    #[serde(default)]
    pub path: Vec<String>,
    #[serde(default)]
    pub options: Vec<SlashOption>,
    #[serde(rename = "as", default)]
    pub as_user: Option<String>,
    /// 実行したチャンネル。省略時は general
    #[serde(default)]
    pub channel: Option<String>,
}

/// slash / click のレスポンス
#[derive(Debug, Serialize, Deserialize)]
pub struct InteractionResponse {
    pub interaction_id: Id,
    pub token: String,
    pub cursor: u64,
}

/// `POST /_fake/click`
#[derive(Debug, Serialize, Deserialize)]
pub struct ClickRequest {
    pub custom_id: String,
    /// 押すボタンのあるメッセージ。省略時は channel 内で custom_id を持つ最新のメッセージ
    #[serde(default)]
    pub message_id: Option<Id>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(rename = "as", default)]
    pub as_user: Option<String>,
}

/// `POST /_fake/react`
#[derive(Debug, Serialize, Deserialize)]
pub struct ReactRequest {
    /// Unicode の絵文字そのもの、またはカスタム絵文字の `name:id`
    pub emoji: String,
    /// 省略時は channel の最新メッセージ
    #[serde(default)]
    pub message_id: Option<Id>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(rename = "as", default)]
    pub as_user: Option<String>,
}

/// react のレスポンス
#[derive(Debug, Serialize, Deserialize)]
pub struct ReactResponse {
    pub message_id: Id,
    pub cursor: u64,
}

/// `GET /_fake/log` のクエリ
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LogQuery {
    /// 省略時は全チャンネル
    pub channel: Option<String>,
    /// 返すメッセージ数の上限(新しい方から)。既定 20
    pub limit: Option<usize>,
    /// この seq より後の Bot 操作だけ返す。省略時は全部
    pub since: Option<u64>,
}

/// `GET /_fake/log` のレスポンス。messages は古い順
#[derive(Debug, Serialize, Deserialize)]
pub struct LogResponse {
    pub messages: Vec<Value>,
    pub actions: Vec<BotAction>,
    pub cursor: u64,
}

/// `POST /_fake/wait-reply`
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WaitReplyRequest {
    /// この seq より後の Bot 操作を待つ。省略時は直近の send/slash/click/react の cursor
    #[serde(default)]
    pub since: Option<u64>,
    /// 秒。既定 60
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// 指定すると、そのチャンネルへの操作だけを見る(interaction 系は常に対象)
    #[serde(default)]
    pub channel: Option<String>,
    /// 返信が出てから新しい操作が来なくなるまで待つミリ秒。既定 2000
    #[serde(default)]
    pub settle_ms: Option<u64>,
}

/// wait-reply のレスポンス。タイムアウトしても 200 で、timed_out を true にする
#[derive(Debug, Serialize, Deserialize)]
pub struct WaitReplyResponse {
    pub timed_out: bool,
    /// 返信(投稿・編集・interaction 応答)が 1 つでもあったか
    pub replied: bool,
    pub actions: Vec<BotAction>,
    /// 返した最後の seq(次の since に使える)
    pub cursor: u64,
}

/// `POST /_fake/users`
#[derive(Debug, Serialize, Deserialize)]
pub struct UserAddRequest {
    pub name: String,
    /// "allowed" / "master" / "none"。master は allowed も付ける。既定 allowed
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub bot: bool,
}

/// `POST /_fake/users` のレスポンス
#[derive(Debug, Serialize, Deserialize)]
pub struct UserAddResponse {
    pub user: Value,
    pub member: Value,
}

/// `POST /_fake/snapshot`
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SnapshotRequest {
    /// 保存先。省略時は `serve --snapshot` のパス
    #[serde(default)]
    pub path: Option<String>,
}

/// `POST /_fake/snapshot` のレスポンス
#[derive(Debug, Serialize, Deserialize)]
pub struct SnapshotResponse {
    pub path: String,
}

// ---------- helpers ----------

type HandlerResult = Result<Json<Value>, (StatusCode, Json<ErrorBody>)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (status, Json(ErrorBody { error: msg.into() }))
}

fn find_channel_id(world: &World, name_or_id: &str) -> Option<Id> {
    if let Ok(id) = name_or_id.parse::<Id>()
        && world.channels.contains_key(&id)
    {
        return Some(id);
    }
    let name = name_or_id.trim_start_matches('#');
    world
        .channels
        .values()
        .filter(|c| c.name == name)
        .min_by_key(|c| c.id)
        .map(|c| c.id)
}

fn find_user_id(world: &World, name_or_id: &str) -> Option<Id> {
    if let Ok(id) = name_or_id.parse::<Id>()
        && world.users.contains_key(&id)
    {
        return Some(id);
    }
    let name = name_or_id.trim_start_matches('@');
    world
        .users
        .values()
        .filter(|u| u.username.eq_ignore_ascii_case(name))
        .min_by_key(|u| u.id)
        .map(|u| u.id)
}

fn primary_guild_id(world: &World) -> Id {
    world.guilds.keys().next().copied().unwrap_or(Id(0))
}

fn role_id_by_name(world: &World, guild_id: Id, name: &str) -> Option<Id> {
    world
        .roles
        .values()
        .find(|r| r.guild_id == guild_id && r.name == name)
        .map(|r| r.id)
}

fn action_channel(world: &World, action: &BotAction) -> Option<Id> {
    match &action.kind {
        BotActionKind::Post { channel_id, .. }
        | BotActionKind::Edit { channel_id, .. }
        | BotActionKind::Reaction { channel_id, .. }
        | BotActionKind::Typing { channel_id } => Some(*channel_id),
        BotActionKind::InteractionResponse { interaction_id, .. }
        | BotActionKind::InteractionEdit { interaction_id, .. }
        | BotActionKind::InteractionFollowup { interaction_id, .. } => {
            world.interactions.get(interaction_id).map(|i| i.channel_id)
        }
    }
}

fn is_reply_action(action: &BotAction) -> bool {
    match &action.kind {
        BotActionKind::Post { .. }
        | BotActionKind::Edit { .. }
        | BotActionKind::InteractionEdit { .. }
        | BotActionKind::InteractionFollowup { .. } => true,
        BotActionKind::InteractionResponse { response_type, .. } => {
            *response_type == 4 || *response_type == 7
        }
        _ => false,
    }
}

const OPTION_USER: u8 = 6;
const OPTION_CHANNEL: u8 = 7;

/// 登録された定義から、path(サブコマンドグループ、サブコマンド)の先にある引数の定義を取り出す
fn declared_options(definition: &Value, path: &[String]) -> Vec<Value> {
    let mut options = definition["options"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for name in path {
        options = options
            .iter()
            .find(|o| o["name"] == name.as_str())
            .and_then(|o| o["options"].as_array().cloned())
            .unwrap_or_default();
    }
    options
}

fn declared_type(declared: &[Value], name: &str) -> Option<u8> {
    declared
        .iter()
        .find(|o| o["name"] == name)
        .and_then(|o| o["type"].as_u64())
        .and_then(|t| u8::try_from(t).ok())
}

/// 定義に無い引数の型を、値の見た目から推す
fn option_type(v: &Value) -> u8 {
    if v.is_boolean() {
        5
    } else if v.is_string() {
        3
    } else if v.is_i64() || v.is_u64() {
        4
    } else if v.is_f64() {
        10
    } else {
        3
    }
}

fn find_custom_id_type(value: &Value, target: &str) -> Option<u8> {
    match value {
        Value::Object(map) => {
            if map.get("custom_id").and_then(|v| v.as_str()) == Some(target) {
                return map.get("type").and_then(|v| v.as_u64()).map(|t| t as u8);
            }
            for v in map.values() {
                if let Some(t) = find_custom_id_type(v, target) {
                    return Some(t);
                }
            }
            None
        }
        Value::Array(arr) => {
            for v in arr {
                if let Some(t) = find_custom_id_type(v, target) {
                    return Some(t);
                }
            }
            None
        }
        _ => None,
    }
}

fn contains_custom_id(value: &Value, target: &str) -> bool {
    match value {
        Value::Object(map) => {
            if map.get("custom_id").and_then(|v| v.as_str()) == Some(target) {
                return true;
            }
            map.values().any(|v| contains_custom_id(v, target))
        }
        Value::Array(arr) => arr.iter().any(|v| contains_custom_id(v, target)),
        _ => false,
    }
}

fn parse_mentions(content: &str, world: &World) -> (String, Vec<Id>) {
    // ユーザー名を長い順に並べる
    let mut users: Vec<&User> = world.users.values().collect();
    users.sort_by_key(|u| std::cmp::Reverse(u.username.len()));

    let mut result = String::new();
    let mut mentions: Vec<Id> = Vec::new();
    let chars: Vec<char> = content.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            // `<@ID>` 形式
            if i > 0 && chars[i - 1] == '<' {
                // 先頭の '<' はすでに result に入っているはず。ID を読む
                let mut j = i + 1;
                let mut id_str = String::new();
                while j < chars.len() && chars[j].is_ascii_digit() {
                    id_str.push(chars[j]);
                    j += 1;
                }
                if j < chars.len() && chars[j] == '>' && !id_str.is_empty() {
                    // そのままコピー
                    result.push('@');
                    i += 1;
                    // id_str は後続でコピーされるのでここでは何もしない
                    continue;
                }
            }
            // @name の照合
            let rest: String = chars[i + 1..].iter().collect();
            let mut matched: Option<&User> = None;
            for u in &users {
                if rest.starts_with(&u.username) {
                    let next_idx = i + 1 + u.username.chars().count();
                    let boundary_ok = if next_idx < chars.len() {
                        let c = chars[next_idx];
                        !(c.is_alphanumeric() || c == '_' || c == '-')
                    } else {
                        true
                    };
                    if boundary_ok {
                        matched = Some(*u);
                        break;
                    }
                }
            }
            if let Some(u) = matched {
                let name_len = u.username.chars().count();
                result.push_str(&format!("<@{}>", u.id));
                if !mentions.contains(&u.id) {
                    mentions.push(u.id);
                }
                i += 1 + name_len;
                continue;
            }
            result.push('@');
            i += 1;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    // `<@ID>` で既に書かれていたメンションを mentions に追加(上のループは名前の照合だけ)
    let bytes = content.as_bytes();
    let mut k = 0;
    while k < bytes.len() {
        if k + 2 < bytes.len() && bytes[k] == b'<' && bytes[k + 1] == b'@' {
            let mut m = k + 2;
            let mut digits = String::new();
            while m < bytes.len() && bytes[m].is_ascii_digit() {
                digits.push(bytes[m] as char);
                m += 1;
            }
            if m < bytes.len() && bytes[m] == b'>' && !digits.is_empty() {
                if let Ok(id) = digits.parse::<Id>()
                    && !mentions.contains(&id)
                {
                    mentions.push(id);
                }
                k = m + 1;
                continue;
            }
        }
        k += 1;
    }
    (result, mentions)
}

// ---------- handlers ----------

/// `POST /_fake/send`
pub async fn send(State(state): State<AppState>, Json(req): Json<SendRequest>) -> HandlerResult {
    if req.content.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "content が空です"));
    }
    let channel_id = {
        let w = state.world();
        match find_channel_id(&w, &req.channel) {
            Some(id) => id,
            None => return Err(err(StatusCode::NOT_FOUND, "チャンネルが見つかりません")),
        }
    };
    {
        let w = state.world();
        if w.channels.get(&channel_id).map(|c| c.kind) == Some(CHANNEL_CATEGORY) {
            return Err(err(StatusCode::BAD_REQUEST, "カテゴリには発言できません"));
        }
    }
    let user_id = {
        let w = state.world();
        let name = req.as_user.as_deref().unwrap_or("kurazu");
        match find_user_id(&w, name) {
            Some(id) => id,
            None => return Err(err(StatusCode::NOT_FOUND, "ユーザーが見つかりません")),
        }
    };

    // reply_to の検証
    let reply_to_msg = if let Some(rid) = req.reply_to {
        let w = state.world();
        let msg = w.messages.get(&rid);
        match msg {
            Some(m) if m.channel_id == channel_id => Some((rid, m.author_id)),
            _ => {
                return Err(err(
                    StatusCode::NOT_FOUND,
                    "返信先メッセージが見つかりません",
                ));
            }
        }
    } else {
        None
    };

    let cursor = {
        let w = state.world();
        w.action_head()
    };

    let (message_json, message_for_publish) = {
        let mut w = state.world();
        let (content, mut mentions) = parse_mentions(&req.content, &w);

        // reply 先の作者を mentions に追加
        if let Some((_, author_id)) = reply_to_msg
            && !mentions.contains(&author_id)
        {
            mentions.push(author_id);
        }

        let id = w.alloc_id();
        let kind = if req.reply_to.is_some() { 19u8 } else { 0u8 };
        let message_reference = req.reply_to.map(|mid| crate::state::MessageReference {
            message_id: Some(mid),
            channel_id: Some(channel_id),
            guild_id: Some(primary_guild_id(&w)),
        });

        let msg = crate::state::Message {
            id,
            channel_id,
            author_id: user_id,
            content,
            timestamp: crate::state::now_rfc3339(),
            edited_timestamp: None,
            kind,
            message_reference,
            mentions,
            components: Vec::new(),
            flags: 0,
            reactions: Vec::new(),
            pinned: false,
            interaction_id: None,
        };
        let msg_clone = msg.clone();
        w.messages.insert(id, msg);
        w.cursor_mark = cursor;

        let gateway_json = w.message_to_discord_json(&msg_clone, true);
        let rest_json = w.message_to_discord_json(&msg_clone, false);
        state.publish("MESSAGE_CREATE", gateway_json);
        (rest_json, msg_clone)
    };
    let _ = message_for_publish;

    Ok(Json(json!({
        "message": message_json,
        "cursor": cursor,
    })))
}

/// `POST /_fake/channels`
pub async fn create_channel(
    State(state): State<AppState>,
    Json(req): Json<ChannelCreateRequest>,
) -> HandlerResult {
    let normalized = req.name.to_lowercase().replace(' ', "-");
    if normalized.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "name が空です"));
    }

    let parent_id = if let Some(p) = &req.parent {
        let w = state.world();
        let id = match find_channel_id(&w, p) {
            Some(id) => id,
            None => return Err(err(StatusCode::NOT_FOUND, "親チャンネルが見つかりません")),
        };
        let kind = w.channels.get(&id).map(|c| c.kind).unwrap_or(0);
        drop(w);
        if kind != CHANNEL_CATEGORY {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "親はカテゴリである必要があります",
            ));
        }
        Some(id)
    } else {
        None
    };

    let position = {
        let w = state.world();
        let max = w
            .channels
            .values()
            .filter(|c| c.parent_id == parent_id)
            .map(|c| c.position)
            .max()
            .unwrap_or(-1);
        max + 1
    };

    let channel_json = {
        let mut w = state.world();
        let id = w.alloc_id();
        let guild_id = primary_guild_id(&w);
        let ch = Channel {
            id,
            guild_id,
            kind: CHANNEL_TEXT,
            name: normalized,
            parent_id,
            position,
            topic: None,
            owner_id: None,
            archived: false,
        };
        let cj = ch.to_discord_json();
        w.channels.insert(id, ch);
        state.publish("CHANNEL_CREATE", cj.clone());
        cj
    };

    Ok(Json(json!({ "channel": channel_json })))
}

/// `POST /_fake/categories`
pub async fn create_category(
    State(state): State<AppState>,
    Json(req): Json<CategoryCreateRequest>,
) -> HandlerResult {
    if req.name.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "name が空です"));
    }

    let position = {
        let w = state.world();
        let max = w
            .channels
            .values()
            .filter(|c| c.parent_id.is_none())
            .map(|c| c.position)
            .max()
            .unwrap_or(-1);
        max + 1
    };

    let channel_json = {
        let mut w = state.world();
        let id = w.alloc_id();
        let guild_id = primary_guild_id(&w);
        let ch = Channel {
            id,
            guild_id,
            kind: CHANNEL_CATEGORY,
            name: req.name,
            parent_id: None,
            position,
            topic: None,
            owner_id: None,
            archived: false,
        };
        let cj = ch.to_discord_json();
        w.channels.insert(id, ch);
        state.publish("CHANNEL_CREATE", cj.clone());
        cj
    };

    Ok(Json(json!({ "channel": channel_json })))
}

/// `POST /_fake/threads`
pub async fn create_thread(
    State(state): State<AppState>,
    Json(req): Json<ThreadCreateRequest>,
) -> HandlerResult {
    let parent_id = {
        let w = state.world();
        match find_channel_id(&w, &req.channel) {
            Some(id) => id,
            None => return Err(err(StatusCode::NOT_FOUND, "チャンネルが見つかりません")),
        }
    };
    {
        let w = state.world();
        if w.channels.get(&parent_id).map(|c| c.kind) != Some(CHANNEL_TEXT) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "親はテキストチャンネルである必要があります",
            ));
        }
    }
    let user_id = {
        let w = state.world();
        let name = req.as_user.as_deref().unwrap_or("kurazu");
        match find_user_id(&w, name) {
            Some(id) => id,
            None => return Err(err(StatusCode::NOT_FOUND, "ユーザーが見つかりません")),
        }
    };

    // message_id 検証
    if let Some(mid) = req.message_id {
        let w = state.world();
        let ok = w
            .messages
            .get(&mid)
            .map(|m| m.channel_id == parent_id)
            .unwrap_or(false);
        if !ok {
            return Err(err(StatusCode::NOT_FOUND, "メッセージが見つかりません"));
        }
    }

    let thread_id = if let Some(mid) = req.message_id {
        mid
    } else {
        // alloc later
        Id(0)
    };

    let channel_json = {
        let mut w = state.world();
        let id = if req.message_id.is_some() {
            w.last_id = w.last_id.max(thread_id.0);
            thread_id
        } else {
            w.alloc_id()
        };
        let guild_id = primary_guild_id(&w);
        let ch = Channel {
            id,
            guild_id,
            kind: CHANNEL_PUBLIC_THREAD,
            name: req.name.clone(),
            parent_id: Some(parent_id),
            position: 0,
            topic: None,
            owner_id: Some(user_id),
            archived: false,
        };
        let mut cj = ch.to_discord_json();
        cj["newly_created"] = json!(true);
        w.channels.insert(id, ch);
        state.publish("THREAD_CREATE", cj.clone());
        // newly_created はレスポンスの channel には含めない? 仕様: d に足す。Channel::to_discord_json は含めない。レスポンスは ChannelResponse channel = to_discord_json。再計算
        let ch2 = w.channels.get(&id).unwrap();
        let resp = ch2.to_discord_json();
        // publish は cj を使う済み
        let _ = cj;
        resp
    };

    Ok(Json(json!({ "channel": channel_json })))
}

/// `POST /_fake/thread-list-sync`。アクティブなスレッドを全部含む THREAD_LIST_SYNC を配信する。
/// 本物の Discord は Gateway 接続の直後などにこれを送り、bot はスレッドの一覧をここで入れ替える
pub async fn thread_list_sync(
    State(state): State<AppState>,
    Json(req): Json<ThreadListSyncRequest>,
) -> HandlerResult {
    let w = state.world();
    let guild_id = primary_guild_id(&w);
    let parent_id = match &req.channel {
        Some(name) => match find_channel_id(&w, name) {
            Some(id) if w.channels.get(&id).map(|c| c.kind) == Some(CHANNEL_TEXT) => Some(id),
            Some(_) => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "親はテキストチャンネルである必要があります",
                ));
            }
            None => return Err(err(StatusCode::NOT_FOUND, "チャンネルが見つかりません")),
        },
        None => None,
    };
    let threads: Vec<&Channel> = w
        .channels
        .values()
        .filter(|c| {
            c.kind == CHANNEL_PUBLIC_THREAD
                && c.guild_id == guild_id
                && !c.archived
                && parent_id.is_none_or(|p| c.parent_id == Some(p))
        })
        .collect();
    // bot が参加しているスレッドの ThreadMember。fakecord のスレッドには bot が常に入っている扱い
    let join_timestamp = crate::state::now_rfc3339();
    let members: Vec<Value> = threads
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "user_id": w.bot_user_id,
                "join_timestamp": join_timestamp,
                "flags": 0,
            })
        })
        .collect();
    let mut d = json!({
        "guild_id": guild_id,
        "threads": threads.iter().map(|t| t.to_discord_json()).collect::<Vec<_>>(),
        "members": members,
    });
    if let Some(p) = parent_id {
        d["channel_ids"] = json!([p]);
    }
    let count = threads.len();
    state.publish("THREAD_LIST_SYNC", d);
    Ok(Json(json!({ "guild_id": guild_id, "threads": count })))
}

/// `PATCH /_fake/channels/{channel}`(チャンネル・カテゴリ・スレッド共通)
pub async fn rename_channel(
    State(state): State<AppState>,
    Path(channel): Path<String>,
    Json(req): Json<ChannelRenameRequest>,
) -> HandlerResult {
    let channel_id = {
        let w = state.world();
        match find_channel_id(&w, &channel) {
            Some(id) => id,
            None => return Err(err(StatusCode::NOT_FOUND, "チャンネルが見つかりません")),
        }
    };

    let (kind, new_name) = {
        let w = state.world();
        let kind = w.channels.get(&channel_id).map(|c| c.kind).unwrap_or(0);
        let normalized = if kind == CHANNEL_CATEGORY {
            if req.name.is_empty() {
                return Err(err(StatusCode::BAD_REQUEST, "name が空です"));
            }
            req.name.clone()
        } else {
            let n = req.name.to_lowercase().replace(' ', "-");
            if n.is_empty() {
                return Err(err(StatusCode::BAD_REQUEST, "name が空です"));
            }
            n
        };
        (kind, normalized)
    };

    let channel_json = {
        let mut w = state.world();
        if let Some(ch) = w.channels.get_mut(&channel_id) {
            ch.name = new_name;
        }
        let ch = w.channels.get(&channel_id).unwrap();
        let cj = ch.to_discord_json();
        let event = if kind == CHANNEL_PUBLIC_THREAD {
            "THREAD_UPDATE"
        } else {
            "CHANNEL_UPDATE"
        };
        state.publish(event, cj.clone());
        cj
    };

    Ok(Json(json!({ "channel": channel_json })))
}

/// `DELETE /_fake/channels/{channel}`(チャンネル・カテゴリ・スレッド共通)
pub async fn delete_channel(
    State(state): State<AppState>,
    Path(channel): Path<String>,
) -> HandlerResult {
    let channel_id = {
        let w = state.world();
        match find_channel_id(&w, &channel) {
            Some(id) => id,
            None => return Err(err(StatusCode::NOT_FOUND, "チャンネルが見つかりません")),
        }
    };

    let (kind, channel_json, guild_id, parent_id) = {
        let w = state.world();
        let ch = match w.channels.get(&channel_id) {
            Some(c) => c.clone(),
            None => return Err(err(StatusCode::NOT_FOUND, "チャンネルが見つかりません")),
        };
        let cj = ch.to_discord_json();
        (ch.kind, cj, ch.guild_id, ch.parent_id)
    };

    match kind {
        CHANNEL_TEXT => {
            // 配下スレッドを先に消す
            let threads: Vec<Id> = {
                let w = state.world();
                w.channels
                    .values()
                    .filter(|c| c.kind == CHANNEL_PUBLIC_THREAD && c.parent_id == Some(channel_id))
                    .map(|c| c.id)
                    .collect()
            };
            for tid in &threads {
                let (tg, tp) = {
                    let w = state.world();
                    let ch = w.channels.get(tid).unwrap();
                    (ch.guild_id, ch.parent_id)
                };
                state.publish(
                    "THREAD_DELETE",
                    json!({
                        "id": tid,
                        "guild_id": tg,
                        "parent_id": tp,
                        "type": CHANNEL_PUBLIC_THREAD,
                    }),
                );
            }
            // 削除
            {
                let mut w = state.world();
                for tid in &threads {
                    w.channels.remove(tid);
                    w.messages.retain(|_, m| m.channel_id != *tid);
                }
                w.messages.retain(|_, m| m.channel_id != channel_id);
                w.channels.remove(&channel_id);
            }
            state.publish("CHANNEL_DELETE", channel_json.clone());
        }
        CHANNEL_CATEGORY => {
            // 子チャンネルの parent_id を null に
            let children: Vec<Id> = {
                let w = state.world();
                w.channels
                    .values()
                    .filter(|c| c.parent_id == Some(channel_id))
                    .map(|c| c.id)
                    .collect()
            };
            for cid in &children {
                let cj = {
                    let mut w = state.world();
                    if let Some(ch) = w.channels.get_mut(cid) {
                        ch.parent_id = None;
                    }
                    let ch = w.channels.get(cid).unwrap();
                    ch.to_discord_json()
                };
                state.publish("CHANNEL_UPDATE", cj);
            }
            {
                let mut w = state.world();
                w.channels.remove(&channel_id);
            }
            state.publish("CHANNEL_DELETE", channel_json.clone());
        }
        CHANNEL_PUBLIC_THREAD => {
            state.publish(
                "THREAD_DELETE",
                json!({
                    "id": channel_id,
                    "guild_id": guild_id,
                    "parent_id": parent_id,
                    "type": CHANNEL_PUBLIC_THREAD,
                }),
            );
            {
                let mut w = state.world();
                w.channels.remove(&channel_id);
                w.messages.retain(|_, m| m.channel_id != channel_id);
            }
        }
        _ => {}
    }

    Ok(Json(json!({ "deleted": channel_json })))
}

/// `POST /_fake/slash`
pub async fn slash(State(state): State<AppState>, Json(req): Json<SlashRequest>) -> HandlerResult {
    if req.path.len() > 2 {
        return Err(err(StatusCode::BAD_REQUEST, "path が長すぎます"));
    }

    let channel_id = {
        let w = state.world();
        let name = req.channel.as_deref().unwrap_or("general");
        match find_channel_id(&w, name) {
            Some(id) => id,
            None => return Err(err(StatusCode::NOT_FOUND, "チャンネルが見つかりません")),
        }
    };
    let user_id = {
        let w = state.world();
        let name = req.as_user.as_deref().unwrap_or("kurazu");
        match find_user_id(&w, name) {
            Some(id) => id,
            None => return Err(err(StatusCode::NOT_FOUND, "ユーザーが見つかりません")),
        }
    };

    // コマンド ID と、登録時の定義から宣言されたオプションを引く
    let (cmd_id, declared) = {
        let w = state.world();
        match w.commands.iter().find(|c| c.name == req.name) {
            Some(c) => (c.id, declared_options(&c.definition, &req.path)),
            None => return Err(err(StatusCode::NOT_FOUND, "未登録のコマンドです")),
        }
    };

    // 本物の Discord は channel / user 型のオプションに ID を入れ、resolved に実体を付ける。
    // 型は値の見た目ではなく、登録された定義に従う
    let mut resolved_channels = serde_json::Map::new();
    let mut resolved_users = serde_json::Map::new();
    let mut converted = Vec::with_capacity(req.options.len());
    {
        let w = state.world();
        for o in &req.options {
            let kind = declared_type(&declared, &o.name).unwrap_or_else(|| option_type(&o.value));
            let raw = o
                .value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| o.value.to_string());
            let value = match kind {
                OPTION_CHANNEL => {
                    let Some(id) = find_channel_id(&w, &raw) else {
                        return Err(err(StatusCode::NOT_FOUND, "チャンネルが見つかりません"));
                    };
                    let c = &w.channels[&id];
                    resolved_channels.insert(
                        id.to_string(),
                        json!({ "id": id, "name": c.name, "type": c.kind, "permissions": "0" }),
                    );
                    json!(id)
                }
                OPTION_USER => {
                    let Some(id) = find_user_id(&w, &raw) else {
                        return Err(err(StatusCode::NOT_FOUND, "ユーザーが見つかりません"));
                    };
                    resolved_users.insert(id.to_string(), w.users[&id].to_discord_json());
                    json!(id)
                }
                _ => o.value.clone(),
            };
            converted.push(json!({ "name": o.name, "type": kind, "value": value }));
        }
    }

    let assembled_options = if req.path.is_empty() {
        converted
    } else if req.path.len() == 1 {
        vec![json!({
            "name": req.path[0],
            "type": 1,
            "options": converted,
        })]
    } else {
        vec![json!({
            "name": req.path[0],
            "type": 2,
            "options": [{
                "name": req.path[1],
                "type": 1,
                "options": converted,
            }],
        })]
    };

    let guild_id = {
        let w = state.world();
        primary_guild_id(&w)
    };

    let cursor = {
        let w = state.world();
        w.action_head()
    };

    let token = {
        let mut w = state.world();
        let a = w.alloc_id();
        let b = w.alloc_id();
        format!(
            "{:016x}{:016x}{:016x}{:016x}",
            a.0,
            b.0,
            a.0.rotate_left(7),
            b.0.rotate_left(13)
        )
    };

    let mut data = json!({
        "id": cmd_id,
        "name": req.name,
        "type": 1,
        "guild_id": guild_id,
        "options": assembled_options,
    });
    if !resolved_channels.is_empty() || !resolved_users.is_empty() {
        data["resolved"] = json!({ "channels": resolved_channels, "users": resolved_users });
    }

    let interaction = {
        let mut w = state.world();
        let id = w.alloc_id();
        let inter = Interaction {
            id,
            token: token.clone(),
            application_id: w.application_id,
            guild_id,
            channel_id,
            user_id,
            kind: INTERACTION_COMMAND,
            data,
            message_id: None,
            state: InteractionState::Pending,
            original_message_id: None,
            created_at: crate::state::now_rfc3339(),
        };
        let inter_clone = inter.clone();
        w.interactions.insert(id, inter);
        w.cursor_mark = cursor;
        let gateway_json = w.interaction_to_discord_json(&inter_clone);
        state.publish("INTERACTION_CREATE", gateway_json);
        inter_clone
    };

    Ok(Json(json!({
        "interaction_id": interaction.id,
        "token": interaction.token,
        "cursor": cursor,
    })))
}

/// `POST /_fake/click`
pub async fn click(State(state): State<AppState>, Json(req): Json<ClickRequest>) -> HandlerResult {
    let user_id = {
        let w = state.world();
        let name = req.as_user.as_deref().unwrap_or("kurazu");
        match find_user_id(&w, name) {
            Some(id) => id,
            None => return Err(err(StatusCode::NOT_FOUND, "ユーザーが見つかりません")),
        }
    };

    // メッセージの特定
    let (message_id, channel_id_for_search) = if let Some(mid) = req.message_id {
        (mid, None)
    } else {
        let cname = req.channel.as_deref().unwrap_or("general");
        let cid = {
            let w = state.world();
            match find_channel_id(&w, cname) {
                Some(id) => id,
                None => return Err(err(StatusCode::NOT_FOUND, "チャンネルが見つかりません")),
            }
        };
        // 最新メッセージを探す
        let found = {
            let w = state.world();
            let mut result = None;
            for (mid, msg) in w.messages.iter().rev() {
                if msg.channel_id == cid {
                    let arr = Value::Array(msg.components.clone());
                    if contains_custom_id(&arr, &req.custom_id) {
                        result = Some(*mid);
                        break;
                    }
                }
            }
            result
        };
        match found {
            Some(mid) => (mid, Some(cid)),
            None => return Err(err(StatusCode::NOT_FOUND, "メッセージが見つかりません")),
        }
    };

    // メッセージ存在確認
    let msg = {
        let w = state.world();
        match w.messages.get(&message_id) {
            Some(m) => m.clone(),
            None => return Err(err(StatusCode::NOT_FOUND, "メッセージが見つかりません")),
        }
    };
    let channel_id = msg.channel_id;

    let component_type = {
        let arr = Value::Array(msg.components.clone());
        find_custom_id_type(&arr, &req.custom_id).unwrap_or(2)
    };

    let cursor = {
        let w = state.world();
        w.action_head()
    };

    let token = {
        let mut w = state.world();
        let a = w.alloc_id();
        let b = w.alloc_id();
        format!(
            "{:016x}{:016x}{:016x}{:016x}",
            a.0,
            b.0,
            a.0.rotate_left(7),
            b.0.rotate_left(13)
        )
    };

    let guild_id = {
        let w = state.world();
        primary_guild_id(&w)
    };

    let data = json!({
        "custom_id": req.custom_id,
        "component_type": component_type,
    });

    let interaction = {
        let mut w = state.world();
        let id = w.alloc_id();
        let inter = Interaction {
            id,
            token: token.clone(),
            application_id: w.application_id,
            guild_id,
            channel_id,
            user_id,
            kind: INTERACTION_COMPONENT,
            data,
            message_id: Some(message_id),
            state: InteractionState::Pending,
            original_message_id: None,
            created_at: crate::state::now_rfc3339(),
        };
        let inter_clone = inter.clone();
        w.interactions.insert(id, inter);
        w.cursor_mark = cursor;
        let gateway_json = w.interaction_to_discord_json(&inter_clone);
        state.publish("INTERACTION_CREATE", gateway_json);
        inter_clone
    };

    let _ = channel_id_for_search;

    Ok(Json(json!({
        "interaction_id": interaction.id,
        "token": interaction.token,
        "cursor": cursor,
    })))
}

/// `POST /_fake/react`
pub async fn react(State(state): State<AppState>, Json(req): Json<ReactRequest>) -> HandlerResult {
    let message_id = if let Some(mid) = req.message_id {
        mid
    } else {
        let cname = req.channel.as_deref().unwrap_or("general");
        let cid = {
            let w = state.world();
            match find_channel_id(&w, cname) {
                Some(id) => id,
                None => return Err(err(StatusCode::NOT_FOUND, "チャンネルが見つかりません")),
            }
        };
        let found = {
            let w = state.world();
            let mut result = None;
            for (mid, msg) in w.messages.iter().rev() {
                if msg.channel_id == cid {
                    result = Some(*mid);
                    break;
                }
            }
            result
        };
        match found {
            Some(mid) => mid,
            None => return Err(err(StatusCode::NOT_FOUND, "メッセージが見つかりません")),
        }
    };

    // メッセージ存在確認
    {
        let w = state.world();
        if !w.messages.contains_key(&message_id) {
            return Err(err(StatusCode::NOT_FOUND, "メッセージが見つかりません"));
        }
    }

    let user_id = {
        let w = state.world();
        let name = req.as_user.as_deref().unwrap_or("kurazu");
        match find_user_id(&w, name) {
            Some(id) => id,
            None => return Err(err(StatusCode::NOT_FOUND, "ユーザーが見つかりません")),
        }
    };

    let (channel_id, guild_id) = {
        let w = state.world();
        let msg = w.messages.get(&message_id).unwrap();
        let ch = w.channels.get(&msg.channel_id).unwrap();
        (msg.channel_id, ch.guild_id)
    };

    // Reaction 追加
    {
        let mut w = state.world();
        let msg = w.messages.get_mut(&message_id).unwrap();
        if let Some(r) = msg.reactions.iter_mut().find(|r| r.emoji == req.emoji) {
            if !r.user_ids.contains(&user_id) {
                r.user_ids.push(user_id);
            }
        } else {
            msg.reactions.push(Reaction {
                emoji: req.emoji.clone(),
                user_ids: vec![user_id],
            });
        }
    }

    // emoji parse for event
    let (emoji_name, emoji_id) = if req.emoji.contains(':') {
        let mut parts = req.emoji.splitn(2, ':');
        let name = parts.next().unwrap_or("").to_string();
        let id = parts.next().map(|s| s.to_string());
        (name, id)
    } else {
        (req.emoji.clone(), None)
    };

    state.publish(
        "MESSAGE_REACTION_ADD",
        json!({
            "user_id": user_id,
            "channel_id": channel_id,
            "message_id": message_id,
            "guild_id": guild_id,
            "emoji": { "id": emoji_id, "name": emoji_name },
        }),
    );

    let cursor = {
        let w = state.world();
        w.action_head()
    };
    {
        let mut w = state.world();
        w.cursor_mark = cursor;
    }

    Ok(Json(json!({
        "message_id": message_id,
        "cursor": cursor,
    })))
}

/// `GET /_fake/log`
pub async fn log(State(state): State<AppState>, Query(query): Query<LogQuery>) -> HandlerResult {
    let channel_filter = if let Some(c) = &query.channel {
        let w = state.world();
        match find_channel_id(&w, c) {
            Some(id) => Some(id),
            None => return Err(err(StatusCode::NOT_FOUND, "チャンネルが見つかりません")),
        }
    } else {
        None
    };

    let limit = query.limit.unwrap_or(20);

    let messages_json = {
        let w = state.world();
        let mut msgs: Vec<(&Id, &crate::state::Message)> = w
            .messages
            .iter()
            .filter(|(_, m)| {
                if let Some(cid) = channel_filter {
                    m.channel_id == cid
                } else {
                    true
                }
            })
            .collect();
        // BTreeMap は id 順 (古い順)
        if msgs.len() > limit {
            let skip = msgs.len() - limit;
            msgs = msgs[skip..].to_vec();
        }
        msgs.iter()
            .map(|(_, m)| w.message_to_discord_json(m, false))
            .collect::<Vec<_>>()
    };

    let actions = {
        let w = state.world();
        let mut acts: Vec<BotAction> = w
            .bot_actions
            .iter()
            .filter(|a| {
                if let Some(since) = query.since
                    && a.seq <= since
                {
                    return false;
                }
                if let Some(cid) = channel_filter {
                    match action_channel(&w, a) {
                        Some(ac) => ac == cid,
                        None => false,
                    }
                } else {
                    true
                }
            })
            .cloned()
            .collect();
        acts.sort_by_key(|a| a.seq);
        acts
    };

    let cursor = {
        let w = state.world();
        w.action_head()
    };

    Ok(Json(json!({
        "messages": messages_json,
        "actions": actions,
        "cursor": cursor,
    })))
}

/// `POST /_fake/wait-reply`
pub async fn wait_reply(
    State(state): State<AppState>,
    Json(req): Json<WaitReplyRequest>,
) -> HandlerResult {
    let since = req.since.unwrap_or_else(|| {
        let w = state.world();
        w.cursor_mark
    });
    let timeout_secs = req.timeout_secs.unwrap_or(60);
    let settle_ms = req.settle_ms.unwrap_or(2000);
    let channel_filter = if let Some(c) = &req.channel {
        let w = state.world();
        match find_channel_id(&w, c) {
            Some(id) => Some(id),
            None => return Err(err(StatusCode::NOT_FOUND, "チャンネルが見つかりません")),
        }
    } else {
        None
    };

    let start = Instant::now();
    let timeout_dur = Duration::from_secs(timeout_secs);

    // timeout 0: 待たずに返す
    if timeout_secs == 0 {
        let (actions, replied) = collect_actions(&state, since, channel_filter);
        let cursor = actions.last().map(|a| a.seq).unwrap_or(since);
        return Ok(Json(json!({
            "timed_out": !replied,
            "replied": replied,
            "actions": actions,
            "cursor": cursor,
        })));
    }

    // ポーリング
    loop {
        let (actions, has_reply) = collect_actions(&state, since, channel_filter);
        if has_reply {
            // 新しい操作が settle_ms のあいだ来なくなるまで待つ(seq は操作ごとに増えるので末尾 seq だけ見ればよい)
            let mut last_change = Instant::now();
            let mut last_seq = actions.last().map(|a| a.seq).unwrap_or(since);
            while last_change.elapsed() < Duration::from_millis(settle_ms) {
                if start.elapsed() >= timeout_dur {
                    break;
                }
                sleep(Duration::from_millis(50)).await;
                let (new_actions, _) = collect_actions(&state, since, channel_filter);
                let new_seq = new_actions.last().map(|a| a.seq).unwrap_or(since);
                if new_seq != last_seq {
                    last_seq = new_seq;
                    last_change = Instant::now();
                }
            }
            // 最終取得
            let (final_actions, _) = collect_actions(&state, since, channel_filter);
            let cursor = final_actions.last().map(|a| a.seq).unwrap_or(since);
            return Ok(Json(json!({
                "timed_out": false,
                "replied": true,
                "actions": final_actions,
                "cursor": cursor,
            })));
        }
        if start.elapsed() >= timeout_dur {
            let cursor = actions.last().map(|a| a.seq).unwrap_or(since);
            return Ok(Json(json!({
                "timed_out": true,
                "replied": false,
                "actions": actions,
                "cursor": cursor,
            })));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn collect_actions(
    state: &AppState,
    since: u64,
    channel_filter: Option<Id>,
) -> (Vec<BotAction>, bool) {
    let w = state.world();
    let mut acts: Vec<BotAction> = w
        .bot_actions
        .iter()
        .filter(|a| {
            if a.seq <= since {
                return false;
            }
            if let Some(cid) = channel_filter {
                match action_channel(&w, a) {
                    Some(ac) => ac == cid,
                    None => false,
                }
            } else {
                true
            }
        })
        .cloned()
        .collect();
    acts.sort_by_key(|a| a.seq);
    let has_reply = acts.iter().any(is_reply_action);
    (acts, has_reply)
}

/// `POST /_fake/users`
pub async fn add_user(
    State(state): State<AppState>,
    Json(req): Json<UserAddRequest>,
) -> HandlerResult {
    if req.name.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "name が空です"));
    }
    let role = req.role.as_deref().unwrap_or("allowed");
    if !["allowed", "master", "none"].contains(&role) {
        return Err(err(StatusCode::BAD_REQUEST, "role が不正です"));
    }

    // 重複チェック
    {
        let w = state.world();
        let exists = w
            .users
            .values()
            .any(|u| u.username.eq_ignore_ascii_case(&req.name));
        if exists {
            return Err(err(StatusCode::BAD_REQUEST, "同じ名前のユーザーがいます"));
        }
    }

    let guild_id = {
        let w = state.world();
        primary_guild_id(&w)
    };

    let allowed_role_id = {
        let w = state.world();
        role_id_by_name(&w, guild_id, ROLE_ALLOWED)
    };
    let master_role_id = {
        let w = state.world();
        role_id_by_name(&w, guild_id, ROLE_MASTER)
    };

    let roles = match role {
        "none" => Vec::new(),
        "allowed" => allowed_role_id.into_iter().collect(),
        "master" => {
            let mut v = Vec::new();
            if let Some(a) = allowed_role_id {
                v.push(a);
            }
            if let Some(m) = master_role_id {
                v.push(m);
            }
            v
        }
        _ => Vec::new(),
    };

    let (user_json, member_json) = {
        let mut w = state.world();
        let user_id = w.add_user(&req.name, req.bot);
        // user の bot フラグは add_user で設定済み
        let joined_at = crate::state::now_rfc3339();
        let member = Member {
            guild_id,
            user_id,
            nick: None,
            roles: roles.clone(),
            joined_at,
        };
        w.members.push(member.clone());

        let uj = w.users.get(&user_id).unwrap().to_discord_json();
        let mj = w.member_to_discord_json(&member);
        state.publish("GUILD_MEMBER_ADD", {
            let mut v = mj.clone();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("guild_id".to_string(), json!(guild_id));
            }
            v
        });
        (uj, mj)
    };

    Ok(Json(json!({
        "user": user_json,
        "member": member_json,
    })))
}

/// `POST /_fake/snapshot`
pub async fn save_snapshot(
    State(state): State<AppState>,
    Json(req): Json<SnapshotRequest>,
) -> HandlerResult {
    let path_str = if let Some(p) = req.path {
        p
    } else {
        let w = state.world();
        match &w.snapshot_path {
            Some(p) => p.to_string_lossy().to_string(),
            None => return Err(err(StatusCode::BAD_REQUEST, "保存先が指定されていません")),
        }
    };
    let path = std::path::PathBuf::from(&path_str);
    let abs = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    if let Err(e) = state.save_snapshot(&abs) {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("保存に失敗しました: {e}"),
        ));
    }
    Ok(Json(json!({ "path": abs.to_string_lossy() })))
}

/// `GET /_fake/channels`(名前解決の確認用。全チャンネルとスレッド)
pub async fn list_channels(State(state): State<AppState>) -> HandlerResult {
    let channels = {
        let w = state.world();
        w.channels
            .values()
            .map(|c| c.to_discord_json())
            .collect::<Vec<_>>()
    };
    Ok(Json(json!({ "channels": channels })))
}

/// `GET /_fake/users`
pub async fn list_users(State(state): State<AppState>) -> HandlerResult {
    let users = {
        let w = state.world();
        let guild_id = primary_guild_id(&w);
        w.users
            .values()
            .map(|u| {
                let mut v = u.to_discord_json();
                // roles
                let member = w
                    .members
                    .iter()
                    .find(|m| m.guild_id == guild_id && m.user_id == u.id);
                let role_names: Vec<String> = member
                    .map(|m| {
                        m.roles
                            .iter()
                            .filter_map(|rid| w.roles.get(rid))
                            .filter(|r| r.name != "@everyone")
                            .map(|r| r.name.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("roles".to_string(), json!(role_names));
                }
                v
            })
            .collect::<Vec<_>>()
    };
    Ok(Json(json!({ "users": users })))
}

/// `GET /_fake/status`(起動確認。guild とカーソルの要約)
pub async fn status(State(state): State<AppState>) -> HandlerResult {
    let (guild_id, app_id, bot_id, port, cursor, channels, messages) = {
        let w = state.world();
        (
            primary_guild_id(&w),
            w.application_id,
            w.bot_user_id,
            w.listen_port,
            w.action_head(),
            w.channels.len(),
            w.messages.len(),
        )
    };
    Ok(Json(json!({
        "guild_id": guild_id,
        "application_id": app_id,
        "bot_user_id": bot_id,
        "port": port,
        "cursor": cursor,
        "channels": channels,
        "messages": messages,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, World};

    fn state() -> AppState {
        AppState::new(World::seed())
    }

    #[tokio::test]
    async fn send_creates_message_and_event() {
        let st = state();
        let mut rx = st.events.subscribe();
        let req = SendRequest {
            channel: "general".into(),
            as_user: None,
            content: "hello @noth".into(),
            reply_to: None,
        };
        let res = send(State(st.clone()), Json(req)).await.unwrap();
        let v = res.0;
        assert!(v["message"]["content"].as_str().unwrap().contains("<@"));
        // event
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "MESSAGE_CREATE");
        // state
        let w = st.world();
        assert_eq!(w.messages.len(), 1);
        assert_eq!(w.cursor_mark, 0);
    }

    #[tokio::test]
    async fn channel_create_rename_delete() {
        let st = state();
        let mut rx = st.events.subscribe();
        // create
        let req = ChannelCreateRequest {
            name: "Random Room".into(),
            parent: None,
        };
        let res = create_channel(State(st.clone()), Json(req))
            .await
            .unwrap()
            .0;
        let name = res["channel"]["name"].as_str().unwrap();
        assert_eq!(name, "random-room");
        let _ = rx.try_recv().unwrap(); // CHANNEL_CREATE

        // rename
        let ch_id = res["channel"]["id"].as_str().unwrap().to_string();
        let req = ChannelRenameRequest {
            name: "random-2".into(),
        };
        let res = rename_channel(State(st.clone()), Path(ch_id.clone()), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(res["channel"]["name"].as_str().unwrap(), "random-2");
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "CHANNEL_UPDATE");

        // delete
        let res = delete_channel(State(st.clone()), Path(ch_id))
            .await
            .unwrap()
            .0;
        assert!(res["deleted"].is_object());
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "CHANNEL_DELETE");
    }

    #[tokio::test]
    async fn category_create() {
        let st = state();
        let mut rx = st.events.subscribe();
        let req = CategoryCreateRequest {
            name: "雑談".into(),
        };
        let res = create_category(State(st.clone()), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(res["channel"]["type"], 4);
        assert_eq!(res["channel"]["name"], "雑談");
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "CHANNEL_CREATE");
    }

    #[tokio::test]
    async fn thread_create() {
        let st = state();
        let mut rx = st.events.subscribe();
        let req = ThreadCreateRequest {
            channel: "general".into(),
            name: "相談".into(),
            message_id: None,
            as_user: None,
        };
        let res = create_thread(State(st.clone()), Json(req)).await.unwrap().0;
        assert_eq!(res["channel"]["type"], 11);
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "THREAD_CREATE");
        assert_eq!(ev.data["newly_created"], true);
    }

    #[tokio::test]
    async fn thread_list_sync_includes_active_threads() {
        let st = state();
        let mk = |name: &str| ThreadCreateRequest {
            channel: "general".into(),
            name: name.into(),
            message_id: None,
            as_user: None,
        };
        let _ = create_thread(State(st.clone()), Json(mk("a")))
            .await
            .unwrap();
        let _ = create_thread(State(st.clone()), Json(mk("b")))
            .await
            .unwrap();
        // アーカイブ済みは含めない
        let archived = {
            let mut w = st.world();
            let id = w.find_channel("b").unwrap().id;
            w.channels.get_mut(&id).unwrap().archived = true;
            id
        };
        let mut rx = st.events.subscribe();
        let res = thread_list_sync(State(st.clone()), Json(ThreadListSyncRequest::default()))
            .await
            .unwrap()
            .0;
        assert_eq!(res["threads"], 1);
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "THREAD_LIST_SYNC");
        // serenity の ThreadListSyncEvent として読める(guild_id / threads / members は必須)
        let sync: serenity::model::event::ThreadListSyncEvent =
            serde_json::from_value(ev.data).unwrap();
        assert_eq!(sync.threads.len(), 1);
        assert_eq!(sync.members.len(), 1);
        assert_ne!(sync.threads[0].id.get(), archived.0);
        assert!(sync.channel_ids.is_none());
    }

    #[tokio::test]
    async fn thread_list_sync_by_channel() {
        let st = state();
        let _ = create_thread(
            State(st.clone()),
            Json(ThreadCreateRequest {
                channel: "general".into(),
                name: "a".into(),
                message_id: None,
                as_user: None,
            }),
        )
        .await
        .unwrap();
        let mut rx = st.events.subscribe();
        let req = ThreadListSyncRequest {
            channel: Some("general".into()),
        };
        let _ = thread_list_sync(State(st.clone()), Json(req))
            .await
            .unwrap();
        let ev = rx.try_recv().unwrap();
        let sync: serenity::model::event::ThreadListSyncEvent =
            serde_json::from_value(ev.data).unwrap();
        assert_eq!(sync.channel_ids.unwrap().len(), 1);
        // 存在しないチャンネルは 404
        let req = ThreadListSyncRequest {
            channel: Some("nope".into()),
        };
        let res = thread_list_sync(State(st), Json(req)).await;
        assert_eq!(res.unwrap_err().0, StatusCode::NOT_FOUND);
    }

    /// noth が登録する `/noth ignore add <channel>` と同じ形の定義を入れる
    fn register_noth(st: &AppState) {
        let mut w = st.world();
        let id = w.alloc_id();
        let application_id = w.application_id;
        w.commands.push(crate::state::RegisteredCommand {
            id,
            application_id,
            guild_id: None,
            name: "noth".into(),
            definition: json!({
                "name": "noth",
                "options": [{
                    "name": "ignore", "type": 2,
                    "options": [{
                        "name": "add", "type": 1,
                        "options": [{ "name": "channel", "type": 7, "required": true }],
                    }],
                }],
            }),
        });
    }

    #[tokio::test]
    async fn slash_resolves_channel_options_by_declared_type() {
        let st = state();
        register_noth(&st);
        let mut rx = st.events.subscribe();
        let general = st
            .world()
            .channels
            .values()
            .find(|c| c.name == "general")
            .unwrap()
            .id;
        let req = SlashRequest {
            name: "noth".into(),
            path: vec!["ignore".into(), "add".into()],
            options: vec![SlashOption {
                name: "channel".into(),
                value: json!("general"),
            }],
            as_user: None,
            channel: None,
        };
        let _ = slash(State(st.clone()), Json(req)).await.unwrap();
        let ev = rx.try_recv().unwrap();
        let arg = &ev.data["data"]["options"][0]["options"][0]["options"][0];
        assert_eq!(arg["type"], 7);
        assert_eq!(arg["value"], json!(general));
        assert_eq!(
            ev.data["data"]["resolved"]["channels"][general.to_string()]["name"],
            "general"
        );

        // noth と同じく、serenity がチャンネル型の値として読めること
        let ix: serenity::model::application::CommandInteraction =
            serde_json::from_value(ev.data.clone()).unwrap();
        let group = &ix.data.options[0].value;
        let serenity::model::application::CommandDataOptionValue::SubCommandGroup(subs) = group
        else {
            panic!("group ではない: {group:?}");
        };
        let serenity::model::application::CommandDataOptionValue::SubCommand(args) = &subs[0].value
        else {
            panic!("subcommand ではない");
        };
        assert!(matches!(
            args[0].value,
            serenity::model::application::CommandDataOptionValue::Channel(id) if id.get() == general.0
        ));
    }

    #[tokio::test]
    async fn slash_rejects_unregistered_commands_and_unknown_channels() {
        let st = state();
        let req = |name: &str, ch: &str| SlashRequest {
            name: name.into(),
            path: vec!["ignore".into(), "add".into()],
            options: vec![SlashOption {
                name: "channel".into(),
                value: json!(ch),
            }],
            as_user: None,
            channel: None,
        };
        assert!(
            slash(State(st.clone()), Json(req("noth", "general")))
                .await
                .is_err()
        );
        register_noth(&st);
        assert!(
            slash(State(st.clone()), Json(req("noth", "no-such-channel")))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn slash_and_click() {
        let st = state();
        register_noth(&st);
        let mut rx = st.events.subscribe();
        // slash
        let req = SlashRequest {
            name: "noth".into(),
            path: vec!["ignore".into(), "add".into()],
            options: vec![SlashOption {
                name: "channel".into(),
                value: json!("general"),
            }],
            as_user: None,
            channel: None,
        };
        let res = slash(State(st.clone()), Json(req)).await.unwrap().0;
        let iid = res["interaction_id"].as_str().unwrap().to_string();
        assert_eq!(res["token"].as_str().unwrap().len(), 64);
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "INTERACTION_CREATE");
        assert_eq!(ev.data["data"]["name"], "noth");

        // click: create a message with components containing custom_id
        let msg_id = {
            let mut w = st.world();
            let id = w.alloc_id();
            let ch = w.channels.values().next().unwrap().id;
            let user = w.users.values().next().unwrap().id;
            let msg = crate::state::Message {
                id,
                channel_id: ch,
                author_id: user,
                content: "btn".into(),
                timestamp: crate::state::now_rfc3339(),
                edited_timestamp: None,
                kind: 0,
                message_reference: None,
                mentions: Vec::new(),
                components: vec![
                    json!({"type":2,"custom_id":"noth:refresh:context","style":1,"label":"x"}),
                ],
                flags: 0,
                reactions: Vec::new(),
                pinned: false,
                interaction_id: None,
            };
            w.messages.insert(id, msg);
            id
        };
        let req = ClickRequest {
            custom_id: "noth:refresh:context".into(),
            message_id: Some(msg_id),
            channel: None,
            as_user: None,
        };
        let res = click(State(st.clone()), Json(req)).await.unwrap().0;
        assert_ne!(res["interaction_id"].as_str().unwrap(), iid);
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "INTERACTION_CREATE");
        assert_eq!(ev.data["data"]["custom_id"], "noth:refresh:context");
    }

    #[tokio::test]
    async fn react_adds_reaction_and_event() {
        let st = state();
        let mut rx = st.events.subscribe();
        // create a message
        let msg_id = {
            let mut w = st.world();
            let id = w.alloc_id();
            let ch = w.channels.values().next().unwrap().id;
            let user = w.users.values().next().unwrap().id;
            let msg = crate::state::Message {
                id,
                channel_id: ch,
                author_id: user,
                content: "hi".into(),
                timestamp: crate::state::now_rfc3339(),
                edited_timestamp: None,
                kind: 0,
                message_reference: None,
                mentions: Vec::new(),
                components: Vec::new(),
                flags: 0,
                reactions: Vec::new(),
                pinned: false,
                interaction_id: None,
            };
            w.messages.insert(id, msg);
            id
        };
        let req = ReactRequest {
            emoji: "👍".into(),
            message_id: Some(msg_id),
            channel: None,
            as_user: None,
        };
        let res = react(State(st.clone()), Json(req)).await.unwrap().0;
        assert_eq!(res["message_id"].as_str().unwrap(), msg_id.to_string());
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "MESSAGE_REACTION_ADD");
        let w = st.world();
        let msg = w.messages.get(&msg_id).unwrap();
        assert_eq!(msg.reactions.len(), 1);
        assert_eq!(msg.reactions[0].user_ids.len(), 1);
    }

    #[tokio::test]
    async fn users_add_and_list() {
        let st = state();
        let mut rx = st.events.subscribe();
        let req = UserAddRequest {
            name: "alice".into(),
            role: Some("allowed".into()),
            bot: false,
        };
        let res = add_user(State(st.clone()), Json(req)).await.unwrap().0;
        assert_eq!(res["user"]["username"], "alice");
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "GUILD_MEMBER_ADD");

        // duplicate
        let req = UserAddRequest {
            name: "alice".into(),
            role: None,
            bot: false,
        };
        let r = add_user(State(st.clone()), Json(req)).await;
        assert!(r.is_err());

        // list
        let res = list_users(State(st.clone())).await.unwrap().0;
        let arr = res["users"].as_array().unwrap();
        assert!(arr.iter().any(|u| u["username"] == "alice"));
    }

    #[tokio::test]
    async fn log_and_status() {
        let st = state();
        // send a message
        let req = SendRequest {
            channel: "general".into(),
            as_user: None,
            content: "hi".into(),
            reply_to: None,
        };
        let _ = send(State(st.clone()), Json(req)).await.unwrap();
        let res = log(
            State(st.clone()),
            Query(LogQuery {
                channel: Some("general".into()),
                limit: Some(10),
                since: None,
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(!res["messages"].as_array().unwrap().is_empty());
        let res = status(State(st.clone())).await.unwrap().0;
        assert!(res["channels"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn wait_reply_timeout() {
        let st = state();
        let req = WaitReplyRequest {
            since: Some(0),
            timeout_secs: Some(0),
            channel: None,
            settle_ms: None,
        };
        let res = wait_reply(State(st.clone()), Json(req)).await.unwrap().0;
        assert_eq!(res["timed_out"], true);
        assert_eq!(res["replied"], false);
    }

    #[tokio::test]
    async fn snapshot_writes_file() {
        let st = state();
        let path = std::env::temp_dir().join("fakecord_test_snap.json");
        let _ = std::fs::remove_file(&path);
        let req = SnapshotRequest {
            path: Some(path.to_string_lossy().to_string()),
        };
        let res = save_snapshot(State(st.clone()), Json(req)).await.unwrap().0;
        assert!(std::path::Path::new(res["path"].as_str().unwrap()).exists());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn list_channels_works() {
        let st = state();
        let res = list_channels(State(st.clone())).await.unwrap().0;
        assert!(!res["channels"].as_array().unwrap().is_empty());
    }
}
