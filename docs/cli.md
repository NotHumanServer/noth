# Noth CLI と設定ファイル仕様

Noth の CLI は設定・セットアップ・常駐プロセスの管理だけを担います。対話チャットや TUI は持ちません。コマンドの形式は [hermes-agent](https://github.com/nousresearch/hermes-agent) の CLI(`website/docs/reference/cli-commands.md`、`user-guide/configuration.md`)を参考にし、Noth に必要な範囲へ絞っています。

## 1. 呼び出し構造

```text
noth [global-options] <command> [subcommand] [args] [--flags]
```

### グローバルオプション

| オプション | 説明 |
|---|---|
| `-c`, `--config <path>` | 設定ファイルを直接指定。既定は `$NOTH_HOME/config.toml`。指定時は `.env` も同じディレクトリから読む |
| `-v`, `--verbose` | 詳細ログ |
| `-V`, `--version` | バージョン表示 |
| `-h`, `--help` | ヘルプ |

## 2. コマンド一覧

### `noth init`

`$NOTH_HOME` を作成し、設定ファイルと profile の雛形を置きます。

```text
noth init [--non-interactive]
```

対話モードの流れ:

1. Discord Bot トークンを入力。`.env` に `DISCORD_BOT_TOKEN` として保存する。`MESSAGE_CONTENT` 特権インテントを Developer Portal で有効にするよう案内する(メンバー一覧の取得に `GUILD_MEMBERS` 特権インテントも必要になる可能性があり、実装時に確認する)
2. トークンで接続し、Bot が参加している guild を一覧して1つ選ぶ。メンバー一覧を取得して `allowed_users` に入れるメンバーを選ぶ。一覧が取れない場合はユーザー ID の手入力も受け付ける
3. router を選ぶ(`router-systemone` の preset `jev` / `clef`、または `router-mention`)。必要なキー(`TYPESAFE_API_KEY`、または `CF_API_TOKEN` と `CF_ACCOUNT_ID`)を `.env` に保存する
4. provider を選び(ビルドに含まれる module から)、preset・モデル名・API キーを設定する
5. ビルドに `tool-web-*` が含まれていれば `TINYFISH_API_KEY` を入力する
6. `profiles/default.md` の雛形を置き、`[profile] name = "default"` を書く
7. `config.toml` を生成し、`noth doctor` を1回実行して結果を表示する

`--non-interactive` では雛形だけを生成し、値はすべてプレースホルダにします。

### `noth config`

```text
noth config show                      # 解決済みの設定を表示(.env の値はマスク)
noth config path                      # config.toml と .env のパス
noth config check                     # 構文・必須キー・${VAR} の解決を検証
noth config get <key> [--json]
noth config set <key> <value>
noth config unset <key>
```

- キーはドット区切り(例: `channel.discord.debounce_secs`)
- 大文字スネークケースのキー(例: `ANTHROPIC_API_KEY`)は `.env` に書き込む。それ以外は `config.toml` に書く
- `set` / `unset` は常駐中なら「再起動が必要」と表示する

### `noth module`

```text
noth module list                      # 表: name / built-in / enabled
noth module enable <name> [--restart]
noth module disable <name> [--restart]
```

- `<name>` は module のフル名(`channel-discord`、`provider-anthropic` など)
- `built-in` はビルド時に feature で組み込まれているか。組み込まれていない module は `enable` できず、必要な feature を案内する
- `enable` / `disable` は `config.toml` の `[modules] enabled` を書き換えるだけ。router / provider を2つ以上有効にした状態は `noth config check` と起動時に拒否する
- 常駐中なら「再起動が必要」と表示し、`--restart` で `noth service restart` まで行う

### `noth run`

```text
noth run
```

フォアグラウンドで常駐します。ログは標準エラーと `$NOTH_HOME/logs/` の両方に出します。`SIGINT` / `SIGTERM` で進行中の Worker を待ってから終了します(最長 `worker.timeout_secs`)。

### `noth service`

OS のサービスとして登録・操作します。macOS は launchd(user agent)、Linux は systemd(user unit)。

```text
noth service install
noth service uninstall
noth service start
noth service stop
noth service restart
noth service status
```

`install` が生成する定義には次を含めます。

- `NOTH_HOME` の固定(環境変数はこれしか読まないため、サービス定義に埋め込む)
- `PATH` の明示(launchd の既定 PATH は最小限で、`tool-shell` の動作に影響する)
- 停止タイムアウトを `worker.timeout_secs` + 余裕に合わせる(systemd `TimeoutStopSec`、launchd `ExitTimeOut`)
- Linux ではログアウト後も動かすために `loginctl enable-linger` が必要なことを `install` の出力で案内する

### `noth status`

常駐プロセスの状態を表示します。SQLite の `heartbeat` テーブルと各テーブルから読むので、プロセスへの IPC は不要です。表示するのは最終 heartbeat 時刻と接続状態、接続中の guild、有効な module、直近の router 判定、進行中の Worker、直近ターンのプロンプト全体トークン数(`usage.input_tokens`)。heartbeat が一定時間更新されていなければ「停止中」と判断します。

### `noth doctor`

設定と外部疎通を検査します。

- `config.toml` の構文と必須キー
- 有効な module のセクションにある `${VAR}` が `.env` で解決できるか
- router と provider がちょうど1つずつ有効か
- Discord トークンの有効性、guild への参加、`MESSAGE_CONTENT` インテント
- router / provider / backend の API 疎通
- SQLite ファイルの読み書き
- 警告: `allowed_users` が空、`worker` 無効で `tool-*` 有効、profile ファイルが無い

### `noth logs`

```text
noth logs [-f] [-n <lines>]
```

## 3. ディレクトリ構成

既定は `~/.noth/`。環境変数 `NOTH_HOME` で変更できます。これはプロセス環境変数から読む唯一の値です。

```text
$NOTH_HOME/
├── config.toml           # 設定本体
├── .env                  # シークレット(API キー、Bot トークン)
├── profiles/
│   └── default.md        # Layer 1 のキャラクター定義
├── data/
│   └── noth.sqlite       # 全 guild 共通。各テーブルが guild_id を持つ
├── work/
│   └── <run_id>/         # tool-shell の作業ディレクトリ(run ごと)
└── logs/
```

## 4. 設定ファイル

### 4.1 シークレットの扱い

- シークレットは **`$NOTH_HOME/.env` にだけ**置きます。プロセス環境変数は `NOTH_HOME` 以外読みません。参照先を1か所にして「どこから読まれているか分からない」状態を避けるためです
- `config.toml` には平文のキーを書かず、`${VAR}` で `.env` の変数を参照します
- `${VAR}` の解決は **参照されるセクションだけ**に対して行い、`.env` に無ければ起動時にエラーで止めます。無効な module のセクションは参照されないので、下の例のように全 provider の設定を書いたままにしておけます

どのセクションが参照されるかの規則:

| セクション | 参照条件 |
|---|---|
| `[modules]`、`[profile]`、`[context]`、`[router.thresholds]` | 常に |
| `[channel.<name>]`、`[router.<name>]`、`[provider.<name>]`、`[tool.<name>]` | 対応する module が `enabled` にあるとき |
| `[worker]`、`[compactor]` | 同名の module が `enabled` にあるとき |
| `[backend.<name>]` | その backend を指定した tool module が `enabled` にあるとき |

優先順位: CLI 引数 > `config.toml`(`${VAR}` は `.env` で解決) > 既定値。`.env` は `${VAR}` の値を供給するだけで、`config.toml` のキーを上書きしません。

### 4.2 `config.toml`

```toml
[modules]
enabled = [
  "channel-discord",
  "router-systemone",          # router はちょうど1つ
  "provider-anthropic",        # provider はちょうど1つ。本線と worker で共通
  "worker",
  "compactor",
  "tool-web-search",
  "tool-web-fetch",
  "tool-shell",
]

[profile]
name = "default"                 # profiles/<name>.md

[context]
prompt_hard_limit_tokens = 80000 # 直近レスポンスの usage.input_tokens(tools + Layer 0〜4)がこれを超えたら緊急コンパクション。compactor 無効時は切り詰め
retry_max                = 3     # provider の 429 / 5xx の再試行回数

[channel.discord]
guild_id      = "123456789012345678"
token         = "${DISCORD_BOT_TOKEN}"
allowed_users = ["111111111111111111", "222222222222222222"]
debounce_secs = 2.0

[router]
recent_messages  = 30            # state.recent に入れる直近発言数
state_max_chars  = 24000         # state 全体の文字数上限。Clef-flash(24,576 tokens)では保守的に
candidate_chunks = 32            # 関連判定にかける候補 TopicChunk の数(上限は max_questions - 4)

[router.thresholds]              # noul の二値化閾値と choice の最低 confidence
is_noise_or_meme = 0.7
is_topic_shift   = 0.6
relevant_chunk   = 0.5
min_confidence   = 0.4

[router.systemone]
preset     = "clef"              # jev | clef | custom
model      = "clef-flash"        # body の model。clef preset では URL の末尾もここから決める
api_key    = "${CF_API_TOKEN}"
account_id = "${CF_ACCOUNT_ID}"  # clef のとき必須
# endpoint = "..."               # custom のとき必須

[router.mention]
keywords        = ["noth", "ノス"]
topic_gap_mins  = 30             # これ以上空いたら is_topic_shift = true

[provider.anthropic]
api_key   = "${ANTHROPIC_API_KEY}"
model     = "claude-opus-5-5"
cache_ttl = "5m"                 # 5m | 1h
# OpenCode Go の Messages 形式モデルを使うときは preset を指定し、api_key にそのキーを書く
# preset  = "opencode-go"
# api_key = "${OPENCODE_GO_API_KEY}"

[provider.openai]
api_key  = "${OPENAI_API_KEY}"
model    = "..."
format   = "chat"                # chat | responses
# base_url = "..."               # 互換サーバ
# preset   = "opencode-go"       # このとき api_key は ${OPENCODE_GO_API_KEY}

[provider.google]
api_key = "${GOOGLE_AI_STUDIO_API_KEY}"
model   = "..."
# preset           = "agent-platform"   # このとき api_key は不要
# project          = "my-project"
# location         = "us-central1"
# credentials_file = "${GOOGLE_APPLICATION_CREDENTIALS}"   # サービスアカウント JSON のパス

[worker]
max_concurrent        = 2
timeout_secs          = 600      # 待ち時間を含む
max_iterations        = 25       # tool 呼び出しの反復上限
tool_output_max_chars = 20000    # 各 tool 出力の切り詰め
approval              = "auto"   # MVP では auto のみ。tool-shell は code_or_tool の run に限る

[compactor]
idle_minutes      = 10           # 人間の最終発言からの経過。Noth 自身の投稿では解除しない
keep_recent_turns = 6
digest_max_chars  = 12000        # Layer 2 の上限。超えたら古いエピソードから落とす

[tool.web-search]
backend = "tinyfish"
[tool.web-fetch]
backend = "tinyfish"
[tool.web-agent]
backend = "tinyfish"
poll_interval_secs = 5
[tool.shell]
# cwd は run ごとの $NOTH_HOME/work/<run_id>/ に固定。設定項目なし

[backend.tinyfish]
api_key = "${TINYFISH_API_KEY}"
```

### 4.3 `.env`

```dotenv
DISCORD_BOT_TOKEN=
CF_API_TOKEN=
CF_ACCOUNT_ID=
TYPESAFE_API_KEY=
ANTHROPIC_API_KEY=
OPENAI_API_KEY=
GOOGLE_AI_STUDIO_API_KEY=
GOOGLE_APPLICATION_CREDENTIALS=
OPENCODE_GO_API_KEY=
TINYFISH_API_KEY=
```

`noth init` は有効にした module に必要な変数だけを書き出します。

### 4.4 profile ファイル

`profiles/<name>.md` は Markdown の自由記述で、そのまま Layer 1 の先頭に入ります。Core はこの後ろに guild のメンバー情報を機械的に付け足します。Harness Rules(Layer 0)はこのファイルより前に置かれるため、profile で Layer 0 の規約を上書きすることはできません。

## 5. hermes-agent との対応

| hermes | Noth | 備考 |
|---|---|---|
| `setup` | `init` | |
| `auth add/list/remove` | `config set <UPPER_KEY>` | hermes の `auth` は同一プロバイダの複数キーを回すプールと OAuth を扱い `auth.json` に保存する。Noth は `.env` の単一キーのみで、プールと OAuth は対象外 |
| `config show/get/set/unset/path/check` | 同名 | |
| `mcp` / `plugins` | `module` | 静的リンクのため install / search は無い |
| `gateway run/start/stop/status/install` | `run` / `service` | |
| `status` / `doctor` / `logs` | 同名 | |
| `profile` | なし | 1ホスト1設定。Noth の「profile」はキャラクター定義を指す別概念 |
| `chat` / `-z` / `--tui` | なし | Discord 常駐専用 |
