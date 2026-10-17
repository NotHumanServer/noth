//! fakecord の CLI 定義と、serve 以外のサブコマンド(操作用 API のクライアント)
use std::collections::BTreeMap;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

pub const DEFAULT_SERVER: &str = "http://127.0.0.1:3900";

#[derive(Debug, Parser)]
#[command(name = "fakecord", about = "擬似 Discord サーバーと、その操作用 CLI")]
pub struct Cli {
    /// 機械可読な JSON で出力する
    #[arg(long, global = true)]
    pub json: bool,

    /// 操作先の fakecord
    #[arg(long, global = true, default_value = DEFAULT_SERVER)]
    pub server: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 擬似 Discord サーバーを起動する(127.0.0.1 のみ)
    Serve {
        #[arg(long, default_value_t = 3900)]
        port: u16,
        /// 状態の保存先。あれば起動時に読み、終了時に書く
        #[arg(long)]
        snapshot: Option<std::path::PathBuf>,
    },
    /// 発言する
    Send {
        content: String,
        #[arg(long, default_value = "general")]
        channel: String,
        /// 発言するユーザー
        #[arg(long = "as", default_value = "kurazu")]
        as_user: String,
        /// 返信先のメッセージ ID
        #[arg(long)]
        reply_to: Option<String>,
    },
    /// テキストチャンネルの作成・削除・改名
    Channel {
        #[command(subcommand)]
        command: ChannelCommand,
    },
    /// カテゴリの作成
    Category {
        #[command(subcommand)]
        command: CategoryCommand,
    },
    /// スレッドの作成
    Thread {
        #[command(subcommand)]
        command: ThreadCommand,
    },
    /// アクティブなスレッドを全部含む THREAD_LIST_SYNC を配信する
    ThreadListSync {
        /// 親テキストチャンネル。省略時は guild 内の全スレッド
        #[arg(long)]
        channel: Option<String>,
    },
    /// スラッシュコマンドを実行する。例: `slash noth ignore add --option channel=general`
    Slash {
        /// コマンド名
        name: String,
        /// サブコマンド(グループ)の並び
        path: Vec<String>,
        /// `key=value` 形式のオプション。繰り返せる
        #[arg(long = "option")]
        options: Vec<String>,
        #[arg(long = "as", default_value = "kurazu")]
        as_user: String,
        #[arg(long, default_value = "general")]
        channel: String,
    },
    /// ボタンを押す
    Click {
        custom_id: String,
        #[arg(long)]
        message: Option<String>,
        #[arg(long, default_value = "general")]
        channel: String,
        #[arg(long = "as", default_value = "kurazu")]
        as_user: String,
    },
    /// リアクションを付ける
    React {
        emoji: String,
        /// 省略時はチャンネルの最新メッセージ
        #[arg(long)]
        message: Option<String>,
        #[arg(long, default_value = "general")]
        channel: String,
        #[arg(long = "as", default_value = "kurazu")]
        as_user: String,
    },
    /// メッセージと Bot の操作の記録を見る
    Log {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Bot の返信(投稿・編集・interaction 応答)を待つ
    WaitReply {
        /// 待つ秒数
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// この seq より後だけ見る。省略時は直近の操作の cursor
        #[arg(long)]
        since: Option<u64>,
        #[arg(long)]
        channel: Option<String>,
    },
    /// ユーザーの追加
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    /// サーバーの状態をスナップショットに保存する
    Snapshot {
        /// 保存先。省略時は `serve --snapshot` のパス
        #[arg(long)]
        path: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ChannelCommand {
    Create {
        name: String,
        /// 親カテゴリの名前または ID
        #[arg(long)]
        parent: Option<String>,
    },
    Delete {
        /// チャンネル(カテゴリ・スレッドも可)の名前または ID
        channel: String,
    },
    Rename {
        channel: String,
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum CategoryCommand {
    Create { name: String },
}

#[derive(Debug, Subcommand)]
pub enum ThreadCommand {
    Create {
        name: String,
        /// 親テキストチャンネル
        #[arg(long, default_value = "general")]
        channel: String,
        /// このメッセージ ID から作る
        #[arg(long)]
        message: Option<String>,
        #[arg(long = "as", default_value = "kurazu")]
        as_user: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum UserCommand {
    Add {
        name: String,
        /// allowed / master / none
        #[arg(long, default_value = "allowed")]
        role: String,
        /// Bot ユーザーとして作る
        #[arg(long)]
        bot: bool,
    },
}

// ---------- 表示の整形 ----------

/// ログの 1 行を `[時刻] #channel 名前: 本文` の形に整形する
pub fn format_log_line(timestamp: &str, channel: &str, author: &str, content: &str) -> String {
    format!("[{}] #{} {}: {}", timestamp, channel, author, content)
}

/// Discord 形のメッセージ JSON からログの 1 行を作る。channel 名は別途与える
pub fn format_log_from_message(msg: &Value, channel_name: &str) -> String {
    let timestamp = msg.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
    let author = msg
        .get("author")
        .and_then(|a| a.get("username"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
    format_log_line(timestamp, channel_name, author, content)
}

// ---------- 補助 ----------

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn error_message(text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(text)
        && let Some(e) = v.get("error").and_then(|x| x.as_str())
    {
        return e.to_string();
    }
    text.trim().to_string()
}

async fn request(
    client: &reqwest::Client,
    server: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> anyhow::Result<String> {
    let url = format!("{}{}", server, path);
    let mut rb = client.request(method, &url);
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    let resp = rb
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("fakecord serve が起動していない (接続エラー: {})", e))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let msg = error_message(&text);
        eprintln!("error: {}", msg);
        std::process::exit(1);
    }
    Ok(text)
}

fn channel_name_map(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Ok(v) = serde_json::from_str::<Value>(text)
        && let Some(arr) = v.get("channels").and_then(|x| x.as_array())
    {
        for c in arr {
            if let (Some(id), Some(name)) = (
                c.get("id").and_then(|x| x.as_str()),
                c.get("name").and_then(|x| x.as_str()),
            ) {
                map.insert(id.to_string(), name.to_string());
            }
        }
    }
    map
}

fn parse_value(s: &str) -> Value {
    if s == "true" {
        return Value::Bool(true);
    }
    if s == "false" {
        return Value::Bool(false);
    }
    if let Ok(i) = s.parse::<i64>() {
        return json!(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return json!(f);
    }
    Value::String(s.to_string())
}

// ---------- 実行 ----------

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let server = cli.server.trim_end_matches('/').to_string();
    let client = reqwest::Client::new();
    let json_mode = cli.json;

    match cli.command {
        Command::Serve { .. } => Ok(()),
        Command::Send {
            content,
            channel,
            as_user,
            reply_to,
        } => {
            let mut body = json!({"channel": channel, "as": as_user, "content": content});
            if let Some(r) = reply_to {
                body["reply_to"] = json!(r);
            }
            let text = request(
                &client,
                &server,
                reqwest::Method::POST,
                "/_fake/send",
                Some(body),
            )
            .await?;
            if json_mode {
                println!("{}", text);
            } else if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(msg) = v.get("message") {
                    // 指定は ID のこともあるので、表示は常に応答の channel_id ではなく指定した名前にする
                    println!(
                        "{}",
                        format_log_from_message(msg, channel.trim_start_matches('#'))
                    );
                } else {
                    println!("{}", text);
                }
            } else {
                println!("{}", text);
            }
            Ok(())
        }
        Command::Channel { command } => match command {
            ChannelCommand::Create { name, parent } => {
                let mut body = json!({"name": name});
                if let Some(p) = parent {
                    body["parent"] = json!(p);
                }
                let text = request(
                    &client,
                    &server,
                    reqwest::Method::POST,
                    "/_fake/channels",
                    Some(body),
                )
                .await?;
                if json_mode {
                    println!("{}", text);
                } else if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if let Some(c) = v.get("channel") {
                        println!(
                            "created channel: {}",
                            c.get("name").and_then(|x| x.as_str()).unwrap_or("")
                        );
                    } else {
                        println!("{}", text);
                    }
                } else {
                    println!("{}", text);
                }
                Ok(())
            }
            ChannelCommand::Delete { channel } => {
                let path = format!("/_fake/channels/{}", percent_encode(&channel));
                let text = request(&client, &server, reqwest::Method::DELETE, &path, None).await?;
                if json_mode {
                    println!("{}", text);
                } else {
                    println!("deleted");
                }
                Ok(())
            }
            ChannelCommand::Rename { channel, name } => {
                let path = format!("/_fake/channels/{}", percent_encode(&channel));
                let body = json!({"name": name});
                let text =
                    request(&client, &server, reqwest::Method::PATCH, &path, Some(body)).await?;
                if json_mode {
                    println!("{}", text);
                } else {
                    println!("renamed");
                }
                Ok(())
            }
        },
        Command::Category { command } => match command {
            CategoryCommand::Create { name } => {
                let body = json!({"name": name});
                let text = request(
                    &client,
                    &server,
                    reqwest::Method::POST,
                    "/_fake/categories",
                    Some(body),
                )
                .await?;
                if json_mode {
                    println!("{}", text);
                } else {
                    println!("created category");
                }
                Ok(())
            }
        },
        Command::Thread { command } => match command {
            ThreadCommand::Create {
                name,
                channel,
                message,
                as_user,
            } => {
                let mut body = json!({"channel": channel, "name": name, "as": as_user});
                if let Some(m) = message {
                    body["message_id"] = json!(m);
                }
                let text = request(
                    &client,
                    &server,
                    reqwest::Method::POST,
                    "/_fake/threads",
                    Some(body),
                )
                .await?;
                if json_mode {
                    println!("{}", text);
                } else {
                    println!("created thread");
                }
                Ok(())
            }
        },
        Command::ThreadListSync { channel } => {
            let mut body = json!({});
            if let Some(c) = channel {
                body["channel"] = json!(c);
            }
            let text = request(
                &client,
                &server,
                reqwest::Method::POST,
                "/_fake/thread-list-sync",
                Some(body),
            )
            .await?;
            if json_mode {
                println!("{}", text);
            } else if let Ok(v) = serde_json::from_str::<Value>(&text) {
                println!(
                    "synced {} threads",
                    v.get("threads").and_then(|x| x.as_u64()).unwrap_or(0)
                );
            } else {
                println!("{}", text);
            }
            Ok(())
        }
        Command::Slash {
            name,
            path,
            options,
            as_user,
            channel,
        } => {
            let opts: Vec<Value> = options
                .iter()
                .map(|s| {
                    let (k, v) = if let Some((a, b)) = s.split_once('=') {
                        (a.to_string(), parse_value(b))
                    } else {
                        (s.clone(), Value::Null)
                    };
                    json!({"name": k, "value": v})
                })
                .collect();
            let mut body = json!({
                "name": name,
                "as": as_user,
                "channel": channel,
            });
            if !path.is_empty() {
                body["path"] = json!(path);
            }
            if !opts.is_empty() {
                body["options"] = json!(opts);
            }
            let text = request(
                &client,
                &server,
                reqwest::Method::POST,
                "/_fake/slash",
                Some(body),
            )
            .await?;
            if json_mode {
                println!("{}", text);
            } else if let Ok(v) = serde_json::from_str::<Value>(&text) {
                println!(
                    "interaction {}",
                    v.get("interaction_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                );
            } else {
                println!("{}", text);
            }
            Ok(())
        }
        Command::Click {
            custom_id,
            message,
            channel,
            as_user,
        } => {
            let mut body = json!({"custom_id": custom_id, "channel": channel, "as": as_user});
            if let Some(m) = message {
                body["message_id"] = json!(m);
            }
            let text = request(
                &client,
                &server,
                reqwest::Method::POST,
                "/_fake/click",
                Some(body),
            )
            .await?;
            if json_mode {
                println!("{}", text);
            } else {
                println!("clicked");
            }
            Ok(())
        }
        Command::React {
            emoji,
            message,
            channel,
            as_user,
        } => {
            let mut body = json!({"emoji": emoji, "channel": channel, "as": as_user});
            if let Some(m) = message {
                body["message_id"] = json!(m);
            }
            let text = request(
                &client,
                &server,
                reqwest::Method::POST,
                "/_fake/react",
                Some(body),
            )
            .await?;
            if json_mode {
                println!("{}", text);
            } else {
                println!("reacted");
            }
            Ok(())
        }
        Command::Log { channel, limit } => {
            let mut qs = String::new();
            let mut first = true;
            if let Some(c) = &channel {
                qs.push_str(if first { "?" } else { "&" });
                qs.push_str(&format!("channel={}", percent_encode(c)));
                first = false;
            }
            if let Some(l) = limit {
                qs.push_str(if first { "?" } else { "&" });
                qs.push_str(&format!("limit={}", l));
            }
            let path = format!("/_fake/log{}", qs);
            // fetch channels for name resolution (human mode only, but cheap; do always for map)
            let channels_text = request(
                &client,
                &server,
                reqwest::Method::GET,
                "/_fake/channels",
                None,
            )
            .await?;
            let cmap = channel_name_map(&channels_text);
            let text = request(&client, &server, reqwest::Method::GET, &path, None).await?;
            if json_mode {
                println!("{}", text);
            } else if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(messages) = v.get("messages").and_then(|x| x.as_array()) {
                    for m in messages {
                        let cid = m.get("channel_id").and_then(|x| x.as_str()).unwrap_or("");
                        let cname = cmap.get(cid).cloned().unwrap_or_else(|| cid.to_string());
                        println!("{}", format_log_from_message(m, &cname));
                    }
                }
            } else {
                println!("{}", text);
            }
            Ok(())
        }
        Command::WaitReply {
            timeout,
            since,
            channel,
        } => {
            let mut body = json!({"timeout_secs": timeout});
            if let Some(s) = since {
                body["since"] = json!(s);
            }
            if let Some(c) = channel {
                body["channel"] = json!(c);
            }
            let text = request(
                &client,
                &server,
                reqwest::Method::POST,
                "/_fake/wait-reply",
                Some(body),
            )
            .await?;
            if json_mode {
                println!("{}", text);
            } else if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(actions) = v.get("actions").and_then(|x| x.as_array()) {
                    for a in actions {
                        println!("{}", a);
                    }
                }
            } else {
                println!("{}", text);
            }
            let replied = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("replied").and_then(|x| x.as_bool()))
                .unwrap_or(false);
            if !replied {
                std::process::exit(2);
            }
            Ok(())
        }
        Command::User { command } => match command {
            UserCommand::Add { name, role, bot } => {
                let body = json!({"name": name, "role": role, "bot": bot});
                let text = request(
                    &client,
                    &server,
                    reqwest::Method::POST,
                    "/_fake/users",
                    Some(body),
                )
                .await?;
                if json_mode {
                    println!("{}", text);
                } else {
                    println!("added user");
                }
                Ok(())
            }
        },
        Command::Snapshot { path } => {
            let mut body = json!({});
            if let Some(p) = path {
                body["path"] = json!(p);
            }
            let text = request(
                &client,
                &server,
                reqwest::Method::POST,
                "/_fake/snapshot",
                Some(body),
            )
            .await?;
            if json_mode {
                println!("{}", text);
            } else if let Ok(v) = serde_json::from_str::<Value>(&text) {
                println!(
                    "saved: {}",
                    v.get("path").and_then(|x| x.as_str()).unwrap_or("")
                );
            } else {
                println!("{}", text);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_send() {
        let cli = Cli::try_parse_from([
            "fakecord",
            "send",
            "hello",
            "--channel",
            "general",
            "--as",
            "kurazu",
        ])
        .unwrap();
        assert!(matches!(cli.command, Command::Send { .. }));
    }

    #[test]
    fn parse_thread_list_sync() {
        let cli = Cli::try_parse_from(["fakecord", "thread-list-sync"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::ThreadListSync { channel: None }
        ));
        let cli =
            Cli::try_parse_from(["fakecord", "thread-list-sync", "--channel", "general"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::ThreadListSync { channel: Some(_) }
        ));
    }

    #[test]
    fn parse_wait_reply() {
        let cli =
            Cli::try_parse_from(["fakecord", "wait-reply", "--timeout", "10", "--since", "5"])
                .unwrap();
        assert!(matches!(cli.command, Command::WaitReply { .. }));
    }

    #[test]
    fn format_log_line_works() {
        assert_eq!(
            format_log_line("2026-01-01T00:00:00Z", "general", "kurazu", "hi"),
            "[2026-01-01T00:00:00Z] #general kurazu: hi"
        );
    }

    #[test]
    fn format_from_message() {
        let msg = json!({"timestamp": "t", "author": {"username": "u"}, "content": "c", "channel_id": "1"});
        assert_eq!(
            format_log_from_message(&msg, "general"),
            "[t] #general u: c"
        );
    }
}
