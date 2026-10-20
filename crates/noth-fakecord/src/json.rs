//! state の型を Discord の JSON 形に直す関数。REST と Gateway の両方が使う
//!
//! serenity 0.12.5 の型が必須とするフィールド(serde(default) も Option でもないもの)は一通り含める
use serde_json::{Value, json};

use crate::state::{
    CHANNEL_PUBLIC_THREAD, Channel, Guild, Id, Interaction, Member, Message, Reaction, Role, User,
    World,
};

impl User {
    pub fn to_discord_json(&self) -> Value {
        json!({
            "id": self.id,
            "username": self.username,
            "discriminator": "0",
            "global_name": self.global_name,
            "avatar": null,
            "bot": self.bot,
        })
    }
}

impl Role {
    pub fn to_discord_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "color": self.color,
            "colors": {
                "primary_color": self.color,
                "secondary_color": Value::Null,
                "tertiary_color": Value::Null,
            },
            "hoist": false,
            "position": self.position,
            "permissions": self.permissions,
            "managed": false,
            "mentionable": false,
            "flags": 0,
            "icon": Value::Null,
            "unicode_emoji": Value::Null,
        })
    }
}

impl Channel {
    pub fn to_discord_json(&self) -> Value {
        let mut value = json!({
            "id": self.id,
            "type": self.kind,
            "guild_id": self.guild_id,
            "name": self.name,
            "parent_id": self.parent_id,
            "position": self.position,
            "permission_overwrites": [],
            "nsfw": false,
            "topic": self.topic,
        });
        if self.kind == CHANNEL_PUBLIC_THREAD {
            value["owner_id"] = json!(self.owner_id);
            value["message_count"] = json!(0);
            value["member_count"] = json!(1);
            value["rate_limit_per_user"] = json!(0);
            value["thread_metadata"] = json!({
                "archived": self.archived,
                "auto_archive_duration": 1440,
                "archive_timestamp": crate::state::now_rfc3339(),
                "locked": false,
            });
        }
        value
    }
}

impl Guild {
    /// `GET /guilds/{id}` 相当。チャンネルや役は含めない
    pub fn to_discord_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "icon": null,
            "owner_id": self.owner_id,
            "afk_timeout": 300,
            "verification_level": 0,
            "default_message_notifications": 0,
            "explicit_content_filter": 0,
            "mfa_level": 0,
            "features": [],
            "premium_tier": 0,
            "preferred_locale": "en-US",
            "nsfw_level": 0,
            "system_channel_flags": 0,
            // serenity の Guild は premium_progress_bar_enabled(bool)を必須とする。
            // Option の 2 つは欠けても読めるが、実物の Discord に合わせて明示する
            "premium_progress_bar_enabled": false,
            "safety_alerts_channel_id": Value::Null,
            "incidents_data": Value::Null,
        })
    }
}

impl World {
    pub fn member_to_discord_json(&self, member: &Member) -> Value {
        json!({
            "user": self.users.get(&member.user_id).map(User::to_discord_json),
            "nick": member.nick,
            "roles": member.roles,
            "joined_at": member.joined_at,
            "deaf": false,
            "mute": false,
            "flags": 0,
            "pending": false,
        })
    }

    pub fn reaction_to_discord_json(&self, reaction: &Reaction) -> Value {
        let (name, id) = match reaction.emoji.split_once(':') {
            Some((name, id)) => (name, Some(id)),
            None => (reaction.emoji.as_str(), None),
        };
        json!({
            "count": reaction.user_ids.len(),
            "count_details": { "burst": 0, "normal": reaction.user_ids.len() },
            "me": reaction.user_ids.contains(&self.bot_user_id),
            "me_burst": false,
            "emoji": { "id": id, "name": name },
            "burst_colors": [],
        })
    }

    /// guild_id と member は MESSAGE_CREATE(Gateway)では付き、REST の応答では付かない
    pub fn message_to_discord_json(&self, message: &Message, gateway: bool) -> Value {
        self.message_json(message, gateway, true)
    }

