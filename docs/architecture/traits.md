# Noth アーキテクチャ: Core の公開 trait

本書は `noth-core` が定義し、module が実装する拡張点(`Channel` / `Router` / `Provider` / `Tool` / `Compactor`)を定義します。シグネチャは意図を示すための骨子で、実装時にエラー型の詳細を詰めます。どの module がどの trait を実装するかは [modules.md](modules.md)、Provider のキャッシュ戦略は [caching.md](caching.md)、Tool の承認ポリシーは [security.md](security.md) を参照してください。

trait は `Box<dyn Trait>` で Module Registry に登録するため、非同期メソッドは `async-trait` クレートで定義します(素の `async fn in trait` は `dyn` にできず、`Send` も保証されないため)。

## 1. 共通型

| 型 | 内容 |
|---|---|
| `Member` | `user_id`、表示名、ロール名の一覧 |
| `IncomingMessage` | `channel_id`、`message_id`、`user_id`、本文(`content` のみ)、時刻、`is_self` |
| `Turn` | Layer 3 / 4 の1ブロック。`role`(user / assistant)、本文、種別(バッファ / トリガー / 応答 / 結論メモ / 関連チャンク)、`chunk_id` |
| `RouterState` | [context-layers.md §4.1](../context-layers.md#41-state-の組み立て) の JSON |
| `CompletionParams` | `max_tokens`、effort などモデル依存の生成パラメータ |
| `ToolContext` | `run_id`、`task_category`、作業ディレクトリ、承認ポリシーへの参照 |

## 2. `Channel`

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    /// 接続し、受信した発言を Event Bus に流す。allowed_users 以外と Bot の発言はここで捨てる
    async fn run(&self, bus: BusHandle) -> Result<()>;
    /// 発言を投稿する。2,000 文字を超えれば分割し、最後の MessageId を返す。
    /// reply_to があれば元発言へのリプライとして投稿する。allowed_mentions は常に空
    async fn send(&self, channel_id: ChannelId, text: &str, reply_to: Option<MessageId>) -> Result<MessageId>;
    /// 生成中の typing インジケータ
    async fn typing(&self, channel_id: ChannelId) -> Result<()>;
    /// Layer 1 に固定するメンバー情報。起動時に1回だけ呼ぶ
    async fn members(&self) -> Result<Vec<Member>>;
}
```

`channel-discord` が読むのはメッセージの `content` だけです。`referenced_message`(リプライ引用)、`message_snapshots`(転送)、`embeds`(URL プレビュー)、添付は読みません。これらは非リストユーザや外部サイトの文字列が入る経路だからです([security.md §2](security.md#2-discord-側の対策))。`content` 内のユーザーメンションは `allowed_users` なら表示名に解決し、それ以外は `<user>` に置き換えます。メッセージの編集・削除イベントは MVP では無視します(SQLite の記録は投稿時のまま)。

## 3. `Router`(System One)

Router の抽象は System One API の形に合わせます。`state`(判断対象の状態)と複数の `questions` を1リクエストで渡し、各質問に確率付きの回答を得ます。質問型は API と同じ3種類だけです。質問セット自体は Core が定義し([context-layers.md §4](../context-layers.md#4-system-one-判定スキーマ))、router module は「どう問い合わせるか」だけを担います。

```rust
pub enum Question {
    /// yes / no。回答は yes である確率(0.0〜1.0)
    Noul { instructions: String },
    /// 選択肢から1つ。回答は各選択肢の確率と confidence。選択肢は最大 255
    Choice { instructions: String, criteria: Vec<(String, String)> },
    /// 順序付きルーブリックで評価。レベルは 2〜10。回答は確率加重スコアと各レベルの確率
    Score { instructions: String, levels: Vec<String> },
}

pub enum Answer {
    Noul { p: f32 },
    Choice { choice: String, probabilities: HashMap<String, f32>, confidence: f32 },
    Score { score: f32, probabilities: HashMap<u8, f32>, confidence: f32 },
}

pub struct Decision {
    pub answers: HashMap<String, Answer>,  // 質問キー → 回答
    pub latency: Duration,
}

#[async_trait]
pub trait Router: Send + Sync {
    async fn decide(&self, state: &RouterState, questions: &[(String, Question)]) -> Result<Decision>;
    /// 1リクエストに載せられる質問数。候補チャンクの数を決めるのに使う(Clef は 64)
    fn max_questions(&self) -> usize;
}
```

確率を閾値で二値化するのは Core の責務です。`router-mention` は `decide` の中で API を呼ばずにルールで `Decision` を組み立て、確率は 0.0 / 1.0 の二値になります。

Core は **provider(System 2 の LLM)に JSON を吐かせる汎用 router を提供しません。** 文字列生成を伴うため遅く、コストが高く、判定のためだけに生ログを LLM に入れることが [設計原則2](../context-layers.md#1-目的と設計原則)(本線の無菌化)に反するからです。

## 4. `Provider`(System 2)

Provider は Layer を区別したまま受け取ります。各 provider module は自分のキャッシュ方式に合わせて Layer を API の `system` / `messages` に載せます([caching.md §1](caching.md#1-provider-ごとの-layer-の載せ方))。Core は Layer の内容と境界だけを保証し、キャッシュ戦略には関与しません。

```rust
pub struct LayeredPrompt {
    pub harness_rules: String,        // Layer 0
    pub profile: String,              // Layer 1
    pub episodic_digest: String,      // Layer 2
    pub active_log: Vec<Turn>,        // Layer 3
    pub ephemeral_tail: Vec<Turn>,    // Layer 4
    pub tools: Vec<ToolSpec>,         // worker が有効なら本線にも同じ定義を渡す(caching.md §2)
}

pub struct Completion {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,    // 本線がこれを返したら Core が spawn_task として扱う
    pub usage: Usage,                 // input_tokens / cached_tokens / output_tokens。プロンプト全体の上限判定に使う
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, prompt: &LayeredPrompt, params: &CompletionParams) -> Result<Completion>;
    /// Layer 0〜3 のキャッシュ期限を延ばす keep-alive。既定は何もしない。MVP では呼び出し元も無効
    async fn keep_alive(&self, _prefix: &LayeredPrompt) -> Result<()> { Ok(()) }
}
```

provider のエラー(429 / 5xx)は `retry_max` 回まで指数バックオフで再試行し、それでも失敗したら Core に `Err` を返します。本線はそのターンを無言で終え、トリガー発言をバッファに戻します。

## 5. `Tool`

```rust
pub struct ToolSpec { pub name: String, pub description: String, pub input_schema: schemars::Schema }

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn call(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;
}
```

tool の出力は worker のコンテキストにだけ入り、本線には届きません。出力は `tool_output_max_chars` で切り詰めてから Worker に返します。

worker が tool を呼ぶ前に `ApprovalPolicy::check(&ToolSpec, &input, &ToolContext)` を通します。承認ポリシーの内容と `tool-shell` の作業ディレクトリは [security.md §3](security.md#3-web-側の対策と承認ポリシー) と [§4](security.md#4-tool-shell-の運用) で定義します。

## 6. `Compactor`

```rust
#[async_trait]
pub trait Compactor: Send + Sync {
    /// Layer 3 の指定範囲を三人称のエピソード要約に変換する
    async fn digest(&self, turns: &[Turn]) -> Result<String>;
}
```

要約の生成には本線と同じ provider を使いますが、Layer 1(Profile)を入れず、Layer 0 の「要約は三人称の事実記述にする」規約だけを与えます。キャラクターの文体が要約に混ざるのを防ぐためです。
