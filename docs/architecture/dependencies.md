# Noth アーキテクチャ: 採用クレート

版は2026年10月14日時点の crates.io の最新安定版です。ツールチェーンは `stable` を使います(sqlx は Rust 1.94 以上を要求します)。

| 用途 | クレート | 版 | 備考 |
|---|---|---|---|
| 非同期ランタイム | `tokio` | 1.53 | イベントバスは `tokio::sync` の mpsc / broadcast、シェル実行は `tokio::process` |
| Discord | `serenity` | 0.12.5 | `EventHandler` で受信、`broadcast_typing`、`reference_message` でリプライ。最終リリースが 2025年12月なので採用前に Issue の活動を確認する |
| HTTP | `reqwest`(rustls + ring) | 0.12 | serenity 0.12 が解決する版に揃える(0.13 だと aws-lc-sys の C ビルドが増える)。証明書は webpki-roots と OS ストアの和集合。LLM / System One / TinyFish の公式 API は、すべて `reqwest` で直接呼び出す。Anthropic の公式 Rust SDK は存在しない |
| シリアライズ | `serde` / `serde_json` / `toml` | 1.0 / 1.0 / 1.1 | |
| `.env` | (自前のパーサ) | | `dotenvy` は値の中の `$VAR` をプロセス環境変数で展開するため使わない。`NOTH_HOME` 以外の環境変数を読まない原則([cli.md §4.1](../cli.md#41-シークレットの扱い))を守り、`$` を含むキーを壊さないため |
| CLI | `clap`(derive) | 4.6 | |
| SQLite | `sqlx`(sqlite, runtime-tokio) | 0.9 | `migrate!` でマイグレーションを同梱 |
| ログ | `tracing` / `tracing-subscriber` / `tracing-appender` | 0.1 / 0.3 / 0.2 | |
| エラー | `thiserror`(クレート側)/ `anyhow`(バイナリ側) | 2.0 / 1.0 | |
| trait の非同期化 | `async-trait` | | [traits.md](traits.md) の全 trait で使う |
| JSON Schema | `schemars` | 1.2 | tool の `input_schema` 生成 |
| 日時 | `jiff` | 0.2 | |
| Google 認証 | `gcp_auth` | 0.12 | `agent-platform` preset のみ。サービスアカウント JSON のファイルパスを渡す |
| umask 設定 | `libc` | 0.2 | unix のみ。ログファイルを 0600 で作るため、`noth run` が `umask(0o077)` を呼ぶ |
| OS サービス | `service-manager` | 0.11 | launchd / systemd(user level)を同一 API で扱う。実機確認が必要 |
| 設定ファイルの編集 | `toml_edit` | 0.25 | `noth config set/unset`、`noth module enable/disable`。コメントと書式を保ったまま書き換えるため。読み込みは `toml` のまま |
| 対話入力 | `inquire` | 0.9 | `noth init` の対話モード(パスワード入力・選択・複数選択) |

**ローカルでトークン数を計算しません。** `tiktoken-rs` は OpenAI のトークナイザ専用で Claude / Gemini には合わないため使いません。Layer 3 の上限判定には各 API のレスポンスに含まれる `usage` を保存して使います。

出典: [serenity](https://crates.io/crates/serenity)、[toml_edit](https://docs.rs/toml_edit/latest/toml_edit/struct.DocumentMut.html)、[inquire](https://docs.rs/inquire/latest/inquire/)、[sqlx](https://crates.io/crates/sqlx)、[libc](https://crates.io/crates/libc)、[service-manager](https://github.com/chipsenkbeil/service-manager-rs)、[gcp_auth](https://crates.io/crates/gcp_auth)、[Anthropic SDK 一覧](https://platform.claude.com/docs/en/cli-sdks-libraries/overview)、[tiktoken-rs README](https://github.com/zurawiki/tiktoken-rs)
