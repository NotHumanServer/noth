# Noth アーキテクチャ: module 一覧とクレート構成

本書は module の命名規則、必須 module と任意 module の一覧、任意 module を無効にしたときの動作、Cargo workspace と feature の構成を定義します。設計原則と全体像は [overview.md](overview.md)、各 module が実装する trait は [traits.md](traits.md) を参照してください。

## 1. 命名規則

module 名は `<種別>-<実装>` の形にします。この名前を Cargo feature 名、クレート名(`noth-` 接頭辞付き)、`config.toml` の `[modules] enabled` の要素、`noth module` コマンドの引数で **そのまま共通に使います**。

## 2. 必須だが差し替え可能

router と provider は **有効なものがちょうど1つ** でなければ起動しません。channel は1つ以上必要です。

| 種別 | module | 内容 |
|---|---|---|
| channel | `channel-discord` | Discord への接続、発言の受信とデバウンス、Bot からの投稿。Noth 以外の Bot(`author.bot`。webhook は除く)の発言は本文を入れない目印だけを流す。`command-*` module があれば、guild コマンド `/noth` を登録して受け付ける(master だけ。§3) |
| router | `router-systemone` | System One API(Jev / Clef)で発言の性質(宛先 `addressee`、外部作業の要否、訂正、補足の価値)・ノイズ判定・話題区切り・合流判定を行う。動き方は Core が決める |
| router | `router-mention` | ルールベース。メンション・リプライ・キーワード一致時のみ `addressee = noth_request`、他は `other`。外部作業・訂正・補足の問いは常に 0.0(Worker は起動しない)。話題区切りは「前の発言から一定時間空いた、またはチャンネルが変わった」で判定する。System One を契約していなくても起動でき、テストにも使う |
| provider | `provider-openai` | OpenAI Chat Completions 形式。Responses 形式も同 module 内で選べる |
| provider | `provider-anthropic` | Anthropic Messages API |
| provider | `provider-google` | Gemini `generateContent`。認証は API キー(AI Studio)か OAuth2(Agent Platform) |

**接続先 preset**: provider は `preset` 設定で接続先(ベース URL、認証方式、モデル ID とプロトコルの対応)を切り替えます。preset は module ではなく各 provider module 内のテーブルです。

