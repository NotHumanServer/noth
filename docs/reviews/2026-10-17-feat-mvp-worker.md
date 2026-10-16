# コードレビュー: `feat/mvp-worker`(main..HEAD)

| 項目 | 内容 |
|---|---|
| 日時 | 2026-10-17 06:48 JST |
| モデル | DeepSeek V4.1 Flash Free(`opencode/deepseek-v4.1-flash-free`) |
| 対象ブランチ | `feat/mvp-worker`(main 比 58 コミット、約 47k 行追加) |
| 対象リビジョン | `76acd36`(作業ツリーに `layers.rs` / `docs/context-layers.md` の未コミット変更あり) |
| 範囲 | ブランチ全体。providers/router、Core(Core logic/config、actor/store)、tools/worker、Discord channel、CLI |
| 実行した検査 | `cargo clippy --all-targets --all-features`(警告なし)、`cargo test --workspace --all-features`(740 件成功)、`cargo build --no-default-features`(成功)、コミット済みシークレット/TODO の走査 |

## 総評

main はほぼ空のスケルトンで、このブランチがプロジェクト全体にあたる。設計書が極めて詳細で、実装はそれに高い忠実度で追随しており、全体の品質は良好。

- `cargo clippy --all-targets --all-features`: 警告なし
- `cargo test --workspace --all-features`: 740 件すべて成功
- コミット済みシークレット・TODO/FIXME・不要物の混入なし。`unsafe` は `umask` の 1 箇所のみで根拠コメントあり
- 信頼境界(Discord の受信フィルタ、`/noth` の master 限定、tool-discord の閲覧権限計算、Codex トークンの 0600/rename/送信先固定)は仕様どおりで、Critical な穴は見つからなかった

以下、実障害に直結しうる順にまとめる。行番号はレビュー時点(`76acd36`)のもの。

## 修正推奨(High)

### 1. 遠隔からプロセスを落とせる panic: `Duration::from_secs_f64` に上限クランプがない
- 該当: `crates/noth-provider-google/src/wire.rs:353`、`crates/noth-router-systemone/src/retry.rs:31`
- 検証は `is_finite() && >= 0.0` のみ。`Retry-After` ヘッダや `RetryInfo.retryDelay` に `1e300` のような巨大な有限値が来ると `from_secs_f64` が overflow で panic し、常駐プロセスが落ちる(ネットワーク入力起点)。
- 推奨: `BACKOFF_CAP` 等でクランプしてから変換する。上流は信頼済みとはいえ、daemon が外部ヘッダで落ちるのは避けるべき。

### 2. エラー時にチャンネルが恒久的に黙る: `silent_pending` が解除されない
- 該当: `crates/noth-core/src/actor.rs:983-987`(`start_silent`)
- `insert_worker_run` → `silent_pending.insert(channel)` の直後の `active_turns(...).await?` が失敗すると早期 return し、`silent_pending` が残ったまま `worker_runs` は `queued` で残留する。解除は Join 経路のみなので、以後そのチャンネルの補足・訂正 Worker が永久に起動しない。
- `start_task`(`actor.rs:942-956`)も `enforce_hard_limit`/`active_turns` の失敗で Ack だけ出て run が残る。
- 推奨: 挿入を成功確定後へ移すか、失敗時に巻き戻す。

### 3. Bot 目印のリプライ先名が未サニタイズ(仕様違反)
- 該当: `crates/noth-channel-discord/src/handler.rs:210-223`
- `author_name` は `bot_display_name` で改行除去・`[`/`]` 全角化・32 字制限しているのに、`reply_to_author` は `nick`/`lookup`/`display_name()` を生で格納。`BotMarker::render`(`crates/noth-core/src/types.rs:59-66`)が `[Bot …]` 行にそのまま連結する。
- `docs/architecture/security.md:17` は「Bot の表示名とリプライ先の名前は長さを制限し改行を除く」と明記し、`types.rs:49-52` のコメントも「Channel で制限済み」と書いているが実装が伴っていない(`text/tests.rs:221` は Bot 名側のみ検証)。
- 推奨: `reply_to_author` にも共通のサニタイズを適用し、偽造できないテストを追加。

### 4. `.env` の読み取り失敗を握りつぶし、既存シークレットを全消ししうる
- 該当: `src/cli/config_cmd.rs:79, 106, 167`
- `std::fs::read_to_string(&p.env).unwrap_or_default()` は「存在するが読めない」場合も空文字として扱い、`env_set` の結果(=そのキー 1 行)で上書きする。
- 推奨: `NotFound` のときだけ空、それ以外は `Err` にする。

