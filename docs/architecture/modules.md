# Noth アーキテクチャ: module 一覧とクレート構成

本書は module の命名規則、必須 module と任意 module の一覧、任意 module を無効にしたときの動作、Cargo workspace と feature の構成を定義します。設計原則と全体像は [overview.md](overview.md)、各 module が実装する trait は [traits.md](traits.md) を参照してください。

## 1. 命名規則

module 名は `<種別>-<実装>` の形にします。この名前を Cargo feature 名、クレート名(`noth-` 接頭辞付き)、`config.toml` の `[modules] enabled` の要素、`noth module` コマンドの引数で **そのまま共通に使います**。

## 2. 必須だが差し替え可能

router と provider は **有効なものがちょうど1つ** でなければ起動しません。channel は1つ以上必要です。

| 種別 | module | 内容 |
|---|---|---|
| channel | `channel-discord` | Discord への接続、発言の受信とデバウンス、Bot からの投稿 |
| router | `router-systemone` | System One API(Jev / Clef)で介入判定・ノイズ判定・話題区切り・関連チャンク選択・合流判定を行う |
| router | `router-mention` | ルールベース。メンション・リプライ・キーワード一致時のみ `reply_only`、他は `silent`。話題区切りは「前の発言から一定時間空いた、またはチャンネルが変わった」で判定し、関連チャンクは常に空。System One を契約していなくても起動でき、テストにも使う |
| provider | `provider-openai` | OpenAI Chat Completions 形式。Responses 形式も同 module 内で選べる |
| provider | `provider-anthropic` | Anthropic Messages API |
| provider | `provider-google` | Gemini `generateContent`。認証は API キー(AI Studio)か OAuth2(Agent Platform) |

**接続先 preset**: provider は `preset` 設定で接続先(ベース URL、認証方式、モデル ID とプロトコルの対応)を切り替えます。preset は module ではなく各 provider module 内のテーブルです。

| preset | 対象 provider | 内容 |
|---|---|---|
| (省略) | すべて | 各ベンダーの API に直接接続(OpenAI / Anthropic / Google AI Studio) |
| `opencode-go` | openai / anthropic / google | ベース URL `https://opencode.ai/inference/go/v1`。モデル ID ごとに Chat Completions / Responses / Messages / Google 形式のいずれかを使う。利用者は使いたいモデルの形式に合う provider module を有効にする |
| `agent-platform` | google | Google Agent Platform(旧 Vertex AI)。`project` / `location` とサービスアカウントの OAuth2 |

## 3. 任意 module を無効にしたときの動作

| 種別 | module | 内容 | 無効時の動作 |
|---|---|---|---|
| worker | `worker` | Ephemeral Task Worker。本線から Fork した隔離コンテキストで tool を使い、結論メモだけを返す | router の `spawn_task` 判定を `reply_only` として扱う。本線に tool 定義を渡さない |
| compactor | `compactor` | アイドル時に Layer 3 を三人称要約へ圧縮し Layer 2 へ統合する | Layer 3 がハード上限を超えたら古い TopicChunk を要約せず破棄する |
| tool | `tool-web-search` | Web 検索。バックエンドは `tinyfish`(Search API) | Worker の `web_research` / `fact_check` で検索できない旨を結論メモに書く |
| tool | `tool-web-fetch` | URL の本文取得。バックエンドは `tinyfish`(Fetch API) | 同上 |
| tool | `tool-web-agent` | 自然言語の目標で実サイトを操作。バックエンドは `tinyfish`(Agent API `run-async` + ポーリング) | 同上 |
| tool | `tool-shell` | ホスト上でのコマンド実行。サンドボックスなし([security.md](security.md)) | `code_or_tool` の task は「対応ツールなし」の結論メモを返す |

tool module は `worker` が有効なときだけ意味を持ちます。`worker` 無効で tool が有効な構成は `noth doctor` が警告します。

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
│   ├── noth-tool-shell/
│   └── noth-backend-tinyfish/
└── src/main.rs                   # バイナリクレート noth。feature で module を束ねる
```

バイナリクレートの `Cargo.toml` では各 module クレートを `optional = true` の依存にし、同名の feature で有効化します。

```toml
[features]
default = [
  "channel-discord", "router-systemone", "router-mention",
  "provider-anthropic", "worker", "compactor",
  "tool-web-search", "tool-web-fetch", "tool-shell",
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
```

`main.rs` は `#[cfg(feature = "...")]` で各 module の登録関数を Module Registry に渡すだけにし、ロジックを持ちません。
