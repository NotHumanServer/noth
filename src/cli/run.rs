use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use noth_core::store::Store;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

use super::{create_private_dir, load, paths};

pub async fn run(cfg: Option<&Path>, verbose: bool) -> Result<ExitCode> {
    let p = paths(cfg)?;
    let logs_dir = p.logs_dir();
    for dir in [&p.home, &p.home.join("data"), &logs_dir] {
        create_private_dir(dir)?;
    }
    // ログの初期化より前に取る。2つ目のプロセスが同じログファイルへ書き始めないようにするため
    let _lock = super::lock::acquire(&p.run_lock())?;
    // ログは会話内容を含むので、以前に緩い権限で作られたディレクトリも絞る
    fs::set_permissions(&logs_dir, fs::Permissions::from_mode(0o700))?;
    // appender はログを既定の umask(通常 0644)で作るが、ログには会話内容が入る。
    // 書き出し先が appender 内部で開かれるので、作成前にプロセス全体の umask を絞るしかない
    // SAFETY: umask は引数を取らない単純なシステムコールで、メモリ安全性に影響しない
    unsafe { libc::umask(0o077) };
    // guard を落とすと未フラッシュのログが消えるので run 終了まで保持する
    let (file, _guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(&logs_dir, "noth.log"));
    tracing_subscriber::registry()
        .with(EnvFilter::new(log_filter(verbose)))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file)
                .with_ansi(false),
        )
        .init();

    let config = load(cfg)?;
    let modules = crate::registry().build(&config)?;
    let store = Store::open(&config.paths.database()).await?;
    tracing::info!("noth を起動します");
    noth_core::actor::run(config, modules, store, shutdown_signal()).await?;
    tracing::info!("noth を終了しました");
    Ok(ExitCode::SUCCESS)
}

// serenity の gateway ws は生のペイロード(メッセージ本文)をログに出すので、-v でも出さない
fn log_filter(verbose: bool) -> String {
    let level = if verbose { "debug" } else { "info" };
    format!("{level},serenity=warn,serenity::gateway::ws=off")
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_filter_is_valid_and_silences_ws() {
        for verbose in [false, true] {
            let f = log_filter(verbose);
            assert!(EnvFilter::try_new(&f).is_ok(), "{f}");
            assert!(f.contains("serenity::gateway::ws=off") && f.contains("serenity=warn"));
        }
    }
}
