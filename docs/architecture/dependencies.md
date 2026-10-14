# Noth アーキテクチャ: 採用クレート

2026年10月14日時点の crates.io 最新安定版。ツールチェーンは `stable`(sqlx が Rust 1.94 以上を要求)。

| 用途 | クレート | 版 | 備考 |
|---|---|---|---|
| 非同期ランタイム | `tokio` | 1.53 | イベントバスは `tokio::sync` の mpsc / broadcast、シェル実行は `tokio::process` |
| Discord | `serenity` | 0.12.5 | `EventHandler` で受信、`broadcast_typing`、`reference_message` でリプライ。最終リリースが 2025年12月なので採用前に Issue の活動を確認する |
| HTTP | `reqwest`(rustls) | 0.13 | LLM / System One / TinyFish すべてこれで公式 API を直接叩く。Anthropic の公式 Rust SDK は存在しない |
| シリアライズ | `serde` / `serde_json` / `toml` | 1.0 / 1.0 / 1.1 | |
| `.env` | `dotenvy` | 0.15 | |
| CLI | `clap`(derive) | 4.6 | |
| SQLite | `sqlx`(sqlite, runtime-tokio) | 0.9 | `migrate!` でマイグレーションを同梱 |
| ログ | `tracing` / `tracing-subscriber` / `tracing-appender` | 0.1 / 0.3 / 0.2 | |
| エラー | `thiserror`(クレート側)/ `anyhow`(バイナリ側) | 2.0 / 1.0 | |
| trait の非同期化 | `async-trait` | | [traits.md](traits.md) の全 trait で使う |
| JSON Schema | `schemars` | 1.2 | tool の `input_schema` 生成 |
| 日時 | `jiff` | 0.2 | |
| Google 認証 | `gcp_auth` | 0.12 | `agent-platform` preset のみ。サービスアカウント JSON のファイルパスを渡す |
| OS サービス | `service-manager` | 0.11 | launchd / systemd(user level)を同一 API で扱う。実機確認が必要 |

**トークン数は数えない。** `tiktoken-rs` は OpenAI のトークナイザ専用で Claude / Gemini には合わないため使いません。Layer 3 の上限判定には各 API のレスポンスに含まれる `usage` を保存して使います。

出典: [serenity](https://crates.io/crates/serenity)、[sqlx](https://crates.io/crates/sqlx)、[service-manager](https://github.com/chipsenkbeil/service-manager-rs)、[gcp_auth](https://crates.io/crates/gcp_auth)、[Anthropic SDK 一覧](https://platform.claude.com/docs/en/cli-sdks-libraries/overview)、[tiktoken-rs README](https://github.com/zurawiki/tiktoken-rs)
