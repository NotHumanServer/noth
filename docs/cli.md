# Noth CLI と設定ファイル仕様

Noth の CLI は設定・セットアップ・常駐プロセスの管理だけを担います。対話チャットや TUI は持ちません。コマンドの形式は [hermes-agent](https://github.com/nousresearch/hermes-agent) の CLI(`website/docs/reference/cli-commands.md`、`user-guide/configuration.md`)を参考にし、Noth に必要な範囲に絞っています。

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

対話モードでは次の順に進みます。

1. Discord Bot トークンを入力。`.env` に `DISCORD_BOT_TOKEN` として保存する。`MESSAGE_CONTENT` 特権インテントを Developer Portal で有効にするよう案内する(メンバー一覧の取得に `GUILD_MEMBERS` 特権インテントも必要になる可能性があり、実装時に確認する)
2. guild ID と `allowed_users` のユーザー ID を手入力する(Discord の開発者モードで ID をコピーする)。トークンが有効か、Bot が guild に参加しているかは最後の `noth doctor` で確かめる。バイナリが Discord の REST クライアントを直接持たないため、一覧からの選択は行わない
3. router を選ぶ(`router-systemone` の preset `jev`(既定)/ `clef`、または `router-mention`)。必要なキー(`TYPESAFE_API_KEY`、または `CF_API_TOKEN` と `CF_ACCOUNT_ID`)を `.env` に保存する
4. ビルドに含まれる module から provider を選び、preset・モデル名・API キーを設定する
5. ビルドに `tool-web-*` が含まれていれば `TINYFISH_API_KEY` を入力する
6. `profiles/default.md` の雛形を作成し、`[profile] name = "default"` を設定する
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
- `built-in` は、ビルド時に feature で組み込まれているかどうかを示す。組み込まれていない module は `enable` できず、必要な feature を案内する
- `enable` / `disable` は `config.toml` の `[modules] enabled` を書き換えるだけ。router / provider を2つ以上有効にした状態は `noth config check` と起動時に拒否する
- 常駐中なら「再起動が必要」と表示し、`--restart` で `noth service restart` まで行う

### `noth run`

```text
noth run
```

フォアグラウンドで常駐します。ログは標準エラーと `$NOTH_HOME/logs/` の両方に出します。`SIGINT` / `SIGTERM` で進行中の Worker を待ってから終了します(最長 `worker.timeout_secs`)。

### `noth service`

OS のサービスとして登録・操作します。macOS では launchd(user agent)、Linux では systemd(user unit)を使います。

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

### `noth auth`

```text
noth auth codex                       # ChatGPT のサブスクリプションで OpenAI Codex にログインする
```

`provider-openai` の `preset = "codex"` 用です。device code ログイン(表示された URL を開いてコードを入力する)を行い、トークンを `$NOTH_HOME/data/codex-auth.json` に保存します。トークンは期限の前に自動で更新されます。保存場所が `.env` でない理由は [security.md §5](architecture/security.md#5-openai-codex-のトークン) を参照してください。

### `noth status`

常駐プロセスの状態を表示します。SQLite の `heartbeat` テーブルと各テーブルを参照するので、プロセスへの IPC は不要です。表示するのは最終 heartbeat 時刻と接続状態、接続中の guild、有効な module、直近の router 判定、進行中の Worker、直近ターンのプロンプト全体のトークン数(`usage.input_tokens`)、今日のコスト(`今日のコスト: $0.0123(LLM 42 回、TinyFish 5 回)`)です。heartbeat が一定時間更新されていなければ「停止中」と判断します。

### `noth cost`

LLM と外部 API の利用量とコストを、SQLite の `costs` テーブルから集計して表示します。対象の guild は `channel.discord.guild_id` です。

```text
noth cost [--since <期間>] [--by source|model|purpose|day|run] [--offline]
```

- `--since`: `today`(ローカル時間の 0 時から)、`24h`、`7d`(既定)、`30d`、`all`。`<数値>h` / `<数値>d` も使える
- `--by`: 集計の単位。既定は `model`
- 表の列は、キー・回数・入力・cache 読み・cache 書き・出力・単位(`12 step` など、トークン以外の課金単位)・コスト。トークンは桁区切り、金額は `$0.0123` 形式。単価が分からない呼び出しがあれば `+不明 N 件` を付け、最後に合計行を出す
- 表のあとに、外部 API が返した直近のレート制限(source・bucket・残り/上限・リセットまでの時間・記録からの経過)と、有効な tool の課金先アカウントの状態(TinyFish の残高など。状態を返す tool だけ)を出す
- アカウントの状態は外部 API を呼んで取得する。`--offline` で省略できる

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

既定は `~/.noth/` で、環境変数 `NOTH_HOME` で変更できます。`NOTH_HOME` はプロセス環境変数から読む唯一の値です。

```text
$NOTH_HOME/
├── config.toml           # 設定本体
├── .env                  # シークレット(API キー、Bot トークン)
├── profiles/
│   ├── default.md        # Layer 1 のキャラクター定義
│   └── memory/
│       ├── MEMORY.md     # Layer 1.5 の長期記憶(雛形は noth init が作る)。手で編集してもよい
│       └── daily/
│           └── YYYY-MM-DD.md  # Dreaming が書く日次レポート
├── data/
│   └── noth.sqlite       # 全 guild 共通。各テーブルが guild_id を持つ
├── work/
│   └── <run_id>/         # tool-shell の作業ディレクトリ(run ごと)
└── logs/
```

## 4. 設定ファイル

### 4.1 シークレットの扱い

- シークレットは **`$NOTH_HOME/.env` にだけ**置く。プロセス環境変数は `NOTH_HOME` 以外読まない。参照先を1か所にして「どこから読まれているか分からない」状態を避けるため
- `config.toml` には平文のキーを書かず、`${VAR}` で `.env` の変数を参照する
- `${VAR}` の解決は **参照されるセクションだけ**に対して行い、`.env` に無ければ起動時にエラーで止める。無効な module のセクションは参照されないので、下の例のように全 provider の設定を書いたままにしておける

セクションが参照される条件は次のとおりです。

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
  "tool-discord",              # 読み取り専用の Discord 参照。[channel.discord] のトークンと guild_id を使う
]

[profile]
name = "default"                 # profiles/<name>.md

[context]
prompt_hard_limit_tokens = 80000 # 直近レスポンスの usage.input_tokens(tools + Layer 0〜4)がこれを超えたら緊急コンパクション。compactor 無効時は切り詰め
retry_max                = 2     # provider の 429 / 5xx・タイムアウトの再試行回数(初回と合わせて最大 retry_max+1 回)

[channel.discord]
guild_id      = "123456789012345678"
token         = "${DISCORD_BOT_TOKEN}"
allowed_users = ["111111111111111111", "222222222222222222"]
debounce_secs = 2.0

[router]
recent_messages  = 30            # state.recent に入れる直近発言数
state_max_chars  = 24000         # state 全体の文字数上限。Clef-flash(24,576 tokens)では保守的に
candidate_chunks = 32            # 関連判定にかける候補 TopicChunk の数(上限は max_questions - 7)。関連チャンクの問いを減らすか外すかは評価で決める
call_names       = ["noth", "ノス", "のす"]  # 本文でこう呼ばれたことを router に伝える(calls_noth_by_name)。決定的に noth_request とするのはメンションとリプライだけ
conversation_window_secs = 120  # Noth が応答してからこの秒数は、話していた相手の発言を noth_request として扱う。窓は外さず残す(評価で addressee だけでは 57 行中 39 行しか拾えなかった)
supplement_min_interval_secs = 300  # 黙って補足の Worker を起動する、チャンネルごとの最短間隔(秒)。訂正の Worker には適用しない
supplement_max_per_day       = 50   # 同じ起動の 1 日の上限回数(guild 全体。訂正は数えない)

[router.thresholds]              # noul の二値化閾値。v2 の質問の値は評価で決める
is_noise_or_meme = 0.5
is_topic_shift   = 0.6
relevant_chunk   = 0.5  # 関連チャンクの問いを外したら不要
needs_outside_work = 0.5  # noth_request でこれ以上なら Worker に回す。取りこぼさない側に倒す
# corrects_noth      = 評価で決める  # Noth の誤りの指摘とみなす(訂正)
# worth_supplement   = 評価で決める  # 低めにして広く拾う
min_confidence   = 0.15  # 合流判定の abort だけに使う。効かなければ外す(評価で決める)

# router が選べる絵文字リアクション。話すほどではないが気になる発言に付ける。
# 省略時は次の 8 種。`reactions = []` でリアクションしなくなる。`<:name:id>` のカスタム絵文字も使える
[[router.reactions]]
emoji   = "👍"
meaning = "同意・了解"
# 😂 笑える / 👀 気になる・続きを見ていたい / 🙏 感謝・お疲れさま / 🎉 お祝い・うれしい知らせ
# 🤔 考えさせられる・ちょっと疑問 / ❤️ 共感・いいね / 😢 残念・つらい

[router.systemone]
preset     = "jev"               # jev | clef | custom。省略時も jev
api_key    = "${TYPESAFE_API_KEY}"
# model    = "<版を固定した名前>"  # body の model。jev-latest などの別名は中身が変わるので避ける。clef preset では URL の末尾もこの値で決まる(例: "clef-flash")
# account_id = "${CF_ACCOUNT_ID}"  # clef のとき必須(api_key は "${CF_API_TOKEN}")
# endpoint = "..."               # custom のとき必須

[router.mention]
keywords        = ["noth", "ノス"]
topic_gap_mins  = 30             # これ以上空いたら is_topic_shift = true

[provider.anthropic]
api_key   = "${ANTHROPIC_API_KEY}"
model     = "claude-opus-5-5"
cache_ttl = "5m"                 # 5m | 1h
effort    = "low"                # low | medium | high | xhigh | max。雑談は low で速く返す。変えると直後の1回はキャッシュが効かない
# request_timeout_secs = 120     # 1 回の試行全体の上限(旧名 timeout_secs はどちらか一方だけ)
# OpenCode Go の Messages 形式モデルを使うときは preset を指定し、api_key にそのキーを書く
# preset  = "opencode-go"
# api_key = "${OPENCODE_GO_API_KEY}"
# 内蔵の単価表($/MTok)を上書きする。指定したキーだけが優先される。表にないモデルは金額が記録されない
# [provider.anthropic.pricing]
# input          = 4.0
# cache_write_5m = 5.0
# cache_write_1h = 8.0
# cache_read     = 0.2
# output         = 20.0

[provider.openai]
api_key  = "${OPENAI_API_KEY}"
model    = "..."
format   = "chat"                # chat | responses
# base_url = "..."               # 互換サーバー
# preset   = "opencode-go"       # このとき api_key は ${OPENCODE_GO_API_KEY}
# preset   = "codex"             # ChatGPT のサブスクリプションで Codex を使う。api_key は不要で、先に `noth auth codex` を実行する。format は responses に固定
# request_timeout_secs = 120     # 1 回の試行全体の上限(旧名 timeout_secs はどちらか一方だけ)。進んでいるストリームでも超えたら切って再試行する。codex は既定 300
# idle_timeout_secs    = 45      # codex の SSE で、SSE のバイトが何も届かない時間の上限。超えたらその試行を捨てて再試行する。reasoning 要約を要求しているが、Codex 実機での効果は未検証。高 effort で途中で切られるようなら大きくする。0 は不可
# effort   = "low"               # none | minimal | low | medium | high | xhigh | max(モデル依存)
# 内蔵の単価表($/MTok)を上書きする。指定したキーだけが優先される。表にないモデルは金額が記録されない。
# preset / base_url の先は表を使わず、input と output を両方指定したときだけ金額が出る(cached_input と cache_write は未指定なら input と同額)。
# preset = "codex" は定額プランの枠を消費するため金額を記録しない
# [provider.openai.pricing]
# input        = 2.5
# cached_input = 0.25
# cache_write  = 2.5
# output       = 15.0

[provider.google]
api_key = "${GOOGLE_AI_STUDIO_API_KEY}"
model   = "..."
# request_timeout_secs = 120   # 1 回の試行全体の上限(旧名 timeout_secs はどちらか一方だけ)
# preset           = "agent-platform"   # このとき api_key は不要
# project          = "my-project"
# location         = "us-central1"
# credentials_file = "${GOOGLE_APPLICATION_CREDENTIALS}"   # サービスアカウント JSON のパス
# 内蔵の単価表($/MTok)を上書きする。指定したキーだけが優先される。
# 表にないモデルと agent-platform / opencode-go は、3 キーすべて指定したときだけ金額が記録される
# [provider.google.pricing]
# input        = 0.3
# cached_input = 0.03
# output       = 2.5            # thinking 込み

[worker]
max_concurrent        = 2
timeout_secs          = 600      # 待ち時間を含む
max_iterations        = 25       # tool 呼び出しの反復上限
tool_output_max_chars = 20000    # 各 tool の出力の文字数上限。超えた分は切り詰める。tool-discord は約 12000 文字で項目単位に収めるので、12000 以上を推奨
approval              = "auto"   # MVP では auto のみ。有効な tool はどの run でも承認なしで呼べる(security.md §3)
show_progress         = true     # Ack を編集して、Worker が使った tool(検索語や URL)を表示する

[compactor]
idle_minutes      = 10           # 人間の最終発言からの経過。Noth 自身の投稿では解除しない
keep_recent_turns = 6
digest_max_chars  = 12000        # Layer 2 の上限。超えたら古いエピソードから削除する

[memory]                          # 長期記憶 MEMORY.md(Layer 1.5)と Dreaming
memo               = true         # 本線の [Memo] 行を MEMORY.md に追記する
memo_max_per_reply = 3
memory_max_chars   = 6000         # 超えている間は [Memo] を追記しない。Dreaming がこの長さに整理する
dreaming           = true         # compactor module が必要
dreaming_hour      = 4            # ローカル時刻。この時刻を過ぎた最初のアイドルで前日分を処理する
dream_input_max_chars = 60000     # 日次レポートに渡す発言ログの上限

[tool.web-search]
backend = "tinyfish"
[tool.web-fetch]
backend = "tinyfish"
[tool.web-agent]
backend = "tinyfish"
poll_interval_secs = 5
max_duration_secs  = 300         # TinyFish 側で run を打ち切る秒数。Agent はステップ課金のため上限を設ける
# max_steps         = 30          # 1 run の最大ステップ数(tool 入力の max_steps との小さい方。上限 150)
# max_steps_per_run  = 60          # 1 回の調査(Worker の run_id)での累計ステップ上限。超えたら API を呼ばず失敗を返す
[tool.discord]
# actions = ["fetch_messages", "search_messages", "list_channels"]   # 使える action を絞る。省略か空なら全部
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

`noth init` は有効にした module に必要な変数だけを書き出します。どの module がどの変数を使うかは、リポジトリ直下の `.env.example` にまとめてあります。

### 4.4 profile ファイル

`profiles/<name>.md` は Markdown の自由記述で、そのまま Layer 1 の先頭に入ります。Core はその後ろに guild のメンバー情報を自動で追加します。Harness Rules(Layer 0)はこのファイルより前に置かれるため、profile で Layer 0 の規約を上書きすることはできません。

## 5. hermes-agent との対応

| hermes | Noth | 備考 |
|---|---|---|
| `setup` | `init` | |
| `auth add/list/remove` | `config set <UPPER_KEY>` | hermes の `auth` は同一プロバイダの複数キーを切り替えて使うプールと OAuth を扱い `auth.json` に保存する。Noth は `.env` の単一キーのみで、プールと OAuth は対象外 |
| `config show/get/set/unset/path/check` | 同名 | |
| `mcp` / `plugins` | `module` | 静的リンクのため install / search は無い |
| `gateway run/start/stop/status/install` | `run` / `service` | |
| `status` / `doctor` / `logs` | 同名 | |
| `profile` | なし | 1ホスト1設定。Noth の「profile」はキャラクター定義を指す別概念 |
| `chat` / `-z` / `--tui` | なし | Discord 常駐専用 |