| preset | 対象 provider | 内容 |
|---|---|---|
| (省略) | すべて | 各ベンダーの API に直接接続(OpenAI / Anthropic / Google AI Studio) |
| `opencode-go` | openai / anthropic / google | ベース URL `https://opencode.ai/inference/go/v1`。モデル ID ごとに Chat Completions / Responses / Messages / Google 形式のいずれかを使う。利用者は使いたいモデルの形式に合う provider module を有効にする |
| `codex` | openai | ChatGPT のサブスクリプションで OpenAI Codex のバックエンド(`https://chatgpt.com/backend-api/codex`)を Responses 形式で呼ぶ。認証は `noth auth codex` の OAuth([security.md §5](security.md#5-openai-codex-のトークン)) |
| `agent-platform` | google | Google Agent Platform(旧 Vertex AI)。`project` / `location` とサービスアカウントの OAuth2 |

## 3. 任意 module を無効にしたときの動作

| 種別 | module | 内容 | 無効時の動作 |
|---|---|---|---|
| worker | `worker` | Ephemeral Task Worker。main から Fork した隔離コンテキストで tool を使い、結論メモだけを返す | Core は Worker を起動しない(`noth_request` は main が返事をし、補足・訂正は静観する)。main に tool 定義を渡さない |
| compactor | `compactor` | アイドル時に Layer 3 を三人称要約へ圧縮し Layer 2 へ統合する | Layer 3 がハード上限を超えたら古い TopicChunk を要約せず破棄する |
| tool | `tool-web-search` | Web 検索。バックエンドは `tinyfish`(Search API) | 未登録なら tool 一覧に出ないので使えない。Worker は検索できない旨を結論メモに書く |
| tool | `tool-web-fetch` | URL の本文取得。バックエンドは `tinyfish`(Fetch API) | 未登録なら tool 一覧に出ないので使えない |
| tool | `tool-web-agent` | 自然言語の目標で実サイトを操作。バックエンドは `tinyfish`(Agent API `run-async` + ポーリング) | 未登録なら tool 一覧に出ないので使えない |
| tool | `tool-discord` | 読み取り専用の Discord 参照。サーバー内チャンネルの REST 取得(`fetch_messages` / `list_pins` / `list_channels` など)と、Noth が記録済みの発言の検索(`search_messages`)。`fetch_messages` は `message_id` を `before` / `after` / `around` で指定でき(Noth 以外の Bot の本文も content と embed のテキストで読める。目印の message_id から読む用途)、`channel_id` を省略するとトリガーのチャンネルを取り、`search_messages` はチャンネルを指定しないとトリガーのチャンネルだけを検索する(サーバー全体は `scope: "guild"`。Bot の目印は対象外)。Bot トークンと `guild_id` は `[channel.discord]` を流用し、読めるのは依頼した人全員に見えるチャンネルだけで、操作は `[channel.discord] guild_id` のサーバーに限る([security.md §2](security.md#2-discord-側の対策)) | Worker が過去ログやサーバー構成を参照できない旨を結論メモに書く。`channel-discord` が有効でないと起動しない(トークンを共有するため) |
| tool | `tool-shell` | ホスト上でのコマンド実行。サンドボックスなし([security.md](security.md)) | 未登録なら tool 一覧に出ないので使えない。Worker は実行できない旨を結論メモに書く |
| command | `command-context` | `/noth context`。main の文脈の使用量を 1 画面にまとめる。Layer 0〜3 と tool 定義ごとのトークン数と割合(文字数からの推定を、直近のリクエストの実測 `usage.input_tokens` に比例配分したもの)、`prompt_hard_limit_tokens` に対する合計の割合と圧縮の基準までの残り、直近のリクエストのキャッシュの読み込み・書き込み・未キャッシュ、モデル名、Layer 3 のターン数とチャンネル数、Layer 2 のエピソード数、動いている Worker の数を出す。Layer 4 はリクエスト中だけ存在するので含めない。読み取り専用で、本文は表示しない | `/noth` コマンドを登録しない。他の module の動作は変わらない |

tool module は `worker` が有効なときだけ意味を持ちます。`worker` 無効で tool が有効な構成は `noth doctor` が警告します。

command module は管理者向けの操作で、Discord などの Channel が受け付けます。コマンドを実行できるのは `[channel.discord] master` に書いた 1 人だけで、結果は実行した本人にだけ見えます(Channel 側の振る舞いは [discord-setup.md §2.6](../discord-setup.md#26-管理コマンド-noth-context-を使う)、trait は [traits.md §7](traits.md#7-command))。`master` が未設定なら、`command-context` が有効でもコマンドは登録されず、誰にも使えません。コマンドに対応しない Channel では何も起きません。

## 4. 共有クレート(module ではない)

| クレート | 用途 |
|---|---|
| `noth-backend-tinyfish` | TinyFish の HTTP クライアントと認証(`X-API-Key`)。`tool-web-*` の3 module が共有する |

バックエンドは tool の設定で `backend = "tinyfish"` のように選びます。MVP のバックエンドは `tinyfish` のみですが、tool の trait はバックエンドに依存しないため、後から別バックエンドを足せます。

## 5. クレート構成

Cargo workspace を使い、module ごとに1クレートにします。module の境界をクレート境界と一致させることで「Core の公開 trait にだけ依存する」制約がコンパイル時に検査されます。

```text
noth/
├── Cargo.toml                    # [workspace]
├── crates/
│   ├── noth-core/                # Core。trait 定義、イベントバス、層マネージャ、SQLite
│   ├── noth-channel-discord/
│   ├── noth-router-systemone/
│   ├── noth-router-mention/
│   ├── noth-provider-openai/
│   ├── noth-provider-anthropic/
│   ├── noth-provider-google/
│   ├── noth-worker/
│   ├── noth-compactor/
│   ├── noth-tool-web-search/
│   ├── noth-tool-web-fetch/
│   ├── noth-tool-web-agent/
│   ├── noth-tool-discord/
│   ├── noth-tool-shell/
│   ├── noth-command-context/
│   └── noth-backend-tinyfish/
└── src/main.rs                   # バイナリクレート noth。feature で module を束ねる
```

バイナリクレートの `Cargo.toml` では各 module クレートを `optional = true` の依存にし、同名の feature で有効化します。

```toml
[features]
default = [
  "channel-discord", "router-systemone", "router-mention",
  "provider-anthropic", "worker", "compactor",
  "tool-web-search", "tool-web-fetch", "tool-shell", "tool-discord",
  "command-context",
]
channel-discord   = ["dep:noth-channel-discord"]
router-systemone  = ["dep:noth-router-systemone"]
router-mention    = ["dep:noth-router-mention"]
provider-openai   = ["dep:noth-provider-openai"]
provider-anthropic = ["dep:noth-provider-anthropic"]
provider-google   = ["dep:noth-provider-google"]
worker            = ["dep:noth-worker"]
compactor         = ["dep:noth-compactor"]
tool-web-search   = ["dep:noth-tool-web-search", "dep:noth-backend-tinyfish"]
tool-web-fetch    = ["dep:noth-tool-web-fetch",  "dep:noth-backend-tinyfish"]
tool-web-agent    = ["dep:noth-tool-web-agent",  "dep:noth-backend-tinyfish"]
tool-shell        = ["dep:noth-tool-shell"]
tool-discord      = ["dep:noth-tool-discord"]
command-context   = ["dep:noth-command-context"]
```

`main.rs` は `#[cfg(feature = "...")]` で各 module の登録関数を Module Registry に渡すだけにし、ロジックを持ちません。
