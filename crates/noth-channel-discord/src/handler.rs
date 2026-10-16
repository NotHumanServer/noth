use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use noth_core::async_trait;
use noth_core::bus::{BusEvent, BusHandle};
use noth_core::exclusion::ChannelExclusions;
use noth_core::traits::ConnectionState;
use noth_core::types::{BotMarker, IncomingMessage, ReplyTo, UserId};
use serenity::all as sn;
use serenity::prelude::{Context, EventHandler};

use crate::command::{Controller, IgnoreOp, Incoming, Kind};
use crate::exclusion::{self, Resolver};
use crate::state::{Names, SharedState, from_stage};
use crate::text::{
    AuthorKind, bot_display_name, classify_author, is_aside, message_chars, replace_mentions,
};

pub(crate) struct Handler {
    pub(crate) bus: BusHandle,
    pub(crate) guild_id: u64,
    pub(crate) allowed: Vec<UserId>,
    pub(crate) names: Names,
    pub(crate) state: SharedState,
    pub(crate) self_id: UserId,
    pub(crate) self_name: String,
    pub(crate) exclusions: ChannelExclusions,
    pub(crate) resolver: Resolver,
    /// 展開を作り直す常駐タスクを 1 回だけ起こすため。`cache_ready` は再接続でも届く
    pub(crate) watching: AtomicBool,
    /// スラッシュコマンドを受け付けないときは None
    pub(crate) commands: Option<Arc<Controller>>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, _ready: sn::Ready) {
        self.state.set(ConnectionState::Connected);
    }

    async fn resume(&self, _ctx: Context, _event: sn::ResumedEvent) {
        self.state.set(ConnectionState::Connected);
    }

    async fn cache_ready(&self, ctx: Context, _guilds: Vec<sn::GuildId>) {
        if self.watching.swap(true, Ordering::SeqCst) {
            self.rebuild_exclusions(&ctx).await;
            return;
        }
        let exclusions = self.exclusions.clone();
        let cache = ctx.cache.clone();
        let guild_id = self.guild_id;
        tokio::spawn(async move {
            let mut changes = exclusions.changes();
            loop {
                exclusion::rebuild(&exclusions, &cache, guild_id).await;
                if changes.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    async fn channel_create(&self, ctx: Context, _channel: sn::GuildChannel) {
        self.rebuild_exclusions(&ctx).await;
    }

    async fn channel_update(
        &self,
        ctx: Context,
        _old: Option<sn::GuildChannel>,
        _new: sn::GuildChannel,
    ) {
        self.rebuild_exclusions(&ctx).await;
    }

    async fn channel_delete(
        &self,
        ctx: Context,
        _channel: sn::GuildChannel,
        _messages: Option<Vec<sn::Message>>,
    ) {
        self.rebuild_exclusions(&ctx).await;
    }

    async fn category_delete(&self, ctx: Context, _category: sn::GuildChannel) {
        self.rebuild_exclusions(&ctx).await;
    }

    async fn thread_create(&self, ctx: Context, _thread: sn::GuildChannel) {
        self.rebuild_exclusions(&ctx).await;
    }

    async fn thread_update(
        &self,
        ctx: Context,
        _old: Option<sn::GuildChannel>,
        _new: sn::GuildChannel,
    ) {
        self.rebuild_exclusions(&ctx).await;
    }

    async fn thread_delete(
        &self,
        ctx: Context,
        _thread: sn::PartialGuildChannel,
        _full: Option<sn::GuildChannel>,
    ) {
        self.rebuild_exclusions(&ctx).await;
    }

    async fn thread_list_sync(&self, ctx: Context, _sync: sn::ThreadListSyncEvent) {
        self.rebuild_exclusions(&ctx).await;
    }

    async fn shard_stage_update(&self, _ctx: Context, event: sn::ShardStageUpdateEvent) {
        self.state.set(from_stage(event.new));
    }

    async fn interaction_create(&self, _ctx: Context, interaction: sn::Interaction) {
        let Some(controller) = &self.commands else {
            return;
        };
        if let Some(incoming) = incoming(interaction) {
            controller.handle(incoming).await;
        }
    }

    async fn message(&self, ctx: Context, msg: sn::Message) {
        if msg.guild_id.map(|g| g.get()) != Some(self.guild_id) {
            return;
        }
        if !matches!(
            msg.kind,
            sn::MessageType::Regular | sn::MessageType::InlineReply
        ) {
            return;
        }
        // Bot の目印も、存在が除外したチャンネルの手がかりになるので、作者の判定より先に捨てる
        if self
            .resolver
            .is_excluded(msg.channel_id.get(), Instant::now(), |id| {
                exclusion::lookup(&ctx, self.guild_id, id)
            })
            .await
        {
            return;
        }
        let author_id = msg.author.id.get();
        let is_self = author_id == self.self_id;
        let origin = classify_author(
            is_self,
            msg.author.bot,
            msg.webhook_id.is_some(),
            self.allowed.contains(&author_id),
            is_aside(&msg.content),
        );
        if origin == AuthorKind::Drop {
            return;
        }

        let mut lookup: HashMap<UserId, String> = msg
            .mentions
            .iter()
            .map(|u| (u.id.get(), u.name.clone()))
            .collect();
        lookup.extend(self.names.read().unwrap_or_else(|e| e.into_inner()).clone());

        let is_bot = origin == AuthorKind::Bot;
        let content = if is_bot {
            String::new()
        } else {
            replace_mentions(
                &msg.content,
                self.self_id,
                &self.self_name,
                &self.allowed,
                &lookup,
            )
        };
        if !is_bot && content.trim().is_empty() {
            return;
        }
        let author_name = msg
            .member
            .as_ref()
            .and_then(|m| m.nick.clone())
            .unwrap_or_else(|| msg.author.display_name().to_string());
        let channel_name = ctx
            .cache
            .guild(self.guild_id)
            .and_then(|g| {
                // スレッドは channels ではなく threads に入っている
                g.channels
                    .get(&msg.channel_id)
                    .or_else(|| g.threads.iter().find(|t| t.id == msg.channel_id))
                    .map(|c| c.name.clone())
            })
            .unwrap_or_else(|| msg.channel_id.get().to_string());

        let bot_marker = is_bot.then(|| BotMarker {
            author_name: bot_display_name(&author_name),
            reply_to_author: msg.referenced_message.as_ref().map(|r| {
                let id = r.author.id.get();
                if id == self.self_id {
                    self.self_name.clone()
                } else if self.allowed.contains(&id) {
                    r.member
                        .as_ref()
                        .and_then(|m| m.nick.clone())
                        .or_else(|| lookup.get(&id).cloned())
                        .unwrap_or_else(|| r.author.display_name().to_string())
                } else {
                    "<user>".into()
                }
            }),
            chars: message_chars(&msg.content, &msg.embeds),
        });
        let reply_to = msg
            .referenced_message
            .as_ref()
            .filter(|_| !is_bot)
            .map(|r| {
                let is_self = r.author.id.get() == self.self_id;
                // allowed_users 以外の発言は保存しない方針なので、返信先でも名前と本文を残さない
                if !is_self && !self.allowed.contains(&r.author.id.get()) {
                    return ReplyTo {
                        message_id: r.id.get(),
                        author_name: "<user>".into(),
                        is_self: false,
                        excerpt: String::new(),
                    };
                }
                let author_name = if is_self {
                    self.self_name.clone()
                } else {
                    r.member
                        .as_ref()
                        .and_then(|m| m.nick.clone())
                        .or_else(|| lookup.get(&r.author.id.get()).cloned())
                        .unwrap_or_else(|| r.author.display_name().to_string())
                };
                let referenced = replace_mentions(
                    &r.content,
                    self.self_id,
                    &self.self_name,
                    &self.allowed,
                    &lookup,
                );
                ReplyTo {
                    message_id: r.id.get(),
                    author_name,
                    is_self,
                    excerpt: ReplyTo::excerpt_of(&referenced),
                }
            });
        let incoming = IncomingMessage {
            guild_id: self.guild_id,
            channel_id: msg.channel_id.get(),
            channel_name,
            message_id: msg.id.get(),
            user_id: author_id,
            author_name,
            content,
            timestamp: jiff::Timestamp::from_nanosecond(msg.timestamp.unix_timestamp_nanos())
                .unwrap_or_else(|_| jiff::Timestamp::now()),
            is_self,
            mentions_self: msg.mentions.iter().any(|u| u.id.get() == self.self_id),
            reply_to_self: msg
                .referenced_message
                .as_ref()
                .is_some_and(|r| r.author.id.get() == self.self_id),
            reply_to,
            bot_marker,
        };
        if let Err(e) = self.bus.publish(BusEvent::MessageReceived(incoming)).await {
            tracing::error!(error = %e, "failed to publish message");
        }
    }
}

impl Handler {
    /// 除外が無いときの発言ごとの更新を避けるため、指定が空なら何もしない。
    /// 指定が空になる変更の後始末は、`cache_ready` で起こした常駐タスクが行う
    async fn rebuild_exclusions(&self, ctx: &Context) {
        if self.exclusions.is_empty() {
            return;
        }
        exclusion::rebuild(&self.exclusions, &ctx.cache, self.guild_id).await;
    }
}

fn incoming(interaction: sn::Interaction) -> Option<Incoming> {
    match interaction {
        sn::Interaction::Command(c) => Some(Incoming {
            id: c.id.get(),
            user_id: c.user.id.get(),
            guild_id: c.guild_id.map(|g| g.get()),
            kind: slash_kind(&c.data.name, &c.data.options),
            token: c.token,
        }),
        sn::Interaction::Component(c) => Some(Incoming {
            id: c.id.get(),
            user_id: c.user.id.get(),
            guild_id: c.guild_id.map(|g| g.get()),
            kind: Kind::Button {
                custom_id: c.data.custom_id.clone(),
            },
            token: c.token,
        }),
        _ => None,
    }
}

/// サブコマンド(とそのグループ)は先頭の option として届く
fn slash_kind(name: &str, options: &[sn::CommandDataOption]) -> Kind {
    if name == crate::command::ROOT
        && let Some(sn::CommandDataOption {
            name: group,
            value: sn::CommandDataOptionValue::SubCommandGroup(ops),
            ..
        }) = options.first()
        && group == crate::command::IGNORE_GROUP
        && let Some(op) = ops.first()
        && let sn::CommandDataOptionValue::SubCommand(args) = &op.value
        && let Some(parsed) = IgnoreOp::parse(&op.name)
    {
        let channel = args.iter().find_map(|a| match (&a.value, a.name.as_str()) {
            (sn::CommandDataOptionValue::Channel(id), crate::command::CHANNEL_OPTION) => {
                Some(id.get())
            }
            _ => None,
        });
        return Kind::Ignore {
            op: parsed,
            channel,
        };
    }
    Kind::Slash {
        name: name.to_string(),
        sub: options.first().map(|o| o.name.clone()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn options(value: Value) -> Vec<sn::CommandDataOption> {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn plain_subcommand_stays_a_slash() {
        let ops = options(json!([{"name": "context", "type": 1, "options": []}]));
        match slash_kind("noth", &ops) {
            Kind::Slash { name, sub } => {
                assert_eq!(name, "noth");
                assert_eq!(sub.as_deref(), Some("context"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn ignore_group_is_parsed_from_the_discord_payload() {
        let add = options(json!([{"name": "ignore", "type": 2, "options": [
            {"name": "add", "type": 1, "options": [
                {"name": "channel", "type": 7, "value": "123456789012345678"}
            ]}
        ]}]));
        assert!(matches!(
            slash_kind("noth", &add),
            Kind::Ignore {
                op: IgnoreOp::Add,
                channel: Some(123456789012345678)
            }
        ));
        let list = options(json!([{"name": "ignore", "type": 2, "options": [
            {"name": "list", "type": 1, "options": []}
        ]}]));
        assert!(matches!(
            slash_kind("noth", &list),
            Kind::Ignore {
                op: IgnoreOp::List,
                channel: None
            }
        ));
    }

    #[test]
    fn unknown_ignore_operation_falls_back_to_slash() {
        let ops = options(json!([{"name": "ignore", "type": 2, "options": [
            {"name": "purge", "type": 1, "options": []}
        ]}]));
        assert!(matches!(slash_kind("noth", &ops), Kind::Slash { .. }));
    }
}
