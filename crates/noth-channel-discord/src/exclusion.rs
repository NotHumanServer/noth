//! 発言が除外したチャンネルのものかの判定と、除外の指定(チャンネル・カテゴリ)を実際のチャンネルとスレッドの ID に展開する処理
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Display;
use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use noth_core::exclusion::ChannelExclusions;
use serenity::all as sn;
use serenity::cache::Cache;
use serenity::http::Http;
use serenity::prelude::Context;

/// 親を引けなかったチャンネルを、この間は引き直さない。引けない状態の発言が続いても REST を連打しない
const UNRESOLVED_TTL: Duration = Duration::from_secs(60);

/// 引けたチャンネルの親を、この間は引き直さない。変更はイベントで `forget` するので、取りこぼしの保険の期限
const RESOLVED_TTL: Duration = Duration::from_secs(600);
const RESOLVED_PRUNE_AT: usize = 1024;

/// 起動時の REST の失敗を、短い間隔で数回やり直す。ここで諦めると、展開が済むまで復元と昇格を止めることになる
pub(crate) const REST_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// `op` が失敗したら `delays` の間隔でやり直す。尽きたら最後のエラーを返す
pub(crate) async fn retry<T, E, F, Fut>(delays: &[Duration], mut op: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: Display,
{
    let mut delays = delays.iter();
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => match delays.next() {
                Some(delay) => {
                    tracing::warn!(error = %e, retry_in_ms = delay.as_millis() as u64, "REST に失敗したため、やり直します");
                    tokio::time::sleep(*delay).await;
                }
                None => return Err(e),
            },
        }
    }
}

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
    resolved: Mutex<HashMap<u64, (ChannelInfo, Instant)>>,
    /// 一覧に無いのに除外の配下と分かったチャンネル。行を隠した後は未処理の行が無くなって候補から外れるが、展開には残し続ける
    unlisted_excluded: Mutex<BTreeSet<u64>>,
    dropped: AtomicU64,
    /// 展開の作り直しを 1 つずつにする。イベントが重なると、古い構成から作った展開が新しい展開を上書きしうるため
    rebuilding: tokio::sync::Mutex<()>,
}

impl Resolver {
    pub(crate) fn new(exclusions: ChannelExclusions) -> Self {
        Self {
            exclusions,
            unresolved: Mutex::default(),
            resolved: Mutex::default(),
            unlisted_excluded: Mutex::default(),
            dropped: AtomicU64::new(0),
            rebuilding: tokio::sync::Mutex::new(()),
        }
    }

