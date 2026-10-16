//! チャンネルの除外。除外したチャンネルは Noth にとって存在しないものとして扱い、受信・判定・Layer 3・tool の参照・検索のどれにも出さない。
//! 受信する Channel と参照する tool が同じ状態を見るために、ここで 1 つの共有状態を持つ。
//! 実効の除外は config(`ignored_channels`)とコマンド(DB に保存)の和集合で、変更はプロセスを再起動せずに次の発言から効く。
//! 指定はカテゴリでもよいので、Channel が配下のチャンネルとスレッドを展開した ID の集合(`set_expanded`)も持ち、Core と tool は ID だけで判定できる
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock, RwLock};

use crate::judge::JudgeBatch;
use crate::store::{MessageRow, Store};
use crate::types::{ChannelId, GuildId, UserId};
use crate::{Error, Result};

#[derive(Clone)]
pub struct ChannelExclusions {
    inner: Arc<Inner>,
}

struct Inner {
    guild: GuildId,
    /// `attach` までは None で、コマンドの変更はメモリにだけ残る
    store: OnceLock<Store>,
    state: RwLock<State>,
    /// DB への書き込みとメモリへの反映を 1 組にして、同時の変更で食い違わないようにする
    writes: tokio::sync::Mutex<()>,
    /// config とコマンドで指定した集合が変わるたびに増える。Channel が展開を作り直す合図で、展開の更新では増えない
    changes: tokio::sync::watch::Sender<u64>,
    /// 展開を作れていない間は true。カテゴリの配下かが分からず、除外すべき発言を取り込みうるので、その間は復元と昇格を止める
    expansion_pending: tokio::sync::watch::Sender<bool>,
}

#[derive(Default)]
struct State {
    config: BTreeSet<ChannelId>,
    commanded: BTreeSet<ChannelId>,
    expanded: BTreeSet<ChannelId>,
}