### 5. TinyFish のレスポンス本文にサイズ上限がない
- 該当: `crates/noth-backend-tinyfish/src/client.rs:243-246`
- `resp.text()` で無制限に読み込む。`web_fetch`/`web_agent` は任意ページ・巨大 JSON を受けるため OOM を誘発できる。同リポジトリの `crates/noth-tool-discord/src/api.rs:9-10,92-105` は 4MB 上限を実装済みなので同方式に。

### 6. Worker の 1 応答あたり tool 呼び出し数と履歴蓄積に上限がない
- 該当: `crates/noth-worker/src/lib.rs:599-611`
- `completion.tool_calls` を全件逐次実行し、上限は往復回数(`max_iterations` 既定 25)だけ。1 ターンで 100 個返れば 100 コマンドが走る(副作用は戻せない)。`tool_exchange` は反復ごとに無制限に伸び、25 × 20k 文字で context 超過の run 全損・コスト増になりうる。
- 推奨: 1 応答あたりの上限と、蓄積の総量上限を設ける。

## 中(Medium)

### 除外の保留整合
- `crates/noth-core/src/exclusion.rs:102-116` `set_expanded` が `expansion_pending` を無条件解除。呼び出し側 `crates/noth-channel-discord/src/exclusion.rs:260` は `unlisted_under` が DB エラーで空を返しても `set_expanded` するため、一覧に無い除外(アーカイブ済みスレッド等)が未展開のまま保留が解け、未処理発言が昇格・判定されうる(docs/context-layers.md の保留規定に反する)。
- `crates/noth-core/src/exclusion.rs:176-184` `filter_batch` は pending も親チェーンも見ない(`filter_rows:188-191` は見る)。非対称。

### 設定 / 判定
- `crates/noth-core/src/config.rs:762-787` 閉じていない `${` を黙って通す(タイポが「未定義」エラーにならず実行時故障)。
- `crates/noth-core/src/config.rs:714-728` backend 名の解決を未解決の `raw` から行う。
- `crates/noth-core/src/config.rs:187-212` 閾値・`state_max_chars`・`recent_messages`・`max_tokens` に範囲検査なし(他の項目は検査済みで不揃い)。
- `crates/noth-core/src/judge.rs:268,398-405` `delivery_style` の同点が先頭の `normal_report` に落ちる。docs §4「同点は安全側を先頭」に照らすと `abort` が安全側。

### プロンプト整形
- `crates/noth-core/src/layers.rs:326-353,374-378` チャンネル名のサニタイズが `[発言先]` にのみ効き、`format_block` の `l.channel`/`l.author` は生のまま。
- `crates/noth-core/src/memory.rs:231` Layer 1.5 の上限が `memory_max_chars * 2`(既定 12,000 字)。docs の 6,000 字と揺れる。

### tool-shell / worker
- `crates/noth-tool-shell/src/capture.rs:7-8` の「先頭＋末尾 32KB」設計が、`crates/noth-worker/src/lib.rs:334-342` の先頭からの `tool_output_max_chars`(既定 20k 字)切り詰めで無効化され、末尾と中略が消える。`[stderr]` が `[stdout]` より先に連結されるため stderr 多めだと stdout が全滅。
- `crates/noth-tool-shell/src/lib.rs:51,97-99` 「背景プロセスも含めすべて終了」は `setsid`/`nohup` でグループを抜けた子には効かない。説明を実挙動に合わせる。
- `crates/noth-worker/src/lib.rs:169-173` 進捗表示で `mysql -psecret` 等の連結パスワードがマスクされない。

### Discord / CLI
- `crates/noth-tool-discord/src/lib.rs:597,617` `channel_info.topic`/`server_info.description` がメンション置換・長さ上限なしで素通し。
- `crates/noth-core/src/store/mod.rs:568-570` `search_messages` の `exclude_channels` バインドが未分割(`crates/noth-core/src/store/exclusions.rs:74-76` は 500 件ずつ)。除外数が SQLite 変数上限を超えると失敗。
- `crates/noth-core/src/store/mod.rs:833-849` `last_usage` が `removed = 1`(圧縮済み)のターンも拾いうる。
- `src/cli/config_cmd.rs:86-90` 平文シークレットを config.toml(0644)へ警告のみで書き込み、`config show/get`(`crates/noth-core/src/config.rs:789-797`)は平文を表示。
- `crates/noth-channel-discord/src/command/api.rs:64-72` `Client::build().unwrap_or_default()` はタイムアウト無しクライアントに縮退し、defer がハングしうる。
- `src/cli/logs.rs:18,57-65` 常に全ファイル読み(数 GB で OOM)、`latest_log` が `noth.log.gz` を拾いうる。
- `src/cli/init.rs:124-127` 出力 `.env` が「有効 module に必要な分だけ」ではない(docs/cli.md:40 と不一致)。

