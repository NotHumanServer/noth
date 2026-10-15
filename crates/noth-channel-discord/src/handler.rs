use std::collections::HashMap;

use noth_core::async_trait;
use noth_core::bus::{BusEvent, BusHandle};
use noth_core::traits::ConnectionState;
use noth_core::types::{BotMarker, IncomingMessage, ReplyTo, UserId};
use serenity::all as sn;
use serenity::prelude::{Context, EventHandler};

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
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, _ready: sn::Ready) {
        self.state.set(ConnectionState::Connected);
    }

    async fn resume(&self, _ctx: Context, _event: sn::ResumedEvent) {
        self.state.set(ConnectionState::Connected);
    }

    async fn shard_stage_update(&self, _ctx: Context, event: sn::ShardStageUpdateEvent) {
        self.state.set(from_stage(event.new));
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
