//! 擬似 Discord サーバー fakecord。noth の評価を本物の Discord なしで回すためのハーネス

pub mod cli;
pub mod control;
pub mod gateway;
pub mod json;
pub mod rest;
pub mod router;
pub mod state;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::state::{AppState, World};

/// 127.0.0.1 だけで待ち受ける。`snapshot` があれば起動時に読み、終了時(Ctrl-C)に書く。
/// port に 0 を渡すと空きポートを使い、実際のポートを `World::listen_port` に入れる
pub async fn serve(port: u16, snapshot: Option<PathBuf>) -> anyhow::Result<()> {
    serve_with_ready(port, snapshot, None).await
}

/// `serve` と同じ。待ち受けを始めたら実際のアドレスを `ready` へ送る(port 0 で起動するテスト向け)
pub async fn serve_with_ready(
    port: u16,
    snapshot: Option<PathBuf>,
    ready: Option<oneshot::Sender<SocketAddr>>,
) -> anyhow::Result<()> {
    let mut world = match &snapshot {
        Some(path) if path.exists() => World::load(path)?,
        _ => World::seed(),
    };
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let addr = listener.local_addr()?;
    world.listen_port = addr.port();
    world.snapshot_path = snapshot.clone();

    let state = AppState::new(world);
    let app = router::build(state.clone());
    tracing::info!("fakecord を http://{addr} で起動しました");
    if let Some(ready) = ready {
        let _ = ready.send(addr);
    }

    // Ctrl-C(Unix では SIGTERM も)で止める。先に WebSocket へ終了を知らせないと、開いたままの
    // 接続が graceful shutdown を止めてしまう
    let signal_state = state.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            wait_for_signal().await;
            signal_state.shutdown();
        })
        .await?;

    // 全接続が閉じて serve が戻ってから保存するので、終了直前の変更も入る
    if let Some(path) = snapshot {
        match state.save_snapshot(&path) {
            Ok(()) => tracing::info!("スナップショットを {} に保存しました", path.display()),
            Err(e) => tracing::error!("スナップショットを保存できませんでした: {e}"),
        }
    }
    Ok(())
}

/// Ctrl-C か SIGTERM を待つ
async fn wait_for_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => ctrl_c.await,
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}