    /// `embed_reference` が false のときは referenced_message を付けない(入れ子は 1 段で止める)
    fn message_json(&self, message: &Message, gateway: bool, embed_reference: bool) -> Value {
        let author = self.users.get(&message.author_id);
        let mentions: Vec<Value> = message
            .mentions
            .iter()
            .filter_map(|id| self.users.get(id))
            .map(User::to_discord_json)
            .collect();
        let reactions: Vec<Value> = message
            .reactions
            .iter()
            .map(|r| self.reaction_to_discord_json(r))
            .collect();
        let mut value = json!({
            "id": message.id,
            "channel_id": message.channel_id,
            "author": author.map(User::to_discord_json),
            "content": message.content,
            "timestamp": message.timestamp,
            "edited_timestamp": message.edited_timestamp,
            "tts": false,
            "mention_everyone": false,
            "mentions": mentions,
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": message.pinned,
            // 返信(message_reference 付き)は本物と同じく type 19 にする
            "type": if message.message_reference.is_some() && message.kind == 0 { 19 } else { message.kind },
            "flags": message.flags,
            "components": message.components,
        });
        if !reactions.is_empty() {
            value["reactions"] = json!(reactions);
        }
        if let Some(reference) = &message.message_reference {
            value["message_reference"] = json!(reference);
            if embed_reference {
                // 参照先を 1 段だけ埋め込む。noth は返信先が自分の発言かをこれで判定する。
                // 参照先が無い(削除済みなど)ときは null
                let referenced = reference
                    .message_id
                    .and_then(|id| self.messages.get(&id))
                    .map(|m| self.message_json(m, false, false));
                value["referenced_message"] = referenced.unwrap_or(Value::Null);
            }
        }
        if gateway && let Some(channel) = self.channels.get(&message.channel_id) {
            value["guild_id"] = json!(channel.guild_id);
            if let Some(member) = self.member(channel.guild_id, message.author_id) {
                // Gateway の member は user を含まない
                let mut member_json = self.member_to_discord_json(member);
                if let Some(obj) = member_json.as_object_mut() {
                    obj.remove("user");
                }
                value["member"] = member_json;
            }
        }
        value
    }

    /// INTERACTION_CREATE の `d`
    pub fn interaction_to_discord_json(&self, interaction: &Interaction) -> Value {
        let user = self.users.get(&interaction.user_id);
        let mut value = json!({
            "id": interaction.id,
            "application_id": interaction.application_id,
            "type": interaction.kind,
            "data": interaction.data,
            "guild_id": interaction.guild_id,
            "channel_id": interaction.channel_id,
            "channel": self.channels.get(&interaction.channel_id).map(Channel::to_discord_json),
            "member": self
                .member(interaction.guild_id, interaction.user_id)
                .map(|m| self.member_to_discord_json(m)),
            "user": user.map(User::to_discord_json),
            "token": interaction.token,
            "version": 1,
            "app_permissions": "1071698660929",
            "locale": "ja",
            "guild_locale": "ja",
            // serenity の Command / Component Interaction が必須とするフィールド
            "entitlements": [],
            "attachment_size_limit": 10_485_760,
            "context": 0,
            "authorizing_integration_owners": { "0": interaction.guild_id },
        });
        if let Some(message) = interaction.message_id.and_then(|id| self.messages.get(&id)) {
            value["message"] = self.message_to_discord_json(message, false);
        }
        value
    }

