//! 発言が除外したチャンネルのものかの判定。カテゴリ(チャンネルの親)と、チャンネルのスレッド(スレッドの親)を含む
use std::future::Future;

use noth_core::exclusion::ChannelExclusions;
use serenity::all as sn;
use serenity::prelude::Context;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChannelInfo {
    pub(crate) parent: Option<u64>,
    pub(crate) is_thread: bool,
}

/// 除外の対象なら true。親を引けないときも true にする。除外していない側に倒すと、除外したはずの発言を取り込んでしまうため
pub(crate) async fn is_excluded<F, Fut>(
    exclusions: &ChannelExclusions,
    channel_id: u64,
    mut info_of: F,
) -> bool
where
    F: FnMut(u64) -> Fut,
    Fut: Future<Output = Option<ChannelInfo>>,
{
    // 除外が無い間は親を引かない。通常運用の発言ごとの問い合わせを避ける
    if exclusions.is_empty() {
        return false;
    }
    let mut chain = vec![channel_id];
    if exclusions.is_excluded(&chain) {
        return true;
    }
    let Some(info) = info_of(channel_id).await else {
        warn_unresolved(channel_id);
        return true;
    };
    let Some(parent) = info.parent else {
        return false;
    };
    chain.push(parent);
    if exclusions.is_excluded(&chain) {
        return true;
    }
    if !info.is_thread {
        return false;
    }
    // スレッドの親チャンネルのカテゴリ
    match info_of(parent).await {
        Some(ChannelInfo {
            parent: Some(grandparent),
            ..
        }) => {
            chain.push(grandparent);
            exclusions.is_excluded(&chain)
        }
        Some(_) => false,
        None => {
            warn_unresolved(parent);
            true
        }
    }
}

fn warn_unresolved(channel_id: u64) {
    tracing::warn!(
        channel_id,
        "チャンネルの親を調べられないため、除外したものとして扱います"
    );
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const CATEGORY: u64 = 1;
    const TEXT: u64 = 2;
    const THREAD: u64 = 3;
    const OTHER_TEXT: u64 = 4;

    fn channels() -> HashMap<u64, ChannelInfo> {
        let channel = |parent, is_thread| ChannelInfo { parent, is_thread };
        HashMap::from([
            (CATEGORY, channel(None, false)),
            (TEXT, channel(Some(CATEGORY), false)),
            (THREAD, channel(Some(TEXT), true)),
            (OTHER_TEXT, channel(None, false)),
        ])
    }

    async fn excluded(ex: &ChannelExclusions, id: u64) -> bool {
        let known = channels();
        is_excluded(ex, id, |c| {
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
        let broken = |c: u64| async move {
            (c == THREAD).then_some(ChannelInfo {
                parent: Some(TEXT),
                is_thread: true,
            })
        };
        assert!(is_excluded(&ex, THREAD, broken).await);
    }

    #[tokio::test]
    async fn no_lookup_happens_while_nothing_is_excluded() {
        let ex = with(&[]);
        assert!(!is_excluded(&ex, 999, |_| async { panic!("lookup") }).await);
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
}
