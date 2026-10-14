# Noth 設計書

Noth は少人数の Discord サーバーに常駐する AI エージェントハーネスです。サーバーの全チャンネルを常時観測し、「同じ空間にいる仲間」として文脈を共有しながら、自然な雑談への介入と自律的な調査・作業を行います。CLI は設定・セットアップ・常駐管理だけを担い、TUI や対話チャットは持ちません。

## 文書一覧

| 文書 | 内容 | こんなときに読む |
|---|---|---|
| [architecture/overview.md](architecture/overview.md) | 設計原則、Core と module の全体像、Core の責務、起動シーケンス、MVP に含めないもの | 最初に読む |
| [architecture/modules.md](architecture/modules.md) | module の命名規則と一覧、無効時の動作、preset、Cargo workspace と feature | module を足す・外す |
| [architecture/traits.md](architecture/traits.md) | Core の公開 trait(`Channel` / `Router` / `Provider` / `Tool` / `Compactor`)と共通型 | module を実装する |
| [architecture/caching.md](architecture/caching.md) | provider ごとの KV キャッシュ戦略、本線と Worker のキャッシュ共有、コスト | provider を実装する、Fork の挙動を変える |
| [architecture/security.md](architecture/security.md) | 信頼境界、Discord 側と Web 側の対策、承認ポリシー、`tool-shell` の運用 | tool を足す、本番に置く |
| [architecture/dependencies.md](architecture/dependencies.md) | 採用クレートと版、出典 | 依存を更新する |
| [context-layers.md](context-layers.md) | Layer 0〜4 の層構造、System One 判定スキーマ、ライフサイクル(静観 / 介入 / 自律タスク / 圧縮)、SQLite のデータと再起動復元 | 本線の挙動を変える |
| [cli.md](cli.md) | `noth` コマンド群、ディレクトリ構成、`config.toml` と `.env`、profile ファイル | セットアップする、設定項目を足す |
| [discord-setup.md](discord-setup.md) | Discord Bot の作成、特権インテント、招待 URL と権限、ID の取得、`noth init` から `noth run` まで | Bot を用意する、Discord に接続できない |

## 用語

| 用語 | 意味 |
|---|---|
| Core | 必須部分。Config Loader、CLI、Event Bus、Context Layer Manager、SQLite Store、Module Registry |
| module | Core の trait を実装する差し替え単位。`<種別>-<実装>` の名前を feature・クレート・設定・CLI で共通に使う |
| 本線(Main Profile Engine) | サーバーの仲間として対話する LLM。Layer 0〜4 のプロンプトを受け取る |
| Worker(Ephemeral Task Worker) | 本線から Fork して tool を使い、結論メモだけを返す使い捨ての LLM |
| router(System One) | 文字列を生成せず確率付きの判定だけを返す。介入判定・ノイズ判定・話題区切り・関連チャンク選択・合流判定 |
| TopicChunk | 話題の区切りでまとめた Layer 3 の単位。チャンネルごとに持つ |
| Layer 0〜4 | 本線プロンプトの層。0 は Harness Rules、1 は Profile、2 は Episodic Digest、3 は Active Clean Log、4 は Ephemeral Tail。Layer 0〜3 は追記専用で、Layer 4 だけを毎ターン差し替える |
| 本線 actor | Context Layer Manager。本線の状態を持つ唯一の writer で、`Judge` / `Join` / `Compact` を1本のキューから順に処理する |
| 昇格 | 発言や応答を Layer 3 の末尾へ確定追記すること |
| 未昇格クリーンバッファ | router が `silent` と判定し、ノイズでもない発言を、次に本線が発言するまで溜めておく場所 |
| Ack | 自律タスクを始めるときに本線が Discord に投稿する宣言(「それ、ちょっと調べてみますね」など) |
| Goal | 本線が Worker に渡す作業指示。Discord には出さない |
| Fork / Join | Fork は本線の Layer 0〜3 と tool 定義を引き継いで Worker を起動すること。Join は Worker の完了を本線 actor が受け取り、報告するかを判定すること |
| run | Worker の1回の実行。`run_id` で識別する |
| 結論メモ | Worker が作業後に本線へ返す構造化された要約(数百トークン)。途中の tool 出力は含まない |
| module 無効時の動作 | 任意 module を無効にした構成で、利用できる機能に応じて取る動作 |

## 設計判断の一覧

設計時に決めたことと、採らなかった案を残します。詳細は各文書の該当節にあります。

