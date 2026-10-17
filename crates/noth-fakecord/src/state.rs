//! fakecord が持つ状態の型。サーバー全体で共有する契約なので、型の追加や変更は統合担当だけが行う
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use tokio::sync::{broadcast, watch};

/// チャンネル種別(Discord の type)
pub const CHANNEL_TEXT: u8 = 0;
pub const CHANNEL_CATEGORY: u8 = 4;
pub const CHANNEL_PUBLIC_THREAD: u8 = 11;

/// interaction 種別(Discord の type)
pub const INTERACTION_COMMAND: u8 = 2;
pub const INTERACTION_COMPONENT: u8 = 3;

/// メッセージ flags
pub const MESSAGE_FLAG_EPHEMERAL: u64 = 1 << 6;
pub const MESSAGE_FLAG_COMPONENTS_V2: u64 = 1 << 15;

/// 通知を受ける役の名前。`World::seed` が作る
pub const ROLE_ALLOWED: &str = "allowed";
pub const ROLE_MASTER: &str = "master";

/// Discord epoch(2015-01-01T00:00:00Z)のミリ秒
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// snowflake。JSON では常に文字列で出し、読むときは文字列と数値のどちらも受ける
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub u64);

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Id {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(Id)
    }
}

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdVisitor;

        impl Visitor<'_> for IdVisitor {
            type Value = Id;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("snowflake(文字列または整数)")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Id, E> {
                Ok(Id(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Id, E> {
                u64::try_from(v).map(Id).map_err(E::custom)
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Id, E> {
                v.parse().map(Id).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(IdVisitor)
    }
}

/// 現在時刻を RFC 3339(UTC)で返す。メッセージの timestamp などに使う
pub fn now_rfc3339() -> String {
    jiff::Timestamp::now().to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Guild {
    pub id: Id,
    pub name: String,
    pub owner_id: Id,
}

/// kind は 0 テキスト / 4 カテゴリ / 11 公開スレッド。スレッドの parent_id は親テキストチャンネル
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Channel {
    pub id: Id,
    pub guild_id: Id,
    pub kind: u8,
    pub name: String,
    pub parent_id: Option<Id>,
    pub position: i32,
    #[serde(default)]
    pub topic: Option<String>,
    /// スレッドの作成者。スレッド以外は None
    #[serde(default)]
    pub owner_id: Option<Id>,
    /// スレッドのアーカイブ状態。スレッド以外は常に false
    #[serde(default)]
    pub archived: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct User {
    pub id: Id,
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    pub bot: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Role {
    pub id: Id,
    pub guild_id: Id,
    pub name: String,
    pub position: i32,
    /// 権限ビットを 10 進文字列で持つ(Discord と同じ)
    pub permissions: String,
    pub color: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub guild_id: Id,
    pub user_id: Id,
    #[serde(default)]
    pub nick: Option<String>,
    /// @everyone(= guild の id)は含めない
    pub roles: Vec<Id>,
    pub joined_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageReference {
    pub message_id: Option<Id>,
    pub channel_id: Option<Id>,
    pub guild_id: Option<Id>,
}

/// emoji は Unicode ならそのまま、カスタムなら `name:id`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reaction {
    pub emoji: String,
    pub user_ids: Vec<Id>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: Id,
    pub channel_id: Id,
    pub author_id: Id,
    pub content: String,
    pub timestamp: String,
    pub edited_timestamp: Option<String>,
    /// Discord の type。0 通常、19 返信
    pub kind: u8,
    pub message_reference: Option<MessageReference>,
    /// メンションされたユーザー
    #[serde(default)]
    pub mentions: Vec<Id>,
    /// Components V2 を含む生 JSON をそのまま持つ
    #[serde(default)]
    pub components: Vec<Value>,
    pub flags: u64,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    #[serde(default)]
    pub pinned: bool,
    /// interaction の応答として作られたメッセージなら、その interaction
    #[serde(default)]
    pub interaction_id: Option<Id>,
}

/// 応答状態。callback の type 5/6 は Deferred、4/7 と編集は Responded
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionState {
    Pending,
    Deferred,
    Responded,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Interaction {
    pub id: Id,
    pub token: String,
    pub application_id: Id,
    pub guild_id: Id,
    pub channel_id: Id,
    pub user_id: Id,
    /// 2 = アプリケーションコマンド、3 = メッセージコンポーネント
    pub kind: u8,
    /// INTERACTION_CREATE の `data` をそのまま持つ
    pub data: Value,
    /// コンポーネント操作のとき、押されたボタンのあるメッセージ
    #[serde(default)]
    pub message_id: Option<Id>,
    pub state: InteractionState,
    /// 初回応答(または @original)が作ったメッセージ
    #[serde(default)]
    pub original_message_id: Option<Id>,
    pub created_at: String,
}

/// `PUT /applications/{app}/guilds/{guild}/commands` で Bot が登録したコマンド
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegisteredCommand {
    pub id: Id,
    pub application_id: Id,
    pub guild_id: Option<Id>,
    pub name: String,
    /// 登録時にもらった定義の生 JSON(options を含む)
    pub definition: Value,
}

/// Bot が REST で行った操作。wait-reply と log がこれを読む
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BotAction {
    /// 1 から始まる通し番号。カーソルとして使う
    pub seq: u64,
    pub at: String,
    #[serde(flatten)]
    pub kind: BotActionKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BotActionKind {
    /// `POST /channels/{id}/messages`
    Post {
        channel_id: Id,
        message_id: Id,
        content: String,
        reply_to: Option<Id>,
    },
    /// `PATCH /channels/{id}/messages/{mid}`
    Edit {
        channel_id: Id,
        message_id: Id,
        content: Option<String>,
    },
    /// `PUT .../reactions/{emoji}/@me`
    Reaction {
        channel_id: Id,
        message_id: Id,
        emoji: String,
    },
    /// `POST /channels/{id}/typing`
    Typing { channel_id: Id },
    /// `POST /interactions/{id}/{token}/callback`。response_type は callback の type
    InteractionResponse {
        interaction_id: Id,
        response_type: u8,
        message_id: Option<Id>,
        content: Option<String>,
    },
    /// `PATCH /webhooks/{app}/{token}/messages/@original`
    InteractionEdit {
        interaction_id: Id,
        message_id: Id,
        content: Option<String>,
    },
    /// `POST /webhooks/{app}/{token}`
    InteractionFollowup {
        interaction_id: Id,
        message_id: Id,
        content: Option<String>,
    },
}

impl BotActionKind {
    /// 操作の対象チャンネル。interaction 系はメッセージが無ければ None
    pub fn channel_id(&self) -> Option<Id> {
        match self {
            Self::Post { channel_id, .. }
            | Self::Edit { channel_id, .. }
            | Self::Reaction { channel_id, .. }
            | Self::Typing { channel_id } => Some(*channel_id),
            Self::InteractionResponse { .. }
            | Self::InteractionEdit { .. }
            | Self::InteractionFollowup { .. } => None,
        }
    }
}

/// 擬似サーバーの全状態。スナップショットはこれを JSON にしたもの
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct World {
    /// アプリケーション ID。Discord と同じく bot ユーザーの ID と同じ値にする
    pub application_id: Id,
    pub bot_user_id: Id,
    pub guilds: BTreeMap<Id, Guild>,
    pub channels: BTreeMap<Id, Channel>,
    pub users: BTreeMap<Id, User>,
    pub roles: BTreeMap<Id, Role>,
    pub members: Vec<Member>,
    /// 全チャンネルのメッセージ。ID は単調増加なので、順序は ID の順と同じ
    pub messages: BTreeMap<Id, Message>,
    pub interactions: BTreeMap<Id, Interaction>,
    pub commands: Vec<RegisteredCommand>,
    pub bot_actions: Vec<BotAction>,
    /// 操作用 API(send / slash / click / react)が最後に実行した時点の `bot_actions` の末尾 seq。
    /// wait-reply で since を省略したときの既定値
    pub cursor_mark: u64,
    /// 最後に採番した ID。次は必ずこれより大きくする
    pub last_id: u64,
    /// 待ち受けポート。起動時に `serve` が入れる(`GET /gateway` の url に使う)
    #[serde(skip)]
    pub listen_port: u16,
    /// `serve --snapshot` のパス。終了時と `/_fake/snapshot` の既定の保存先
    #[serde(skip)]
    pub snapshot_path: Option<PathBuf>,
}

impl Default for World {
    fn default() -> Self {
        Self::seed()
    }
}

impl World {
    /// 初期状態。guild 1 つ、#general、bot ユーザー(noth)、kurazu、allowed/master 役
    pub fn seed() -> Self {
        let mut world = World {
            application_id: Id(0),
            bot_user_id: Id(0),
            guilds: BTreeMap::new(),
            channels: BTreeMap::new(),
            users: BTreeMap::new(),
            roles: BTreeMap::new(),
            members: Vec::new(),
            messages: BTreeMap::new(),
            interactions: BTreeMap::new(),
            commands: Vec::new(),
            bot_actions: Vec::new(),
            cursor_mark: 0,
            last_id: 0,
            listen_port: 0,
            snapshot_path: None,
        };

        let guild_id = world.alloc_id();
        let bot = world.add_user("noth", true);
        let kurazu = world.add_user("kurazu", false);
        world.bot_user_id = bot;
        world.application_id = bot;
        world.guilds.insert(
            guild_id,
            Guild {
                id: guild_id,
                name: "noth-dev".into(),
                owner_id: kurazu,
            },
        );

        // @everyone は guild と同じ ID の役
        world.roles.insert(
            guild_id,
            Role {
                id: guild_id,
                guild_id,
                name: "@everyone".into(),
                position: 0,
                permissions: "1071698660929".into(),
                color: 0,
            },
        );
        let allowed = world.alloc_id();
        world.roles.insert(
            allowed,
            Role {
                id: allowed,
                guild_id,
                name: ROLE_ALLOWED.into(),
                position: 1,
                permissions: "0".into(),
                color: 0,
            },
        );
        let master = world.alloc_id();
        world.roles.insert(
            master,
            Role {
                id: master,
                guild_id,
                name: ROLE_MASTER.into(),
                position: 2,
                permissions: "0".into(),
                color: 0,
            },
        );

        let joined_at = now_rfc3339();
        world.members.push(Member {
            guild_id,
            user_id: bot,
            nick: None,
            roles: Vec::new(),
            joined_at: joined_at.clone(),
        });
        world.members.push(Member {
            guild_id,
            user_id: kurazu,
            nick: None,
            roles: vec![allowed, master],
            joined_at,
        });

        let general = world.alloc_id();
        world.channels.insert(
            general,
            Channel {
                id: general,
                guild_id,
                kind: CHANNEL_TEXT,
                name: "general".into(),
                parent_id: None,
                position: 0,
                topic: None,
                owner_id: None,
                archived: false,
            },
        );
        world
    }

    /// 時刻順に単調増加する snowflake 風の ID を採番する
    pub fn alloc_id(&mut self) -> Id {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let timed = ms.saturating_sub(DISCORD_EPOCH_MS) << 22;
        self.last_id = timed.max(self.last_id + 1);
        Id(self.last_id)
    }

    /// ユーザーを作って ID を返す。guild への参加はしない
    pub fn add_user(&mut self, username: &str, bot: bool) -> Id {
        let id = self.alloc_id();
        self.users.insert(
            id,
            User {
                id,
                username: username.into(),
                global_name: None,
                bot,
            },
        );
        id
    }

    /// 最初の guild。擬似サーバーは guild を 1 つしか持たない前提のハンドラが使う
    pub fn primary_guild(&self) -> Option<&Guild> {
        self.guilds.values().next()
    }

    pub fn member(&self, guild_id: Id, user_id: Id) -> Option<&Member> {
        self.members
            .iter()
            .find(|m| m.guild_id == guild_id && m.user_id == user_id)
    }

    /// ユーザー名(大文字小文字を区別しない)または ID 文字列でユーザーを探す
    pub fn find_user(&self, name_or_id: &str) -> Option<&User> {
        if let Ok(id) = name_or_id.parse::<Id>()
            && let Some(user) = self.users.get(&id)
        {
            return Some(user);
        }
        let name = name_or_id.trim_start_matches('@');
        self.users
            .values()
            .find(|u| u.username.eq_ignore_ascii_case(name))
    }

    /// チャンネル名(`#` は無視)または ID 文字列でチャンネルを探す。同名が複数あれば ID が小さい方
    pub fn find_channel(&self, name_or_id: &str) -> Option<&Channel> {
        if let Ok(id) = name_or_id.parse::<Id>()
            && let Some(channel) = self.channels.get(&id)
        {
            return Some(channel);
        }
        let name = name_or_id.trim_start_matches('#');
        self.channels.values().find(|c| c.name == name)
    }

    /// Bot の操作を記録して seq を返す
    pub fn record_action(&mut self, kind: BotActionKind) -> u64 {
        let seq = self.action_head() + 1;
        self.bot_actions.push(BotAction {
            seq,
            at: now_rfc3339(),
            kind,
        });
        seq
    }

    /// 記録済みの最後の seq。無ければ 0
    pub fn action_head(&self) -> u64 {
        self.bot_actions.last().map_or(0, |a| a.seq)
    }

    /// スナップショットを JSON で保存する。途中で壊れないよう一時ファイル経由で置き換える
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// スナップショットを読む。足りないフィールドは初期状態の値で埋まる
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)
    }
}

/// Gateway へ流すイベント。name は `MESSAGE_CREATE` など、data は `d` に入れる JSON
#[derive(Clone, Debug)]
pub struct GatewayEvent {
    pub name: String,
    pub data: Value,
}

/// Resume のために覚えておくイベントの件数(全体のログと、セッションごとの送信履歴の両方)
pub const EVENT_HISTORY: usize = 1000;

/// 配信したイベントの全体ログ。通番(1 から)付きのリングバッファ。
/// 切断中のセッションは誰も読まないので、Resume のときにここから取りこぼし分を拾う
#[derive(Debug, Default)]
pub struct EventLog {
    /// 最後に振った通番。まだ何も無ければ 0
    head: u64,
    ring: VecDeque<(u64, GatewayEvent)>,
}

impl EventLog {
    pub fn push(&mut self, event: GatewayEvent) -> u64 {
        self.head += 1;
        self.ring.push_back((self.head, event));
        while self.ring.len() > EVENT_HISTORY {
            self.ring.pop_front();
        }
        self.head
    }

    /// 最後の通番
    pub fn head(&self) -> u64 {
        self.head
    }
}

/// Gateway セッション。Identify で作り、Resume で別の接続が引き継ぐ。
/// 接続が切れても残るので、seq は接続をまたいで続き、送ったイベントは履歴から再送できる
#[derive(Debug)]
pub struct Session {
    /// 最後に振った seq
    pub seq: u64,
    /// 送った(送る予定の)ペイロード。(seq, 送信する JSON 文字列)
    history: VecDeque<(u64, String)>,
    /// 全体ログのうち、ここまでをセッションに取り込んだ通番
    cursor: u64,
    /// このセッションを持っている接続の札。Resume のたびに進め、古い接続を降ろす
    pub owner: u64,
    /// 全体ログから溢れてイベントを取りこぼしたときの seq(その直前まで)。これ以前からは Resume できない
    gap: Option<u64>,
}

impl Session {
    /// `cursor` はスナップショットを取った時点のログの末尾。それより後のイベントだけを流す
    pub fn new(cursor: u64) -> Self {
        Self {
            seq: 0,
            history: VecDeque::new(),
            cursor,
            owner: 1,
            gap: None,
        }
    }

    /// 次の seq でディスパッチを履歴へ積み、その seq を返す
    pub fn push(&mut self, name: &str, data: Value) -> u64 {
        self.seq += 1;
        let frame = serde_json::json!({"op": 0, "s": self.seq, "t": name, "d": data});
        self.history.push_back((self.seq, frame.to_string()));
        while self.history.len() > EVENT_HISTORY {
            self.history.pop_front();
        }
        self.seq
    }

    /// 全体ログの未取り込み分をセッションへ積む。ログから溢れて取りこぼしたときは、
    /// その位置を覚えておき(以前の seq からは Resume できなくなる)、残っている分だけ積む。
    /// 戻り値は取りこぼしが無かったか
    pub fn pull(&mut self, log: &EventLog) -> bool {
        let complete = log
            .ring
            .front()
            .is_none_or(|(oldest, _)| *oldest <= self.cursor + 1);
        if !complete {
            self.gap = Some(self.seq);
        }
        let start = self.cursor;
        for (n, event) in log.ring.iter().filter(|(n, _)| *n > start) {
            self.push(&event.name, event.data.clone());
            self.cursor = *n;
        }
        // ログが空でも、取り込み済みの位置は末尾に揃える
        self.cursor = self.cursor.max(log.head());
        complete
    }

    /// `after` より後のペイロードを返す
    pub fn frames_after(&self, after: u64) -> Vec<String> {
        self.history
            .iter()
            .filter(|(s, _)| *s > after)
            .map(|(_, f)| f.clone())
            .collect()
    }

    /// クライアントの最終 seq から再送できるか。未来の seq や、履歴から溢れた seq はできない
    pub fn can_resume_from(&self, seq: u64) -> bool {
        if seq > self.seq || self.gap.is_some_and(|gap| seq <= gap) {
            return false;
        }
        match self.history.front() {
            Some((oldest, _)) => *oldest <= seq + 1,
            None => seq == self.seq,
        }
    }
}

/// ハンドラ間で共有する状態。clone は安い
#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<Mutex<World>>,
    pub events: broadcast::Sender<GatewayEvent>,
    /// 終了の合図。`shutdown()` で true になり、Gateway の各セッションがこれを見て閉じる。
    /// プロセス全体の static にしないのは、1 プロセスで複数のサーバーを立てても干渉させないため
    pub shutdown: watch::Sender<bool>,
    /// 発行済みの Gateway セッション。Resume がこれを参照する(サーバーごと)。
    /// ロックの順序は World → sessions → log。どれも `.await` をまたいで持たない
    pub sessions: Arc<Mutex<HashMap<String, Session>>>,
    /// 配信したイベントの全体ログ
    pub log: Arc<Mutex<EventLog>>,
    /// ログの末尾の通番。Gateway の接続がこれを見て、新しいイベントを取りに行く
    pub head: watch::Sender<u64>,
}

impl AppState {
    pub fn new(world: World) -> Self {
        let (events, _) = broadcast::channel(1024);
        let (shutdown, _) = watch::channel(false);
        let (head, _) = watch::channel(0);
        Self {
            inner: Arc::new(Mutex::new(world)),
            events,
            shutdown,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            log: Arc::new(Mutex::new(EventLog::default())),
            head,
        }
    }

    /// 終了を知らせる。接続中の WebSocket が閉じ、graceful shutdown が完了できるようになる
    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// World を借りる。`.await` をまたいで保持しないこと。毒化していても中身を使う
    pub fn world(&self) -> MutexGuard<'_, World> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Gateway セッションの表を借りる。`.await` をまたいで保持しないこと
    pub fn sessions(&self) -> MutexGuard<'_, HashMap<String, Session>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// イベントの全体ログを借りる。`.await` をまたいで保持しないこと
    pub fn event_log(&self) -> MutexGuard<'_, EventLog> {
        self.log.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Gateway の接続全員へイベントを流す。受け手がいなくても失敗扱いにしない。
    /// 切断中のセッションが Resume で拾えるよう、全体ログにも残す
    pub fn publish(&self, name: &str, data: Value) {
        let event = GatewayEvent {
            name: name.into(),
            data,
        };
        // ログへの追記と通知を同じロックの中で行い、通番の順序を保つ
        let mut log = self.event_log();
        let head = log.push(event.clone());
        let _ = self.events.send(event);
        self.head.send_replace(head);
    }

    pub fn save_snapshot(&self, path: &Path) -> std::io::Result<()> {
        self.world().save(path)
    }
}
