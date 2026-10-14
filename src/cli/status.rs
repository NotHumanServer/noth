use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use jiff::Timestamp;
use noth_core::heartbeat::STALE_AFTER;
use noth_core::store::Store;

use super::paths;

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
    println!(
        "進行中の Worker: {} 件",
        store.active_runs(hb.guild_id).await?.len()
    );
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
