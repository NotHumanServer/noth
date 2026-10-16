//! チャンネルの除外。除外したチャンネルは Noth にとって存在しないものとして扱い、受信・判定・Layer 3・tool の参照・検索のどれにも出さない。
//! 受信する Channel と参照する tool が同じ状態を見るために、ここで 1 つの共有状態を持つ。
//! 実効の除外は config(`ignored_channels`)とコマンド(DB に保存)の和集合で、変更はプロセスを再起動せずに次の発言から効く
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock, RwLock};

use crate::store::Store;
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
}

#[derive(Default)]
struct State {
    config: BTreeSet<ChannelId>,
    commanded: BTreeSet<ChannelId>,
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
        let mut state = self.write_state();
        state.commanded = rows.into_iter().map(|r| r.channel_id).collect();
        Ok(())
    }

    /// config の `ignored_channels`。起動時に Channel が設定する
    pub fn set_config(&self, ids: impl IntoIterator<Item = ChannelId>) {
        self.write_state().config = ids.into_iter().collect();
    }

    /// `chain` は発言のあるチャンネル、その親(カテゴリやスレッドの親チャンネル)、さらにその親の ID。
    /// どれか 1 つでも除外に入っていれば true
    pub fn is_excluded(&self, chain: &[ChannelId]) -> bool {
        let state = self.read_state();
        chain
            .iter()
            .any(|id| state.config.contains(id) || state.commanded.contains(id))
    }

    pub fn is_empty(&self) -> bool {
        let state = self.read_state();
        state.config.is_empty() && state.commanded.is_empty()
    }

    /// 実効の除外(config とコマンドの和集合)。昇順
    pub fn effective(&self) -> Vec<ChannelId> {
        let state = self.read_state();
        state.config.union(&state.commanded).copied().collect()
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
}
