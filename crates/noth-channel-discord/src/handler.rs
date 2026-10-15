use std::collections::HashMap;

use noth_core::async_trait;
use noth_core::bus::{BusEvent, BusHandle};
use noth_core::traits::ConnectionState;
use noth_core::types::{IncomingMessage, UserId};
use serenity::all as sn;
use serenity::prelude::{Context, EventHandler};

use crate::state::{Names, SharedState, from_stage};
use crate::text::replace_mentions;

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
        if !is_self
            && (msg.author.bot || msg.webhook_id.is_some() || !self.allowed.contains(&author_id))
        {
            return;
        }

        let mut lookup: HashMap<UserId, String> = msg
            .mentions
            .iter()
            .map(|u| (u.id.get(), u.name.clone()))
            .collect();
        lookup.extend(self.names.read().unwrap_or_else(|e| e.into_inner()).clone());

        let content = replace_mentions(
            &msg.content,
            self.self_id,
            &self.self_name,
            &self.allowed,
            &lookup,
        );
        if content.trim().is_empty() {
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
        };
        if let Err(e) = self.bus.publish(BusEvent::MessageReceived(incoming)).await {
            tracing::error!(error = %e, "failed to publish message");
        }
    }
}