impl State {
    fn contains(&self, id: &ChannelId) -> bool {
        self.config.contains(id) || self.commanded.contains(id) || self.expanded.contains(id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddOutcome {
    Added,
    AlreadyInConfig,
    AlreadyCommanded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    /// コマンド由来の行は消したが、config にも書かれているので除外は続く
    RemovedStillInConfig,
    /// config にだけある。コマンドでは外せない
    ConfigOnly,
    NotIgnored,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExclusionList {
    pub config: Vec<ChannelId>,
    pub commanded: Vec<ChannelId>,
}

impl ChannelExclusions {
    pub fn new(guild: GuildId) -> Self {
        Self {
            inner: Arc::new(Inner {
                guild,
                store: OnceLock::new(),
                state: RwLock::default(),
                writes: tokio::sync::Mutex::new(()),
                changes: tokio::sync::watch::channel(0).0,
                expansion_pending: tokio::sync::watch::channel(false).0,
            }),
        }
    }

    /// DB に保存済みのコマンド由来の除外を読み込み、以降の変更の保存先にする。起動時に 1 回だけ呼ぶ
    pub async fn attach(&self, store: Store) -> Result<()> {
        let rows = store.ignored_channels(self.inner.guild).await?;
        self.inner
            .store
            .set(store)
            .map_err(|_| Error::Other("チャンネルの除外には保存先が設定済みです".into()))?;
        self.write_state().commanded = rows.into_iter().map(|r| r.channel_id).collect();
        self.notify_changed();
        self.purge().await;
        Ok(())
    }

    /// config の `ignored_channels`。起動時に Channel が設定する
    pub fn set_config(&self, ids: impl IntoIterator<Item = ChannelId>) {
        self.write_state().config = ids.into_iter().collect();
        self.notify_changed();
    }

    /// 指定(config とコマンド)の配下を展開した、実際に除外するチャンネルとスレッドの ID。Channel が作り直すたびに丸ごと置き換える。
    /// 展開に入っていない新しいチャンネルやスレッドは、Channel が受信時に親をたどって判定する
    pub async fn set_expanded(&self, ids: impl IntoIterator<Item = ChannelId>) {
        let ids: BTreeSet<ChannelId> = ids.into_iter().collect();
        let changed = {
            let mut state = self.write_state();
            let changed = state.expanded != ids;
            state.expanded = ids;
            changed
        };
        // イベントのたびに呼ばれるが、集合が同じなら隠す行は増えていない
        if changed {
            self.purge().await;
        }
        // 隠し終えてから解く。先に解くと、止めていた復元が隠す前の行を拾う
        self.inner.expansion_pending.send_replace(false);
    }

    /// 展開を作れなかったときに Channel が呼ぶ。次の `set_expanded` まで復元と昇格を止める。指定が空なら展開は要らないので何もしない
    pub fn mark_expansion_pending(&self) {
        if !self.is_empty() {
            self.inner.expansion_pending.send_replace(true);
        }
    }

    pub fn expansion_pending(&self) -> bool {
        *self.inner.expansion_pending.borrow()
    }

    pub async fn wait_expanded(&self) {
        let mut rx = self.inner.expansion_pending.subscribe();
        let _ = rx.wait_for(|pending| !*pending).await;
    }

    /// Layer 3 に入る前で、まだ隠していない発言があるチャンネル。展開に載らないチャンネル(アーカイブ済みのスレッドなど)の洗い出し用
    pub async fn unpromoted_channels(&self) -> Result<Vec<ChannelId>> {
        match self.inner.store.get() {
            Some(store) => store.unpromoted_channels(self.inner.guild).await,
            None => Ok(Vec::new()),
        }
    }

    /// 指定が変わったときに通知が届く。受け取った Channel は展開を作り直す
    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }

    fn notify_changed(&self) {
        self.inner.changes.send_modify(|n| *n += 1);
    }

    /// 除外したチャンネルに残る、Layer 3 に入る前の発言を、二度と判定・昇格されない状態にする
    async fn purge(&self) {
        let Some(store) = self.inner.store.get() else {
            return;
        };
        let ids = self.effective();
        if ids.is_empty() {
            return;
        }
        match store.hide_channel_messages(self.inner.guild, &ids).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(count = n, "除外したチャンネルの未処理の発言を外しました"),
            Err(e) => {
                tracing::warn!(error = %e, "除外したチャンネルの未処理の発言を外せませんでした")
            }
        }
    }

    /// 展開を含め、ID だけで除外対象と分かるか
    pub fn is_channel_excluded(&self, channel: ChannelId) -> bool {
        self.read_state().contains(&channel)
    }

    /// 判定に回す前の発言群から、除外したチャンネルのものを外す。キューに残っていた分が、除外の確定後に届いたときのため。
    /// 外した行は DB でも外し、除外を後で解いても拾われないようにする
    pub async fn filter_batch(&self, mut batch: JudgeBatch) -> JudgeBatch {
        if !self.is_channel_excluded(batch.channel_id) {
            return batch;
        }
        let ids: Vec<i64> = batch.messages.iter().map(|m| m.row_id).collect();
        self.hide_rows(&ids).await;
        batch.messages.clear();
        batch
    }

    /// 昇格の前の未昇格バッファから、除外したチャンネルの行を外す。理由は `filter_batch` と同じ。
    /// 展開を作れていない間は、どの行が除外対象か分からないので全部を保留にする(隠さず、次の昇格で拾う)
    pub async fn filter_rows(&self, rows: Vec<MessageRow>) -> Vec<MessageRow> {
        if self.expansion_pending() {
            return Vec::new();
        }
        let (dropped, kept): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .partition(|r| self.is_channel_excluded(r.channel_id));
        if !dropped.is_empty() {
            let ids: Vec<i64> = dropped.iter().map(|r| r.id).collect();
            self.hide_rows(&ids).await;
        }
        kept
    }

    async fn hide_rows(&self, ids: &[i64]) {
        let Some(store) = self.inner.store.get() else {
            return;
        };
        if let Err(e) = store.hide_messages(ids).await {
            tracing::warn!(error = %e, "除外したチャンネルの発言を外せませんでした");
        }
    }

    /// `chain` は発言のあるチャンネル、その親(カテゴリやスレッドの親チャンネル)、さらにその親の ID。
    /// どれか 1 つでも除外に入っていれば true
    pub fn is_excluded(&self, chain: &[ChannelId]) -> bool {
        let state = self.read_state();
        chain.iter().any(|id| state.contains(id))
    }

    pub fn is_empty(&self) -> bool {
        let state = self.read_state();
        state.config.is_empty() && state.commanded.is_empty()
    }

    /// 実際に除外するチャンネルの ID(config とコマンドの指定と、その展開の和集合)。昇順
    pub fn effective(&self) -> Vec<ChannelId> {
        let state = self.read_state();
        let mut all: BTreeSet<ChannelId> = state.config.union(&state.commanded).copied().collect();
        all.extend(state.expanded.iter().copied());
        all.into_iter().collect()
    }

    pub fn list(&self) -> ExclusionList {
        let state = self.read_state();
        ExclusionList {
            config: state.config.iter().copied().collect(),
            commanded: state.commanded.iter().copied().collect(),
        }
    }

    /// 保存に失敗したときはメモリも変えない。効く状態と再起動後の状態を食い違わせないため
    pub async fn add(&self, channel: ChannelId, by: UserId) -> Result<AddOutcome> {
        let _guard = self.inner.writes.lock().await;
        {
            let state = self.read_state();
            if state.config.contains(&channel) {
                return Ok(AddOutcome::AlreadyInConfig);
            }
            if state.commanded.contains(&channel) {
                return Ok(AddOutcome::AlreadyCommanded);
            }
        }
        if let Some(store) = self.inner.store.get() {
            store
                .add_ignored_channel(self.inner.guild, channel, by)
                .await?;
        }
        self.write_state().commanded.insert(channel);
        self.notify_changed();
        self.purge().await;
        Ok(AddOutcome::Added)
    }

    pub async fn remove(&self, channel: ChannelId) -> Result<RemoveOutcome> {
        let _guard = self.inner.writes.lock().await;
        let (in_config, commanded) = {
            let state = self.read_state();
            (
                state.config.contains(&channel),
                state.commanded.contains(&channel),
            )
        };
        if !commanded {
            return Ok(if in_config {
                RemoveOutcome::ConfigOnly
            } else {
                RemoveOutcome::NotIgnored
            });
        }
        if let Some(store) = self.inner.store.get() {
            store
                .remove_ignored_channel(self.inner.guild, channel)
                .await?;
        }
        self.write_state().commanded.remove(&channel);
        self.notify_changed();
        Ok(if in_config {
            RemoveOutcome::RemovedStillInConfig
        } else {
            RemoveOutcome::Removed
        })
    }

    // ロックを持ったまま panic した後も除外を読めるように、poison は無視する
    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.inner.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.inner.state.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for ChannelExclusions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelExclusions")
            .field("guild", &self.inner.guild)
            .field("list", &self.list())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: GuildId = 1;

    #[tokio::test]
    async fn effective_is_the_union_and_matches_any_ancestor() {
        let ex = ChannelExclusions::new(G);
        assert!(ex.is_empty());
        assert!(!ex.is_excluded(&[10, 20, 30]));
        ex.set_config([30]);
        assert_eq!(ex.add(20, 9).await.unwrap(), AddOutcome::Added);
        assert_eq!(ex.effective(), vec![20, 30]);
        assert!(ex.is_excluded(&[10, 20]));
        assert!(ex.is_excluded(&[10, 99, 30]));
        assert!(!ex.is_excluded(&[10, 99]));
    }

    #[tokio::test]
    async fn config_entries_cannot_be_removed_by_command() {
        let ex = ChannelExclusions::new(G);
        ex.set_config([30]);
        assert_eq!(ex.add(30, 9).await.unwrap(), AddOutcome::AlreadyInConfig);
        assert_eq!(ex.remove(30).await.unwrap(), RemoveOutcome::ConfigOnly);
        assert!(ex.is_excluded(&[30]));
        assert_eq!(ex.remove(31).await.unwrap(), RemoveOutcome::NotIgnored);
    }

    #[tokio::test]
    async fn add_and_remove_take_effect_immediately() {
        let ex = ChannelExclusions::new(G);
        assert_eq!(ex.add(5, 9).await.unwrap(), AddOutcome::Added);
        assert_eq!(ex.add(5, 9).await.unwrap(), AddOutcome::AlreadyCommanded);
        assert!(ex.is_excluded(&[5]));
        assert_eq!(ex.remove(5).await.unwrap(), RemoveOutcome::Removed);
        assert!(!ex.is_excluded(&[5]));
    }

    #[tokio::test]
    async fn commanded_entries_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("noth-exclusion-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("noth.db");

        let first = ChannelExclusions::new(G);
        first
            .attach(Store::open(&path).await.unwrap())
            .await
            .unwrap();
        first.add(5, 9).await.unwrap();
        first.add(6, 9).await.unwrap();
        first.remove(6).await.unwrap();

        let second = ChannelExclusions::new(G);
        second
            .attach(Store::open(&path).await.unwrap())
            .await
            .unwrap();
        assert_eq!(second.list().commanded, vec![5]);
        assert!(second.is_excluded(&[5]));
        assert!(!second.is_excluded(&[6]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn removing_a_commanded_entry_also_in_config_keeps_it_excluded() {
        let ex = ChannelExclusions::new(G);
        assert_eq!(ex.add(5, 9).await.unwrap(), AddOutcome::Added);
        ex.set_config([5]);
        assert_eq!(
            ex.remove(5).await.unwrap(),
            RemoveOutcome::RemovedStillInConfig
        );
        assert!(ex.is_excluded(&[5]));
        assert!(ex.list().commanded.is_empty());
    }

    #[tokio::test]
    async fn a_failed_save_leaves_memory_unchanged() {
        let store = Store::open_memory().await.unwrap();
        let ex = ChannelExclusions::new(G);
        ex.attach(store.clone()).await.unwrap();
        // 保存先を壊して書き込みを失敗させる
        store.drop_ignored_channels_for_test().await;
        assert!(ex.add(5, 9).await.is_err());
        assert!(!ex.is_excluded(&[5]));
    }

    fn incoming(message_id: u64, channel_id: u64) -> crate::types::IncomingMessage {
        crate::types::IncomingMessage {
            guild_id: G,
            channel_id,
            channel_name: "c".into(),
            message_id,
            user_id: 5,
            author_name: "a".into(),
            content: "x".into(),
            timestamp: jiff::Timestamp::from_millisecond(1_700_000_000_000 + message_id as i64)
                .unwrap(),
            is_self: false,
            mentions_self: false,
            reply_to_self: false,
            reply_to: None,
            bot_marker: None,
        }
    }

    #[tokio::test]
    async fn expansion_is_matched_by_channel_id_and_replaced_wholesale() {
        let ex = ChannelExclusions::new(G);
        ex.set_config([1]);
        ex.set_expanded([1, 2, 3]).await;
        assert!(ex.is_channel_excluded(3));
        assert!(ex.is_excluded(&[99, 2]));
        assert_eq!(ex.effective(), vec![1, 2, 3]);
        ex.set_expanded([1]).await;
        assert!(!ex.is_channel_excluded(3));
        assert_eq!(ex.effective(), vec![1]);
    }

    #[tokio::test]
    async fn only_changes_to_the_specification_notify_the_channel() {
        let ex = ChannelExclusions::new(G);
        let mut rx = ex.changes();
        rx.mark_unchanged();
        ex.set_expanded([5]).await;
        assert!(!rx.has_changed().unwrap());
        ex.add(5, 9).await.unwrap();
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        ex.remove(5).await.unwrap();
        assert!(rx.has_changed().unwrap());
    }

    #[tokio::test]
    async fn unprocessed_messages_in_excluded_channels_are_dropped_from_every_queue() {
        let store = Store::open_memory().await.unwrap();
        let ex = ChannelExclusions::new(G);
        ex.attach(store.clone()).await.unwrap();
        let kept = store.insert_message(&incoming(1, 20)).await.unwrap();
        let in_category = store.insert_message(&incoming(2, 10)).await.unwrap();

        // 展開で初めて分かる除外でも、再起動の復元や昇格の対象から外れる
        ex.set_expanded([10]).await;
        let unjudged: Vec<i64> = store
            .unjudged_messages(G)
            .await
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(unjudged, vec![kept]);

        // 外す前にキューへ入っていた判定の束
        let pending = |row_id| crate::judge::PendingMessage {
            row_id,
            message_id: 7,
            author_name: "a".into(),
            content: "x".into(),
            at: jiff::Timestamp::UNIX_EPOCH,
            mentions_self: false,
            reply_to_self: false,
            reply_to: None,
        };
        let batch = |channel_id, row_id| crate::judge::JudgeBatch {
            channel_id,
            channel_name: "c".into(),
            messages: vec![pending(row_id)],
        };
        assert!(
            ex.filter_batch(batch(10, in_category))
                .await
                .messages
                .is_empty()
        );
        assert_eq!(ex.filter_batch(batch(20, kept)).await.messages.len(), 1);

        // 判定済みで未昇格だった行
        let late = store.insert_message(&incoming(3, 30)).await.unwrap();
        let decision = store
            .insert_decision(G, "judge", "m", &serde_json::json!({}), 1)
            .await
            .unwrap();
        let chunk = store
            .create_chunk(G, 30, jiff::Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        store
            .mark_judged(&[late], decision, chunk, false)
            .await
            .unwrap();
        assert_eq!(store.buffered_messages(G).await.unwrap().len(), 1);
        ex.set_expanded([10, 30]).await;
        assert!(store.buffered_messages(G).await.unwrap().is_empty());

        let rows = vec![store.message(kept).await.unwrap().unwrap()];
        assert_eq!(ex.filter_rows(rows).await.len(), 1);
        let rows = vec![store.message(late).await.unwrap().unwrap()];
        assert!(ex.filter_rows(rows).await.is_empty());
    }

    #[tokio::test]
    async fn rows_are_hidden_only_when_the_expansion_changes() {
        let store = Store::open_memory().await.unwrap();
        let ex = ChannelExclusions::new(G);
        ex.attach(store.clone()).await.unwrap();
        ex.set_expanded([10]).await;

        // 受信の検査を通らずに入った行。同じ集合の再設定が隠す処理を走らせなければ残る
        let row = store.insert_message(&incoming(1, 10)).await.unwrap();
        ex.set_expanded([10]).await;
        assert_eq!(store.unjudged_messages(G).await.unwrap().len(), 1);

        ex.set_expanded([10, 11]).await;
        assert!(store.unjudged_messages(G).await.unwrap().is_empty());
        assert!(store.message(row).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn promotion_is_held_while_the_expansion_is_pending_and_resumes_after() {
        let store = Store::open_memory().await.unwrap();
        let ex = ChannelExclusions::new(G);
        ex.attach(store.clone()).await.unwrap();

        // 指定が無ければ展開は要らないので止めない
        ex.mark_expansion_pending();
        assert!(!ex.expansion_pending());

        ex.set_config([1]);
        ex.mark_expansion_pending();
        assert!(ex.expansion_pending());

        let id = store.insert_message(&incoming(1, 20)).await.unwrap();
        let rows = vec![store.message(id).await.unwrap().unwrap()];
        assert!(ex.filter_rows(rows.clone()).await.is_empty());
        // 保留は隠さない
        assert_eq!(store.unjudged_messages(G).await.unwrap().len(), 1);

        let waiter = {
            let ex = ex.clone();
            tokio::spawn(async move { ex.wait_expanded().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        ex.set_expanded([1, 2]).await;
        waiter.await.unwrap();
        assert!(!ex.expansion_pending());
        assert_eq!(ex.filter_rows(rows).await.len(), 1);
    }

    #[tokio::test]
    async fn unpromoted_channels_lists_only_visible_unpromoted_rows() {
        let store = Store::open_memory().await.unwrap();
        let ex = ChannelExclusions::new(G);
        assert!(ex.unpromoted_channels().await.unwrap().is_empty());
        ex.attach(store.clone()).await.unwrap();
        for (m, c) in [(1, 10), (2, 10), (3, 20), (4, 30)] {
            store.insert_message(&incoming(m, c)).await.unwrap();
        }
        store.hide_channel_messages(G, &[30]).await.unwrap();
        let mut got = ex.unpromoted_channels().await.unwrap();
        got.sort();
        assert_eq!(got, vec![10, 20]);
    }
}