| 決定 | 理由 | 採らなかった案 | 採らなかった理由 | 詳細 |
|---|---|---|---|---|
| module は Cargo feature とクレートで静的リンクする | 1バイナリで配布でき、「Core の trait にだけ依存する」制約がコンパイル時に検査される | 外部プロセス(MCP / stdio)、動的ロード(dylib / WASM) | ABI とサンドボックスの設計コストが MVP に見合わない | [overview §1, §5](architecture/overview.md#1-設計原則) |
| router と provider は「ちょうど1つ」有効 | 本線と Worker が同一 provider・モデルを使う前提を単純に保つ | 複数 provider のフォールバック | キャッシュ共有が崩れる | [modules §2](architecture/modules.md#2-必須だが差し替え可能) |
| preset は provider module 内のテーブル | 接続先の切り替えは認証とベース URL の差でしかなく、module に分ける粒度ではない | `[preset.*]` セクションや preset module | | [modules §2](architecture/modules.md#2-必須だが差し替え可能) |
| Router 抽象は System One API の形(`Noul` / `Choice` / `Score`)、二値化は Core | 判定の質問セットを Core が持ち、router は問い合わせ方だけを担う | provider(LLM)に JSON を生成させる汎用 router | 遅く、コストが高く、生ログを本線系に入れる | [traits §3](architecture/traits.md#3-routersystem-one) |
| 本線にも Worker と同じ tool 定義を渡し、本線の `tool_calls` は `spawn_task` 扱い | Anthropic は `tools → system → messages` の前方一致なので、tool 定義が違うと Fork でキャッシュを引き継げない | Worker だけが tool を持つ | Worker を起動するたびにキャッシュ書き込みが発生する | [caching §2](architecture/caching.md#2-本線と-worker-のキャッシュ共有) |
| Worker 固有の指示は最初の user メッセージに置く | `system` が1文字でも違えば Layer 3 のキャッシュが全部外れる | `system` に追記 | | [caching §3](architecture/caching.md#3-worker-固有の指示の置き場) |
| Anthropic はブレークポイント3つ、OpenAI / Google は自動キャッシュに任せる | Layer 2 更新時に Layer 0〜1 を保ち、バッファ一括追記の lookback 超過に備える | Gemini の explicit caching | Gemini 4 Argon 以降で非対応 | [caching §1](architecture/caching.md#1-provider-ごとの-layer-の載せ方) |
| キャッシュの keep-alive は MVP 外 | 効果の検証を MVP 後に回す。trait メソッド(`keep_alive()`)だけ用意 | 常時 keep-alive | | [caching §5](architecture/caching.md#5-keep-alivemvp-外) |
| Discord は `allowed_users` の発言だけ通し、リストに含まれないユーザーと他 Bot の発言は保存もしない。読むのは `content` だけ | 信頼境界の外の文字列をプロセスに入れない | 全部保存して router で捨てる | SQLite と router の入力に未信頼の文字列が残る | [security §2](architecture/security.md#2-discord-側の対策) |
| `tool-shell` はサンドボックスなし。`code_or_tool` の run でだけ許可 | Web 由来の文字列を読む run からシェルを動かさない(間接プロンプトインジェクション → RCE の遮断) | run ごとに tool 定義を変える | キャッシュ共有が崩れる | [security §3, §4](architecture/security.md#3-web-側の対策と承認ポリシー) |
| Compactor は人間の最終発言から 10 分で通常圧縮、ハード上限超過で TopicChunk 単位の緊急退避 | 会話中に Layer 3 を書き換えず、Append-only を保つ | ターンごとの圧縮 | | [context-layers §5 パターン D](context-layers.md#パターン-d-アイドル時コンパクション) |
| 本線への書き込みは1つの actor に直列化 | 複数チャンネルの発言が交錯しても Layer 3 の追記順を一意にする | チャンネルごとの並列処理 | | [context-layers §2.1](context-layers.md#21-本線-actor-と処理単位) |
| SQLite 1ファイル、全テーブルに `guild_id`、MVP の実装は1 guild | 将来の複数 guild に備えつつ MVP を単純にする | guild ごとのファイル | | [context-layers §6](context-layers.md#6-sqlite-に持つデータ) |
| 設定は `config.toml`、シークレットは `.env` のみ。環境変数は `NOTH_HOME` だけ | シークレットの置き場を1つにし、`${VAR}` は有効 module のセクションだけ解決する | 設定ファイルにシークレットを直書き、環境変数での上書き | | [cli §4.1](cli.md#41-シークレットの扱い) |
| module の変更は再起動で反映 | ホットリロードの複雑さを MVP で持たない | 設定のホットリロード | | [overview §4](architecture/overview.md#4-起動シーケンス) |
| LLM / System One / TinyFish は SDK を使わず公式 HTTP API を直接呼び出す | 公式 HTTP API を直接使い、SDK への依存を減らす。Anthropic の公式 Rust SDK は存在しない | コミュニティ SDK | | [dependencies](architecture/dependencies.md) |
| ローカルでトークン数を計算せず、API の `usage` を保存して上限判定に使う | `tiktoken-rs` は OpenAI 専用で Claude / Gemini に合わない | トークナイザクレート | | [dependencies](architecture/dependencies.md) |
| CLI は設定・セットアップ・常駐管理だけ。TUI と対話チャットは持たない | Noth は Discord 常駐専用 | hermes-agent 風の対話 CLI | | [cli](cli.md) |

## 要確認

- Discord のメンバー一覧取得(`Channel::members()`)に `GUILD_MEMBERS` 特権インテントが要るか
