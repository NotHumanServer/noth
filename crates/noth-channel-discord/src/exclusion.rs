//! 発言が除外したチャンネルのものかの判定と、除外の指定(チャンネル・カテゴリ)を実際のチャンネルとスレッドの ID に展開する処理
use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use noth_core::exclusion::ChannelExclusions;
use serenity::all as sn;
use serenity::cache::Cache;
use serenity::prelude::Context;

/// 親を引けなかったチャンネルを、この間は引き直さない。引けない状態の発言が続いても REST を連打しない
const UNRESOLVED_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChannelInfo {
    pub(crate) parent: Option<u64>,
    pub(crate) is_thread: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChannelNode {
    pub(crate) id: u64,
    pub(crate) info: ChannelInfo,
}

pub(crate) struct Resolver {
    exclusions: ChannelExclusions,
    unresolved: Mutex<HashMap<u64, Instant>>,
    dropped: AtomicU64,
}

impl Resolver {
    pub(crate) fn new(exclusions: ChannelExclusions) -> Self {
        Self {
            exclusions,
            unresolved: Mutex::default(),
            dropped: AtomicU64::new(0),
        }
    }

    /// 除外の対象なら true。展開に入っていないチャンネルやスレッドは、親をたどって調べる。
    /// 親を引けないときも true にする。除外していない側に倒すと、除外したはずの発言を取り込んでしまうため
    pub(crate) async fn is_excluded<F, Fut>(
        &self,
        channel_id: u64,
        now: Instant,
        mut info_of: F,
    ) -> bool
    where
        F: FnMut(u64) -> Fut,
        Fut: Future<Output = Option<ChannelInfo>>,
    {
        // 除外が無い間は親を引かない。通常運用の発言ごとの問い合わせを避ける
        if self.exclusions.is_empty() {
            return false;
        }
        let mut chain = vec![channel_id];
        if self.exclusions.is_excluded(&chain) {
            return true;
        }
        let Some(info) = self.resolve(channel_id, now, &mut info_of).await else {
            return true;
        };
        let Some(parent) = info.parent else {
            // 親の無いスレッドは、どのカテゴリのものか分からない
            if info.is_thread {
                self.count_drop(channel_id, "親の無いスレッド");
            }
            return info.is_thread;
        };
        chain.push(parent);
        if self.exclusions.is_excluded(&chain) {
            return true;
        }
        if !info.is_thread {
            return false;
        }
        // スレッドの親チャンネルのカテゴリ
        match self.resolve(parent, now, &mut info_of).await {
            Some(ChannelInfo {
                parent: Some(grandparent),
                ..
            }) => {
                chain.push(grandparent);
                self.exclusions.is_excluded(&chain)
            }
            Some(_) => false,
            None => true,
        }
    }

    async fn resolve<F, Fut>(&self, id: u64, now: Instant, info_of: &mut F) -> Option<ChannelInfo>
    where
        F: FnMut(u64) -> Fut,
        Fut: Future<Output = Option<ChannelInfo>>,
    {
        let recently_failed = self
            .unresolved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .is_some_and(|at| now.saturating_duration_since(*at) < UNRESOLVED_TTL);
        if recently_failed {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let info = info_of(id).await;
        if info.is_none() {
            self.unresolved
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, now);
            self.count_drop(id, "親を調べられない");
        }
        info
    }

    fn count_drop(&self, channel_id: u64, reason: &str) {
        let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::warn!(
            channel_id,
            dropped_total = total,
            reason,
            "除外の対象か確かめられないため、除外したものとして発言を捨てました(同じチャンネルは {} 秒間、引き直しません)",
            UNRESOLVED_TTL.as_secs()
        );
    }

    #[cfg(test)]
    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// 指定(`direct`)の配下を展開した ID の集合。指定そのものと、配下のチャンネル、それらのスレッドを含む。
/// 親の無いスレッドは、カテゴリを確かめられないので入れる
pub(crate) fn expand(direct: &[u64], nodes: &[ChannelNode]) -> BTreeSet<u64> {
    if direct.is_empty() {
        return BTreeSet::new();
    }
    let direct: HashSet<u64> = direct.iter().copied().collect();
    let parent_of: HashMap<u64, Option<u64>> =
        nodes.iter().map(|n| (n.id, n.info.parent)).collect();
    let mut out: BTreeSet<u64> = direct.iter().copied().collect();
    for n in nodes {
        let in_direct_parent = n.info.parent.is_some_and(|p| direct.contains(&p));
        let in_direct_grandparent = n.info.is_thread
            && n.info
                .parent
                .and_then(|p| parent_of.get(&p).copied().flatten())
                .is_some_and(|g| direct.contains(&g));
        let orphan_thread = n.info.is_thread && n.info.parent.is_none();
        if direct.contains(&n.id) || in_direct_parent || in_direct_grandparent || orphan_thread {
            out.insert(n.id);
        }
    }
    out
}

/// 設定した ID のうち、guild のチャンネルにもスレッドにも見当たらないもの。誤記を起動時に知らせるため
pub(crate) fn unknown_ids(configured: &[u64], known: &[ChannelNode]) -> Vec<u64> {
    let known: HashSet<u64> = known.iter().map(|n| n.id).collect();
    configured
        .iter()
        .copied()
        .filter(|id| !known.contains(id))
        .collect()
}

pub(crate) fn node_of(c: &sn::GuildChannel) -> ChannelNode {
    ChannelNode {
        id: c.id.get(),
        info: info_of_channel(c),
    }
}

fn info_of_channel(c: &sn::GuildChannel) -> ChannelInfo {
    ChannelInfo {
        parent: c.parent_id.map(|p| p.get()),
        is_thread: matches!(
            c.kind,
            sn::ChannelType::NewsThread
                | sn::ChannelType::PublicThread
                | sn::ChannelType::PrivateThread
        ),
    }
}

/// serenity のキャッシュを先に見て、無ければ REST で引く
pub(crate) async fn lookup(ctx: &Context, guild_id: u64, channel_id: u64) -> Option<ChannelInfo> {
    let id = sn::ChannelId::new(channel_id);
    let cached = ctx.cache.guild(guild_id).and_then(|g| {
        // スレッドは channels ではなく threads に入っている
        g.channels
            .get(&id)
            .or_else(|| g.threads.iter().find(|t| t.id == id))
            .map(info_of_channel)
    });
    if cached.is_some() {
        return cached;
    }
    match ctx.http.get_channel(id).await {
        Ok(sn::Channel::Guild(c)) => Some(info_of_channel(&c)),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(channel_id, error = %e, "チャンネルを取得できませんでした");
            None
        }
    }
}

/// キャッシュにあるチャンネルとスレッド。guild がまだキャッシュに無ければ None
pub(crate) fn nodes_from_cache(cache: &Cache, guild_id: u64) -> Option<Vec<ChannelNode>> {
    let g = cache.guild(guild_id)?;
    Some(
        g.channels
            .values()
            .chain(g.threads.iter())
            .map(node_of)
            .collect(),
    )
}

/// 指定が変わったとき、またはチャンネルやスレッドの構成が変わったときに呼ぶ。
/// guild がキャッシュに載る前は、REST で作った展開を空で上書きしないよう何もしない
pub(crate) async fn rebuild(exclusions: &ChannelExclusions, cache: &Cache, guild_id: u64) {
    let Some(nodes) = nodes_from_cache(cache, guild_id) else {
        return;
    };
    let list = exclusions.list();
    let direct: Vec<u64> = list.config.into_iter().chain(list.commanded).collect();
    exclusions.set_expanded(expand(&direct, &nodes)).await;
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const CATEGORY: u64 = 1;
    const TEXT: u64 = 2;
    const THREAD: u64 = 3;
    const OTHER_TEXT: u64 = 4;

    fn info(parent: Option<u64>, is_thread: bool) -> ChannelInfo {
        ChannelInfo { parent, is_thread }
    }

    fn channels() -> HashMap<u64, ChannelInfo> {
        HashMap::from([
            (CATEGORY, info(None, false)),
            (TEXT, info(Some(CATEGORY), false)),
            (THREAD, info(Some(TEXT), true)),
            (OTHER_TEXT, info(None, false)),
        ])
    }

    fn nodes() -> Vec<ChannelNode> {
        channels()
            .into_iter()
            .map(|(id, info)| ChannelNode { id, info })
            .collect()
    }

    async fn excluded(ex: &ChannelExclusions, id: u64) -> bool {
        let known = channels();
        Resolver::new(ex.clone())
            .is_excluded(id, Instant::now(), |c| {
                let info = known.get(&c).copied();
                async move { info }
            })
            .await
    }

    fn with(ids: &[u64]) -> ChannelExclusions {
        let ex = ChannelExclusions::new(1);
        ex.set_config(ids.iter().copied());
        ex
    }

    #[tokio::test]
    async fn nothing_is_excluded_by_default() {
        let ex = with(&[]);
        for id in [CATEGORY, TEXT, THREAD, OTHER_TEXT] {
            assert!(!excluded(&ex, id).await);
        }
    }

    #[tokio::test]
    async fn a_channel_covers_itself_and_its_threads_only() {
        let ex = with(&[TEXT]);
        assert!(excluded(&ex, TEXT).await);
        assert!(excluded(&ex, THREAD).await);
        assert!(!excluded(&ex, CATEGORY).await);
        assert!(!excluded(&ex, OTHER_TEXT).await);
    }

    #[tokio::test]
    async fn a_category_covers_its_channels_and_their_threads() {
        let ex = with(&[CATEGORY]);
        assert!(excluded(&ex, CATEGORY).await);
        assert!(excluded(&ex, TEXT).await);
        assert!(excluded(&ex, THREAD).await);
        assert!(!excluded(&ex, OTHER_TEXT).await);
    }

    #[tokio::test]
    async fn a_thread_can_be_excluded_on_its_own() {
        let ex = with(&[THREAD]);
        assert!(excluded(&ex, THREAD).await);
        assert!(!excluded(&ex, TEXT).await);
    }

    #[tokio::test]
    async fn an_unresolvable_parent_counts_as_excluded() {
        let ex = with(&[CATEGORY]);
        assert!(excluded(&ex, 999).await);
        // スレッドの親チャンネルを引けない
        let broken = |c: u64| async move { (c == THREAD).then_some(info(Some(TEXT), true)) };
        assert!(
            Resolver::new(ex)
                .is_excluded(THREAD, Instant::now(), broken)
                .await
        );
    }

    #[tokio::test]
    async fn a_thread_without_a_parent_is_excluded_but_a_parentless_channel_is_not() {
        let ex = with(&[CATEGORY]);
        let resolver = Resolver::new(ex);
        let lookup = |c: u64| async move {
            match c {
                10 => Some(info(None, true)),
                11 => Some(info(None, false)),
                _ => None,
            }
        };
        assert!(resolver.is_excluded(10, Instant::now(), lookup).await);
        assert!(!resolver.is_excluded(11, Instant::now(), lookup).await);
    }

    #[tokio::test]
    async fn no_lookup_happens_while_nothing_is_excluded() {
        let ex = with(&[]);
        let resolver = Resolver::new(ex);
        assert!(
            !resolver
                .is_excluded(999, Instant::now(), |_| async { panic!("lookup") })
                .await
        );
    }

    #[tokio::test]
    async fn changes_apply_to_the_next_message() {
        let ex = with(&[]);
        assert!(!excluded(&ex, TEXT).await);
        ex.add(CATEGORY, 9).await.unwrap();
        assert!(excluded(&ex, TEXT).await);
        ex.remove(CATEGORY).await.unwrap();
        assert!(!excluded(&ex, TEXT).await);
    }

    #[tokio::test]
    async fn an_unresolved_channel_is_not_looked_up_again_until_the_ttl_passes() {
        let resolver = Resolver::new(with(&[CATEGORY]));
        let calls = Cell::new(0);
        let failing = |_: u64| {
            calls.set(calls.get() + 1);
            async { None }
        };
        let t0 = Instant::now();
        assert!(resolver.is_excluded(999, t0, failing).await);
        assert!(
            resolver
                .is_excluded(999, t0 + Duration::from_secs(30), failing)
                .await
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(resolver.dropped(), 2);

        // 期限を過ぎたら引き直し、解決できれば除外に倒さない
        let later = t0 + UNRESOLVED_TTL + Duration::from_secs(1);
        let ok = |_: u64| {
            calls.set(calls.get() + 1);
            async { Some(info(None, false)) }
        };
        assert!(!resolver.is_excluded(999, later, ok).await);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn expansion_covers_category_members_and_threads_of_excluded_channels() {
        // 5 はカテゴリ 1 の外のチャンネルで、6 はそのスレッド
        let mut all = nodes();
        all.push(ChannelNode {
            id: 5,
            info: info(None, false),
        });
        all.push(ChannelNode {
            id: 6,
            info: info(Some(5), true),
        });
        assert_eq!(
            expand(&[CATEGORY], &all),
            BTreeSet::from([CATEGORY, TEXT, THREAD])
        );
        assert_eq!(expand(&[5], &all), BTreeSet::from([5, 6]));
        assert_eq!(expand(&[THREAD], &all), BTreeSet::from([THREAD]));
        assert!(expand(&[], &all).is_empty());
    }

    #[test]
    fn expansion_includes_orphan_threads() {
        let all = vec![ChannelNode {
            id: 9,
            info: info(None, true),
        }];
        assert_eq!(expand(&[CATEGORY], &all), BTreeSet::from([CATEGORY, 9]));
    }

    #[tokio::test]
    async fn expansion_is_rebuilt_when_the_specification_or_the_layout_changes() {
        let ex = ChannelExclusions::new(1);
        let mut all = nodes();
        ex.add(CATEGORY, 9).await.unwrap();
        ex.set_expanded(expand(&[CATEGORY], &all)).await;
        assert!(ex.is_channel_excluded(THREAD));

        // 構成が変わった(チャンネルが別のカテゴリへ移った)あとの作り直し
        all.iter_mut().find(|n| n.id == TEXT).unwrap().info.parent = None;
        ex.set_expanded(expand(&[CATEGORY], &all)).await;
        assert!(!ex.is_channel_excluded(TEXT));
        assert!(!ex.is_channel_excluded(THREAD));

        // 指定が外れたあとの作り直し
        ex.remove(CATEGORY).await.unwrap();
        ex.set_expanded(expand(&[], &all)).await;
        assert!(ex.effective().is_empty());
    }

    #[test]
    fn configured_ids_missing_from_the_guild_are_reported() {
        assert_eq!(unknown_ids(&[TEXT, 777], &nodes()), vec![777]);
        assert!(unknown_ids(&[], &nodes()).is_empty());
    }
}
