use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use jiff::Timestamp;
use noth_core::heartbeat::STALE_AFTER;
use noth_core::store::Store;

use super::paths;

/// heartbeat が STALE_AFTER 以内に更新されていれば常駐中とみなす。DB が読めなければ停止中扱い
pub async fn is_running(cfg: Option<&Path>) -> bool {
    let Ok(p) = paths(cfg) else { return false };
    let db = p.database();
    if !db.exists() {
        return false;
    }
    let Ok(store) = Store::open(&db).await else {
        return false;
    };
    match store.read_heartbeat().await {
        Ok(Some(hb)) => {
            Timestamp::now().as_second() - hb.updated_at.as_second() <= STALE_AFTER.as_secs() as i64
        }
        _ => false,
    }
}

/// 設定を変えても常駐中のプロセスには反映されないので、再起動が要ることを知らせる
pub async fn notify_restart_needed(cfg: Option<&Path>) {
    if is_running(cfg).await {
        println!("再起動が必要です(`noth service restart`、またはフォアグラウンドなら再実行)");
    }
}

pub async fn run(cfg: Option<&Path>) -> Result<ExitCode> {
    let db = paths(cfg)?.database();
    // Store::open は DB を作ってしまうので、未起動の環境では開かない
    if !db.exists() {
        println!("未起動です(heartbeat がありません)");
        return Ok(ExitCode::SUCCESS);
    }
    let store = Store::open(&db).await?;
    let Some(hb) = store.read_heartbeat().await? else {
        println!("未起動です(heartbeat がありません)");
        return Ok(ExitCode::SUCCESS);
    };

    let age = Timestamp::now().as_second() - hb.updated_at.as_second();
    if age > STALE_AFTER.as_secs() as i64 {
        println!(
            "状態: 停止中(最終 heartbeat {} 、{age} 秒前)",
            hb.updated_at
        );
    } else {
        println!(
            "状態: {}(最終 heartbeat {}、{age} 秒前)",
            state_label(&hb.state),
            hb.updated_at
        );
    }
    println!("guild: {}", hb.guild_id);
    println!("info: {}", serde_json::to_string_pretty(&hb.info)?);

    match store.latest_decision(hb.guild_id).await? {
        Some(d) => {
            let mut answers = d.answers.to_string();
            if answers.chars().count() > 200 {
                answers = answers.chars().take(200).collect::<String>() + "...";
            }
            println!(
                "直近の判定: {} ({}) answers={answers}",
                d.kind, d.created_at
            );
        }
        None => println!("直近の判定: なし"),
    }
    match store.last_usage(hb.guild_id).await? {
        Some(u) => println!("直近ターンの input_tokens: {}", u.input_tokens),
        None => println!("直近ターンの input_tokens: -"),
    }
    let runs = store.active_runs(hb.guild_id).await?;
    println!("進行中の Worker: {} 件", runs.len());
    for r in &runs {
        match r.started_at {
            Some(t) => println!(
                "  {} {}(開始 {t})",
                r.run_id,
                run_status_label(r.status.as_str())
            ),
            None => println!("  {} {}", r.run_id, run_status_label(r.status.as_str())),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn state_label(state: &str) -> &str {
    match state {
        "connecting" => "接続中",
        "connected" => "接続済み",
        "disconnected" => "切断",
        other => other,
    }
}

fn run_status_label(status: &str) -> &str {
    match status {
        "queued" => "待機中",
        "running" => "実行中",
        other => other,
    }
}