## 低 / ドキュメントと実装のずれ

- `docs/architecture/modules.md:101-103` の feature 例は `tool-web-*` が `noth-backend-tinyfish` を有効化すると書くが、実際の `Cargo.toml` は無効化(backend はクレート直依存)。
- `docs/architecture/dependencies.md:17` は `schemars 1.2` を「tool の input_schema 生成」に使うと書くが、`schemars` は `Cargo.toml` にも一切無く、`input_schema` は手書き `json!`。
- `docs/architecture/overview.md:37,53` の Event Bus は `MessageReceived`/`WorkerFinished`/`IdleDetected` の 3 つだが、実装(`crates/noth-core/src/bus.rs:7-9`)は `MessageReceived` のみ(Join/Idle は actor のキュー直積み)。
- `docs/architecture/traits.md` のシグネチャが実装から遅れ(`CompletionParams.purpose/run_id`、`Compactor::digest`、`ToolContext` の `meter/history/audience`)。
- `docs/context-layers.md:36,425-454` の `Dream` Job / `dreams` テーブル / Dreaming は未実装(`crates/noth-core/src/actor.rs` の `Job` に `Dream` なし)。
- `docs/discord-setup.md:86` 「`noth service` 未実装」は古い(`src/cli/service.rs` は実装済み)。
- `docs/architecture/modules.md:11` 「channel は1つ以上」に対し `crates/noth-core/src/registry.rs:154` は「ちょうど1つ」。
- noise 発言で新規チャンクを作る件(`crates/noth-core/src/actor.rs:539-554` にコメントあり)は docs/context-layers.md §4.2 と齟齬。意図的な例外なら docs に明記を。
- 本番経路の `expect`/`unwrap`/`unreachable!`: `crates/noth-core/src/registry.rs:183-213`、`crates/noth-core/src/judge.rs:378,501`、`crates/noth-core/src/config.rs:606`、`crates/noth-provider-openai/src/wire.rs:513`、`crates/noth-tool-discord` 各所、`crates/noth-worker/src/lib.rs:169`、`src/cli/run.rs:64` ほか。多くは不変条件で守られているが、`crates/noth-core/src/error.rs` の方針に合わせ縮退させると頑健。

## 確認して問題がなかった主な点

- Discord 受信フィルタ: guild 一致 → 種別(`Regular`/`InlineReply`)→ 除外 → 作者分類の順で、`AuthorKind::Drop` は `content` を読む前に return。`content` 以外は読まない。webhook は破棄。Bot 本文は空で目印のみ。SQLite/ログに非許可ユーザーの本文が入る経路なし。
- `/noth` 権限: guild と実行者 ID を毎回照合(ボタン押下も同じ経路)。`master` 未設定ならコマンド削除。拒否・エラー・未知はすべて ephemeral、内部エラー文は置換してログのみ。
- tool-discord 閲覧権限: `@everyone`＋ロール和 → ADMINISTRATOR/オーナー → overwrite の deny/allow の順で計算。拒否文は統一。`search_messages` は Bot 目印を除外。
- provider の認証/秘匿: `codex-auth.json` は 0600/0700・tmp rename + `sync_all`・`FileLock`。`Debug` マスクあり。Codex は base_url 固定・送信先限定。Anthropic の `cache_control` 3 ブレークポイントは caching.md と整合。
- 課金計算: `from_reported` は `saturating_sub` で underflow 耐性あり。`pricing.rs` の prefix+suffix マッチングは妥当。
- actor の直列化、SQLite のトランザクション境界・WAL・パーミッション設定、再起動復元の順序は妥当。

## 横断的な推奨

- CI がない(`.github` なし。pre-commit はローカルの clippy/rustfmt/nixfmt のみ)。特にルートで `cargo test` を実行するとルートクレートの 35 件しか走らず、crate 側の 705 件は `--workspace` を付けないと実行されない。`cargo test --workspace --all-features` と `clippy --workspace --all-targets` を回す CI を 1 本入れると回帰を大きく減らせる。
- docs が仕様の一次情報なので、コード変更時に該当 docs を同じコミットで直す運用を続けつつ、上記の既存ドリフトをまとめて解消するのが望ましい。

## 優先順位の要約

1. ①②(panic とチャンネル恒久停止)
2. ③④(仕様違反のサニタイズ漏れと `.env` 全消し)
3. ⑤⑥(リソース上限)
4. docs ドリフトの一掃
