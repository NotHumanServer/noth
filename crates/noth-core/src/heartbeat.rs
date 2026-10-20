use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::task::JoinHandle;

use crate::store::Store;
use crate::traits::Channel;
use crate::types::GuildId;

pub const INTERVAL: Duration = Duration::from_secs(15);
/// これより古い heartbeat は「停止中」とみなす
pub const STALE_AFTER: Duration = Duration::from_secs(60);

pub fn spawn(
    store: Store,
    guild: GuildId,
    info: Value,
    channel: Arc<dyn Channel>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(INTERVAL);
        // 停止明けにまとめて打たず、次の周期に合わせる
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let state = channel.connection_state().as_str();
            if let Err(e) = store.upsert_heartbeat(guild, state, &info).await {
                tracing::warn!(error = %e, "heartbeat の更新に失敗");
            }
        }
    })
}