    /// GUILD_CREATE の `d`。cache_ready に必要なので、チャンネル・スレッド・役・メンバーを含める
    pub fn guild_create_json(&self, guild_id: Id) -> Option<Value> {
        let guild = self.guilds.get(&guild_id)?;
        let channels: Vec<Value> = self
            .channels
            .values()
            .filter(|c| c.guild_id == guild_id && c.kind != CHANNEL_PUBLIC_THREAD)
            .map(Channel::to_discord_json)
            .collect();
        let threads: Vec<Value> = self
            .channels
            .values()
            .filter(|c| c.guild_id == guild_id && c.kind == CHANNEL_PUBLIC_THREAD && !c.archived)
            .map(Channel::to_discord_json)
            .collect();
        let roles: Vec<Value> = self
            .roles
            .values()
            .filter(|r| r.guild_id == guild_id)
            .map(Role::to_discord_json)
            .collect();
        let members: Vec<Value> = self
            .members
            .iter()
            .filter(|m| m.guild_id == guild_id)
            .map(|m| self.member_to_discord_json(m))
            .collect();
        let mut value = guild.to_discord_json();
        value["joined_at"] = json!(crate::state::now_rfc3339());
        value["large"] = json!(false);
        value["unavailable"] = json!(false);
        value["member_count"] = json!(members.len());
        value["channels"] = json!(channels);
        value["threads"] = json!(threads);
        value["roles"] = json!(roles);
        value["members"] = json!(members);
        value["emojis"] = json!([]);
        value["stickers"] = json!([]);
        value["presences"] = json!([]);
        value["voice_states"] = json!([]);
        // serenity の Guild は stage_instances と guild_scheduled_events(Vec)を必須とする
        value["stage_instances"] = json!([]);
        value["guild_scheduled_events"] = json!([]);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MessageReference;

    fn add_message(w: &mut World, content: &str, author: Id, reply_to: Option<Id>) -> Id {
        let id = w.alloc_id();
        let channel_id = *w.channels.keys().next().unwrap();
        w.messages.insert(
            id,
            Message {
                id,
                channel_id,
                author_id: author,
                content: content.into(),
                timestamp: crate::state::now_rfc3339(),
                edited_timestamp: None,
                kind: if reply_to.is_some() { 19 } else { 0 },
                message_reference: reply_to.map(|m| MessageReference {
                    message_id: Some(m),
                    channel_id: Some(channel_id),
                    guild_id: None,
                }),
                mentions: Vec::new(),
                components: Vec::new(),
                flags: 0,
                reactions: Vec::new(),
                pinned: false,
                interaction_id: None,
            },
        );
        id
    }

    /// 返信には参照先が 1 段だけ埋め込まれ、type は 19。serenity の Message で読める
    #[test]
    fn reply_embeds_referenced_message_one_level() {
        let mut w = World::seed();
        let bot = w.bot_user_id;
        let kurazu = w.find_user("kurazu").unwrap().id;
        let first = add_message(&mut w, "first", bot, None);
        let second = add_message(&mut w, "second", kurazu, Some(first));
        let third = add_message(&mut w, "third", bot, Some(second));

        for gateway in [true, false] {
            let v = w.message_to_discord_json(&w.messages[&third], gateway);
            assert_eq!(v["type"], 19);
            assert_eq!(v["referenced_message"]["content"], "second");
            assert_eq!(v["referenced_message"]["type"], 19);
            // 入れ子は 1 段で止める
            assert!(v["referenced_message"].get("referenced_message").is_none());
            let msg: serenity::model::channel::Message = serde_json::from_value(v).unwrap();
            let referenced = msg.referenced_message.unwrap();
            assert_eq!(referenced.content, "second");
            assert!(referenced.referenced_message.is_none());
        }

        // 返信でないメッセージには付けない
        let v = w.message_to_discord_json(&w.messages[&first], true);
        assert!(v.get("referenced_message").is_none());
        assert_eq!(v["type"], 0);
    }

    /// 参照先が無い返信は referenced_message が null
    #[test]
    fn reply_to_missing_message_has_null_referenced_message() {
        let mut w = World::seed();
        let kurazu = w.find_user("kurazu").unwrap().id;
        let reply = add_message(&mut w, "orphan", kurazu, Some(Id(12345)));
        let v = w.message_to_discord_json(&w.messages[&reply], true);
        assert!(v["referenced_message"].is_null());
        assert_eq!(v["type"], 19);
        let msg: serenity::model::channel::Message = serde_json::from_value(v).unwrap();
        assert!(msg.referenced_message.is_none());
    }
}