    /// チャンネルが作られた・変わった・消えたときに、そのチャンネルの親や取得失敗の記憶を捨てる
    pub(crate) fn forget(&self, channel_id: u64) {
        self.resolved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&channel_id);
        self.unresolved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&channel_id);
    }

    pub(crate) fn forget_all(&self) {
        self.resolved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.unresolved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// 除外の対象なら true。展開に入っていないチャンネルやスレッドは、親をたどって調べる。
    /// 親を引けないときも true にする。除外していない側に倒すと、除外したはずの発言を取り込んでしまうため
    pub(crate) async fn is_excluded<F, Fut>(
        &self,
        channel_id: u64,
        now: Instant,
        info_of: F,
    ) -> bool
    where
        F: FnMut(u64) -> Fut,
        Fut: Future<Output = Option<ChannelInfo>>,
    {
        // 除外が無い間は親を引かない。通常運用の発言ごとの問い合わせを避ける
        if self.exclusions.is_empty() {
            return false;
        }
        self.is_excluded_by(
            |chain| self.exclusions.is_excluded(chain),
            channel_id,
            now,
            info_of,
        )
        .await
    }

    /// `covered` は、チャンネル・親・その親の ID の並びが除外に入っているかの判定
    async fn is_excluded_by<C, F, Fut>(
        &self,
        covered: C,
        channel_id: u64,
        now: Instant,
        mut info_of: F,
    ) -> bool
    where
        C: Fn(&[u64]) -> bool,
        F: FnMut(u64) -> Fut,
        Fut: Future<Output = Option<ChannelInfo>>,
    {
        let mut chain = vec![channel_id];
        if covered(&chain) {
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
        if covered(&chain) {
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
                covered(&chain)
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
        if let Some(info) = self.resolved_info(id, now) {
            return Some(info);
        }
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
        match info {
            Some(found) => self.remember(id, found, now),
            None => {
                self.unresolved
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, now);
                self.count_drop(id, "親を調べられない");
            }
        }
        info
    }

    fn resolved_info(&self, id: u64, now: Instant) -> Option<ChannelInfo> {
        let resolved = self.resolved.lock().unwrap_or_else(|e| e.into_inner());
        let (info, at) = resolved.get(&id)?;
        (now.saturating_duration_since(*at) < RESOLVED_TTL).then_some(*info)
    }

    fn remember(&self, id: u64, info: ChannelInfo, now: Instant) {
        let mut resolved = self.resolved.lock().unwrap_or_else(|e| e.into_inner());
        if resolved.len() >= RESOLVED_PRUNE_AT {
            resolved.retain(|_, (_, at)| now.saturating_duration_since(*at) < RESOLVED_TTL);
        }
        resolved.insert(id, (info, now));
    }

    /// 展開を作り直して置き換える。`snapshot` は今の構成(キャッシュにまだ guild が無ければ None で、何もしない)。
    /// 並行の作り直しは順に処理し、構成の取得もロックを取ってから行うので、最後に置かれるのは常に最新の構成からの展開になる
    pub(crate) async fn rebuild<S, SFut, F, Fut>(&self, snapshot: S, info_of: F)
    where
        S: FnOnce() -> SFut,
        SFut: Future<Output = Option<Vec<ChannelNode>>>,
        F: FnMut(u64) -> Fut,
        Fut: Future<Output = Option<ChannelInfo>>,
    {
        let _one_at_a_time = self.rebuilding.lock().await;
        let Some(nodes) = snapshot().await else {
            return;
        };
        self.apply(&nodes, Instant::now(), info_of).await;
    }

    async fn apply<F, Fut>(&self, nodes: &[ChannelNode], now: Instant, mut info_of: F)
    where
        F: FnMut(u64) -> Fut,
        Fut: Future<Output = Option<ChannelInfo>>,
    {
        let list = self.exclusions.list();
        let direct: HashSet<u64> = list.config.into_iter().chain(list.commanded).collect();
        let direct_ids: Vec<u64> = direct.iter().copied().collect();
        let mut set = expand(&direct_ids, nodes);
        if !direct.is_empty() {
            let unlisted = self
                .unlisted_under(&direct, &set, nodes, now, &mut info_of)
                .await;
            set.extend(unlisted);
        }
        self.exclusions.set_expanded(set).await;
    }

    /// 未昇格の行が残るチャンネルのうち、構成の一覧に無いもの(アーカイブ済みのスレッドなど)で、除外の配下にあるもの。
    /// 一覧に無いと展開からは分からず、そのまま復元や昇格に回ってしまうので、親を引いて確かめる
    async fn unlisted_under<F, Fut>(
        &self,
        direct: &HashSet<u64>,
        base: &BTreeSet<u64>,
        nodes: &[ChannelNode],
        now: Instant,
        info_of: &mut F,
    ) -> Vec<u64>
    where
        F: FnMut(u64) -> Fut,
        Fut: Future<Output = Option<ChannelInfo>>,
    {
        let stored = match self.exclusions.unpromoted_channels().await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(error = %e, "未処理の発言があるチャンネルを調べられませんでした");
                return Vec::new();
            }
        };
        let listed: HashMap<u64, ChannelInfo> = nodes.iter().map(|n| (n.id, n.info)).collect();
        let covered = |chain: &[u64]| {
            chain
                .iter()
                .any(|id| direct.contains(id) || base.contains(id))
        };
        let remembered = self
            .unlisted_excluded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut found = Vec::new();
        for id in stored.into_iter().chain(remembered) {
            if listed.contains_key(&id) || base.contains(&id) || found.contains(&id) {
                continue;
            }
            let lookup = |c: u64| {
                let known = listed.get(&c).copied();
                let fetch = known.is_none().then(|| info_of(c));
                async move {
                    match fetch {
                        Some(f) => f.await,
                        None => known,
                    }
                }
            };
            if self.is_excluded_by(covered, id, now, lookup).await {
                found.push(id);
            }
        }
        *self
            .unlisted_excluded
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = found.iter().copied().collect();
        found
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
    lookup_rest(&ctx.http, channel_id).await
}

pub(crate) async fn lookup_rest(http: &Http, channel_id: u64) -> Option<ChannelInfo> {
    match http.get_channel(sn::ChannelId::new(channel_id)).await {
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

    #[tokio::test]
    async fn retry_gives_up_after_the_last_delay_and_stops_at_the_first_success() {
        let delays = [Duration::from_millis(1); 3];
        let calls = Cell::new(0);
        let third_time_lucky = retry(&delays, || {
            calls.set(calls.get() + 1);
            let attempt = calls.get();
            async move {
                if attempt == 3 {
                    Ok(attempt)
                } else {
                    Err("down")
                }
            }
        })
        .await;
        assert_eq!(third_time_lucky, Ok(3));

        calls.set(0);
        let never: Result<(), &str> = retry(&delays, || {
            calls.set(calls.get() + 1);
            async { Err("down") }
        })
        .await;
        assert_eq!(never, Err("down"));
        // 最初の 1 回と、間隔の数だけのやり直し
        assert_eq!(calls.get(), 4);
    }

    #[tokio::test]
    async fn overlapping_rebuilds_leave_the_expansion_of_the_latest_layout() {
        let ex = with(&[CATEGORY]);
        let resolver = Resolver::new(ex.clone());
        let old = nodes();
        let mut moved = nodes();
        moved.iter_mut().find(|n| n.id == TEXT).unwrap().info.parent = None;
        let no_lookup = |_: u64| async { None };

        // 先に始まった方の構成の取得が遅くても、後から来た方が最後に置かれる
        let slow = resolver.rebuild(
            || async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                Some(old)
            },
            no_lookup,
        );
        let fast = async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            resolver
                .rebuild(|| async move { Some(moved) }, no_lookup)
                .await;
        };
        tokio::join!(slow, fast);
        // 順に処理されるので、後の構成(TEXT はカテゴリの外)が残る。取り違えると TEXT が除外に入ったままになる
        assert!(!ex.is_channel_excluded(TEXT));
    }

    fn stored_message(message_id: u64, channel_id: u64) -> noth_core::types::IncomingMessage {
        noth_core::types::IncomingMessage {
            guild_id: 1,
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
    async fn archived_threads_with_unprocessed_rows_are_resolved_by_their_parent() {
        const UNDER_CATEGORY: u64 = 90;
        const ELSEWHERE: u64 = 91;
        let store = noth_core::store::Store::open_memory().await.unwrap();
        let ex = with(&[CATEGORY]);
        ex.attach(store.clone()).await.unwrap();
        for (m, c) in [(1, UNDER_CATEGORY), (2, ELSEWHERE)] {
            store.insert_message(&stored_message(m, c)).await.unwrap();
        }
        // 一覧(アクティブなもの)に無いスレッド。親は一覧にある
        let archived = HashMap::from([
            (UNDER_CATEGORY, info(Some(TEXT), true)),
            (ELSEWHERE, info(Some(OTHER_TEXT), true)),
        ]);
        let calls = Cell::new(0);
        let lookup = |id: u64| {
            calls.set(calls.get() + 1);
            let found = archived.get(&id).copied();
            async move { found }
        };

        let resolver = Resolver::new(ex.clone());
        resolver.apply(&nodes(), Instant::now(), lookup).await;
        assert!(ex.is_channel_excluded(UNDER_CATEGORY));
        assert!(!ex.is_channel_excluded(ELSEWHERE));
        // 隠れるのは配下の行だけ
        let left: Vec<u64> = store
            .unjudged_messages(1)
            .await
            .unwrap()
            .iter()
            .map(|r| r.channel_id)
            .collect();
        assert_eq!(left, vec![ELSEWHERE]);
        assert_eq!(calls.get(), 2);

        // 次の作り直しでは、引けた分を REST で引き直さない
        resolver.apply(&nodes(), Instant::now(), lookup).await;
        assert!(ex.is_channel_excluded(UNDER_CATEGORY));
        assert_eq!(calls.get(), 2);

        // 構成が変わった(forget)ら引き直す
        resolver.forget(ELSEWHERE);
        resolver.apply(&nodes(), Instant::now(), lookup).await;
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn an_unlisted_channel_whose_parent_cannot_be_found_is_excluded() {
        let store = noth_core::store::Store::open_memory().await.unwrap();
        let ex = with(&[CATEGORY]);
        ex.attach(store.clone()).await.unwrap();
        store.insert_message(&stored_message(1, 77)).await.unwrap();
        let resolver = Resolver::new(ex.clone());
        resolver
            .apply(&nodes(), Instant::now(), |_| async { None })
            .await;
        assert!(ex.is_channel_excluded(77));
        assert!(store.unjudged_messages(1).await.unwrap().is_empty());
    }
}
