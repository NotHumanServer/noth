//! 読み取り専用の Discord 参照 tool。範囲と秘匿の方針は security.md §2
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use noth_core::history::{HistoryQuery, MessageHistory};
use noth_core::registry::{ModuleCtx, Registry};
use noth_core::store::MessageRow;
use noth_core::traits::Tool;
use noth_core::types::{Audience, ToolContext, ToolOutput, ToolSpec};
use noth_core::{Error, Result};
use serde_json::{Value, json};
use tokio::sync::OnceCell;

mod access;
mod api;
mod args;
mod budget;
mod config;
mod format;
mod manifest;
mod search;

use access::NickLookup;
use access::{READ_MESSAGES, VIEW_CHANNEL};
use api::{ApiError, DiscordApi, RestApi, describe_error};
use args::{clamp_limit, id_arg, require_id};
use budget::{CONTENT_MAX_CHARS, cap_content, keep_back, keep_front, listing};
use config::Settings;
use format::{Viewer, format_time, id_of, message_summary, user_name};

/// approval.rs の `DISCORD_TOOL` と同じ名前にする
const TOOL_NAME: &str = noth_core::approval::DISCORD_TOOL;
const FETCH_DEFAULT_LIMIT: usize = 50;
/// ピン一覧は概観用なので、長文は短く切る
const PIN_CONTENT_MAX_CHARS: usize = 300;
/// 新しいピン API の 1 ページの上限
const PINS_PAGE_LIMIT: &str = "50";
const TOPIC_MAX_CHARS: usize = 100;
const SEARCH_PAGE_SIZE: usize = 200;
const SEARCH_MAX_PAGES: usize = 5;

pub fn register(reg: &mut Registry) {
    reg.register_tool("tool-discord", factory);
}

fn factory(ctx: &ModuleCtx) -> Result<Arc<dyn Tool>> {
    if !ctx.config.is_enabled("channel-discord") {
        return Err(Error::Config(
            "tool-discord は channel-discord も有効にしないと使えません(Bot トークンと guild_id を channel-discord と共有するため)。[modules] enabled に channel-discord を追加してください".into(),
        ));
    }
    let settings = Settings::parse(&ctx.config.module_section("channel-discord"), &ctx.section)?;
    let api = RestApi::new(settings.token.clone())?;
    Ok(Arc::new(DiscordTool::new(
        settings,
        Arc::new(api),
        TimeZone::system(),
    )))
}

#[derive(Clone, Copy)]
enum Verdict {
    Readable,
    Denied,
    Unverifiable,
}

#[derive(Default)]
struct SearchOutcome {
    rows: Vec<MessageRow>,
    /// チャンネルの権限を確認できず捨てた行数。権限なしとは区別して知らせる
    skipped_unverifiable: usize,
    /// ページ数の上限で打ち切った
    truncated: bool,
}

struct Me {
    id: u64,
    name: String,
}

/// 取得時刻、ロール、サーバーでのニックネーム
type MemberEntry = (Instant, Vec<u64>, Option<String>);

struct DiscordTool {
    api: Arc<dyn DiscordApi>,
    guild_id: u64,
    allowed: HashSet<u64>,
    actions: Vec<&'static str>,
    tz: TimeZone,
    me: OnceCell<Me>,
    /// 権限の計算に使う情報の短期キャッシュ。ロールやオーバーライトの変更が数十秒遅れて反映される
    channels: Mutex<HashMap<u64, (Instant, Value)>>,
    guild: Mutex<Option<(Instant, Arc<access::GuildPerms>)>>,
    members: Mutex<HashMap<u64, MemberEntry>>,
    nicks: Mutex<HashMap<u64, (Instant, Option<String>)>>,
}

impl DiscordTool {
    fn new(settings: Settings, api: Arc<dyn DiscordApi>, tz: TimeZone) -> Self {
        Self {
            api,
            guild_id: settings.guild_id,
            allowed: settings.allowed,
            actions: settings.actions,
            tz,
            me: OnceCell::new(),
            channels: Mutex::default(),
            guild: Mutex::new(None),
            members: Mutex::default(),
            nicks: Mutex::default(),
        }
    }

    async fn get(
        &self,
        action: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> std::result::Result<Value, String> {
        self.api
            .get(path, query)
            .await
            .map_err(|e| describe_error(action, &e))
    }

    async fn me(&self, action: &str) -> std::result::Result<&Me, String> {
        self.me
            .get_or_try_init(|| async {
                let v = self.api.get("/users/@me", &[]).await?;
                Ok(Me {
                    id: v
                        .get("id")
                        .and_then(id_of)
                        .ok_or_else(|| ApiError::new(0, "Bot 自身の ID を取得できません"))?,
                    name: user_name(&v),
                })
            })
            .await
            .map_err(|e: ApiError| describe_error(action, &e))
    }

    async fn dispatch(
        &self,
        input: &Value,
        ctx: &ToolContext,
    ) -> std::result::Result<Value, String> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or("action は必須です")?;
        if !self.actions.contains(&action) {
            return Err(format!(
                "action `{action}` は使えません(使えるもの: {})",
                self.actions.join(", ")
            ));
        }
        match action {
            "search_messages" => self.search_messages(input, ctx).await,
            "fetch_messages" => self.fetch_messages(input, ctx).await,
            "list_pins" => self.list_pins(input, ctx).await,
            "list_channels" => self.list_channels(ctx).await,
            "channel_info" => self.channel_info(input, ctx).await,
            "server_info" => self.server_info().await,
            "list_roles" => self.list_roles().await,
            "member_info" => self.member_info(input).await,
            _ => unreachable!("manifest にある action だけが actions に入る"),
        }
    }

    /// 発言者とメンションの相手のうち `member` が付いていない allowed_users のニックネームを引く
    async fn with_nicks<'a>(&self, mut viewer: Viewer<'a>, msgs: &[Value]) -> Viewer<'a> {
        let mut wanted: HashSet<u64> = HashSet::new();
        for m in msgs {
            let users = std::iter::once(m.get("author")).chain(
                m.get("mentions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(Some),
            );
            for u in users.flatten() {
                let has_nick = u.get("member").is_some_and(|mem| {
                    mem.get("nick")
                        .and_then(Value::as_str)
                        .is_some_and(|n| !n.is_empty())
                });
                if let Some(id) = u.get("id").and_then(id_of)
                    && !has_nick
                    && self.allowed.contains(&id)
                {
                    wanted.insert(id);
                }
            }
        }
        for id in wanted {
            match self.member_nick(id).await {
                NickLookup::Found(nick) => {
                    viewer.nicks.insert(id, nick);
                }
                NickLookup::Missing => {}
                // 残りを叩き続けると制限が長引くので、取れた分とグローバル名で返す
                NickLookup::RateLimited => break,
            }
        }
        viewer
    }

    async fn viewer<'a>(&'a self, action: &str) -> std::result::Result<Viewer<'a>, String> {
        let me = self.me(action).await?;
        Ok(Viewer {
            self_id: me.id,
            self_name: &me.name,
            allowed: &self.allowed,
            tz: &self.tz,
            nicks: HashMap::new(),
        })
    }

    async fn search_messages(
        &self,
        input: &Value,
        ctx: &ToolContext,
    ) -> std::result::Result<Value, String> {
        let history = ctx
            .history
            .as_ref()
            .ok_or("発言の検索窓口がありません(Worker から呼ばれていません)")?;
        let q = search::build_query(input, self.guild_id, &self.tz, ctx.audience.channel_id)?;
        let found = self
            .search_readable(history.as_ref(), q, &ctx.audience)
            .await?;
        let rows = found.rows;
        let messages: Vec<Value> = rows
            .iter()
            .map(|m| {
                let (content, truncated) = cap_content(&m.content, CONTENT_MAX_CHARS);
                let mut v = json!({
                    "id": m.message_id.to_string(),
                    "channel": m.channel_name,
                    "channel_id": m.channel_id.to_string(),
                    "author": m.author_name,
                    "at": format_time(m.created_at, &self.tz),
                    "content": content,
                    "is_self": m.is_self,
                    "is_noise": m.is_noise,
                    "reply_to": m.reply_to.as_ref().map(|r| json!({
                        "id": r.message_id.to_string(),
                        "author": r.author_name,
                        "excerpt": r.excerpt,
                    })),
                });
                if truncated {
                    v["truncated"] = json!(true);
                }
                v
            })
            .collect();
        let (messages, omitted) = keep_front(messages);
        let mut out = listing("messages", messages, omitted);
        if found.skipped_unverifiable > 0 {
            out["skipped_unverifiable"] = json!(found.skipped_unverifiable);
        }
        if found.truncated {
            out["truncated"] = json!(true);
        }
        Ok(out)
    }

    /// 依頼した人全員が読めるチャンネルの発言だけを集める。読めない行を捨てると件数が足りなくなるので、
    /// 足りるまでか上限のページ数まで古い方へたどる
    async fn search_readable(
        &self,
        history: &dyn MessageHistory,
        mut q: HistoryQuery,
        audience: &Audience,
    ) -> std::result::Result<SearchOutcome, String> {
        const ACTION: &str = "search_messages";
        if let Some(id) = q.channel_id {
            self.readable_channel(ACTION, id, audience).await?;
        }
        let want = q.limit;
        q.limit = SEARCH_PAGE_SIZE;
        let mut found = SearchOutcome::default();
        let mut seen = HashSet::new();
        let mut verdicts: HashMap<u64, Verdict> = HashMap::new();
        for page in 0..SEARCH_MAX_PAGES {
            let rows = history
                .search(&q)
                .await
                .map_err(|e| format!("検索に失敗しました: {e}"))?;
            let page_full = rows.len() >= SEARCH_PAGE_SIZE;
            let oldest = rows.last().map(|r| r.created_at);
            for row in rows {
                if !seen.insert(row.id) {
                    continue;
                }
                let verdict = match verdicts.get(&row.channel_id) {
                    Some(v) => *v,
                    None => {
                        let v = self.channel_verdict(ACTION, row.channel_id, audience).await;
                        verdicts.insert(row.channel_id, v);
                        v
                    }
                };
                match verdict {
                    Verdict::Readable => {
                        found.rows.push(row);
                        if found.rows.len() == want {
                            return Ok(found);
                        }
                    }
                    Verdict::Unverifiable => found.skipped_unverifiable += 1,
                    Verdict::Denied => {}
                }
            }
            let Some(oldest) = oldest.filter(|_| page_full) else {
                return Ok(found);
            };
            if page + 1 == SEARCH_MAX_PAGES {
                // まだ古い側に行があるのに打ち切った
                found.truncated = true;
                break;
            }
            // 同じ時刻の発言を取りこぼさないよう 1ms 重ねる。重複は seen で除く
            q.until = Timestamp::from_millisecond(oldest.as_millisecond() + 1).ok();
        }
        Ok(found)
    }

    /// 削除済み・別サーバー・権限なしは Denied、429/5xx/通信の失敗は確認できなかったものとして Unverifiable
    async fn channel_verdict(&self, action: &str, channel_id: u64, audience: &Audience) -> Verdict {
        if audience.channel_id == Some(channel_id) {
            return Verdict::Readable;
        }
        let ch = match self.channel_fetch(channel_id).await {
            Ok(ch) => ch,
            Err(Some(e)) if !matches!(e.status, 403 | 404) => return Verdict::Unverifiable,
            Err(_) => return Verdict::Denied,
        };
        match self.can_read(action, &ch, audience).await {
            Ok(true) => Verdict::Readable,
            Ok(false) => Verdict::Denied,
            Err(_) => Verdict::Unverifiable,
        }
    }

    async fn fetch_messages(
        &self,
        input: &Value,
        ctx: &ToolContext,
    ) -> std::result::Result<Value, String> {
        const ACTION: &str = "fetch_messages";
        // トリガーのチャンネルは依頼した人が発言した場所なので、「このチャンネル」には ID を省略させる
        let channel_id = id_arg(input, "channel_id")?
            .or(ctx.audience.channel_id)
            .ok_or("channel_id は必須です")?;
        let single = id_arg(input, "message_id")?;
        let mut query = vec![("limit", clamp_limit(input, FETCH_DEFAULT_LIMIT).to_string())];
        let anchors: Vec<(&str, u64)> = ["before", "after", "around"]
            .into_iter()
            .filter_map(|k| id_arg(input, k).transpose().map(|r| r.map(|id| (k, id))))
            .collect::<std::result::Result<_, _>>()?;
        if anchors.len() > 1 {
            return Err("before / after / around は 1 つだけ指定できます".into());
        }
        if single.is_some() && !anchors.is_empty() {
            return Err("message_id は before / after / around と同時に指定できません".into());
        }
        query.extend(anchors.into_iter().map(|(k, id)| (k, id.to_string())));
        self.readable_channel(ACTION, channel_id, &ctx.audience)
            .await?;
        if let Some(message_id) = single {
            let raw = self
                .get(
                    ACTION,
                    &format!("/channels/{channel_id}/messages/{message_id}"),
                    &[],
                )
                .await?;
            let viewer = self.viewer(ACTION).await?;
            let viewer = self.with_nicks(viewer, std::slice::from_ref(&raw)).await;
            let messages = vec![message_summary(&raw, &viewer, CONTENT_MAX_CHARS)];
            return Ok(listing("messages", messages, 0));
        }
        let raw = self
            .get(ACTION, &format!("/channels/{channel_id}/messages"), &query)
            .await?;
        let viewer = self.viewer(ACTION).await?;
        let viewer = self
            .with_nicks(viewer, raw.as_array().map_or(&[][..], Vec::as_slice))
            .await;
        // Discord は新しい順で返す。読みやすいよう古い順に直す
        let messages: Vec<Value> = raw
            .as_array()
            .map(|a| {
                a.iter()
                    .rev()
                    .map(|m| message_summary(m, &viewer, CONTENT_MAX_CHARS))
                    .collect()
            })
            .unwrap_or_default();
        // 収まらないときは古い発言から落とす
        let (messages, omitted) = keep_back(messages);
        Ok(listing("messages", messages, omitted))
    }

    async fn list_pins(
        &self,
        input: &Value,
        ctx: &ToolContext,
    ) -> std::result::Result<Value, String> {
        const ACTION: &str = "list_pins";
        let channel_id = require_id(input, "channel_id")?;
        self.readable_channel(ACTION, channel_id, &ctx.audience)
            .await?;
        let raw = match self
            .api
            .get(
                &format!("/channels/{channel_id}/messages/pins"),
                &[("limit", PINS_PAGE_LIMIT.into())],
            )
            .await
        {
            Ok(v) => v,
            // 旧 API(非推奨)しか無い環境向け。チャンネルの存在は確認済みなので 404 は新 API が無いことを指す
            Err(e) if e.status == 404 => {
                self.get(ACTION, &format!("/channels/{channel_id}/pins"), &[])
                    .await?
            }
            Err(e) => return Err(describe_error(ACTION, &e)),
        };
        // 新しいピン API は `{items: [{message}]}`、旧 API は配列を返す
        let items: Vec<&Value> = match (&raw, raw.get("items").and_then(Value::as_array)) {
            (_, Some(items)) => items
                .iter()
                .map(|i| i.get("message").unwrap_or(i))
                .collect(),
            (Value::Array(a), None) => a.iter().collect(),
            _ => Vec::new(),
        };
        let viewer = self.viewer(ACTION).await?;
        let pinned: Vec<Value> = items.iter().map(|m| (*m).clone()).collect();
        let viewer = self.with_nicks(viewer, &pinned).await;
        let messages: Vec<Value> = items
            .into_iter()
            .map(|m| message_summary(m, &viewer, PIN_CONTENT_MAX_CHARS))
            .collect();
        let (messages, omitted) = keep_front(messages);
        let mut out = listing("messages", messages, omitted);
        // 50 件を超えるピンは 1 ページ目しか取っていない
        if raw.get("has_more").and_then(Value::as_bool) == Some(true) {
            out["has_more"] = json!(true);
        }
        Ok(out)
    }

    async fn list_channels(&self, ctx: &ToolContext) -> std::result::Result<Value, String> {
        let raw = self
            .get(
                "list_channels",
                &format!("/guilds/{}/channels", self.guild_id),
                &[],
            )
            .await?;
        let position = |c: &Value| c.get("position").and_then(Value::as_i64).unwrap_or(0);
        let kind = |c: &Value| c.get("type").and_then(Value::as_i64).unwrap_or(-1);
        // 依頼した人全員が見えるチャンネルだけを載せる。確認できなければ依頼元のチャンネルだけ
        let readers = self.readers("list_channels", &ctx.audience).await?;
        let trigger = ctx.audience.channel_id;
        // fetch_messages と同じ条件(VIEW_CHANNEL と READ_MESSAGE_HISTORY)にして、
        // 一覧に出たチャンネルは読める、という対応を保つ。カテゴリは見えるだけでよい
        let can_view = |c: &Value| {
            let need = if kind(c) == 4 {
                VIEW_CHANNEL
            } else {
                READ_MESSAGES
            };
            (trigger.is_some() && c.get("id").and_then(id_of) == trigger)
                || readers.as_ref().is_some_and(|r| r.can(c, need))
        };
        let all = raw.as_array().cloned().unwrap_or_default();
        let visible_parents: HashSet<u64> = all
            .iter()
            .filter(|c| kind(c) != 4 && can_view(c))
            .filter_map(|c| c.get("parent_id").and_then(id_of))
            .collect();
        let channels: Vec<Value> = all
            .into_iter()
            .filter(|c| {
                if kind(c) == 4 {
                    can_view(c)
                        || c.get("id")
                            .and_then(id_of)
                            .is_some_and(|i| visible_parents.contains(&i))
                } else {
                    can_view(c)
                }
            })
            .collect();
        let mut categories: Vec<&Value> = channels.iter().filter(|c| kind(c) == 4).collect();
        categories.sort_by_key(|c| position(c));
        let summary = |c: &Value| {
            json!({
                "id": c.get("id"),
                "name": c.get("name"),
                "type": channel_type_name(kind(c)),
                "position": position(c),
                "topic": c.get("topic").and_then(Value::as_str).map(|t| cap_content(t, TOPIC_MAX_CHARS).0),
            })
        };
        let in_category = |parent: Option<&Value>| {
            let mut members: Vec<&Value> = channels
                .iter()
                .filter(|c| kind(c) != 4 && c.get("parent_id").filter(|p| !p.is_null()) == parent)
                .collect();
            members.sort_by_key(|c| position(c));
            members.into_iter().map(summary).collect::<Vec<_>>()
        };
        let category_ids: Vec<Option<&Value>> = categories.iter().map(|c| c.get("id")).collect();
        // カテゴリに属さない(または親が見つからない)チャンネルを先頭にまとめる
        let loose: Vec<Value> = {
            let mut members: Vec<&Value> = channels
                .iter()
                .filter(|c| {
                    kind(c) != 4
                        && !c
                            .get("parent_id")
                            .filter(|p| !p.is_null())
                            .is_some_and(|p| category_ids.contains(&Some(p)))
                })
                .collect();
            members.sort_by_key(|c| position(c));
            members.into_iter().map(summary).collect()
        };
        let mut raw_groups = Vec::new();
        if !loose.is_empty() {
            raw_groups.push((Value::Null, loose));
        }
        for c in &categories {
            raw_groups.push((
                json!({"id": c.get("id"), "name": c.get("name")}),
                in_category(c.get("id")),
            ));
        }
        let (groups, count, omitted) = budget::fit_groups(raw_groups);
        let mut out = json!({"count": count, "channel_groups": groups});
        if omitted > 0 {
            out["omitted"] = json!(omitted);
        }
        Ok(out)
    }

    async fn channel_info(
        &self,
        input: &Value,
        ctx: &ToolContext,
    ) -> std::result::Result<Value, String> {
        let channel_id = require_id(input, "channel_id")?;
        let ch = self
            .readable_channel("channel_info", channel_id, &ctx.audience)
            .await?;
        Ok(json!({
            "id": ch.get("id"),
            "name": ch.get("name"),
            "type": channel_type_name(ch.get("type").and_then(Value::as_i64).unwrap_or(-1)),
            "topic": ch.get("topic"),
            "nsfw": ch.get("nsfw").and_then(Value::as_bool).unwrap_or(false),
            "position": ch.get("position"),
            "parent_id": ch.get("parent_id"),
            "last_message_id": ch.get("last_message_id"),
        }))
    }

    async fn server_info(&self) -> std::result::Result<Value, String> {
        let g = self
            .get(
                "server_info",
                &format!("/guilds/{}", self.guild_id),
                &[("with_counts", "true".into())],
            )
            .await?;
        // owner_id は allowed_users 以外の ID を含み得るので返さない
        Ok(json!({
            "id": g.get("id"),
            "name": g.get("name"),
            "description": g.get("description"),
            "member_count": g.get("approximate_member_count"),
            "online_count": g.get("approximate_presence_count"),
            "premium_tier": g.get("premium_tier"),
            "verification_level": g.get("verification_level"),
        }))
    }

    async fn list_roles(&self) -> std::result::Result<Value, String> {
        let raw = self
            .get(
                "list_roles",
                &format!("/guilds/{}/roles", self.guild_id),
                &[],
            )
            .await?;
        let mut roles = raw.as_array().cloned().unwrap_or_default();
        let position = |r: &Value| r.get("position").and_then(Value::as_i64).unwrap_or(0);
        roles.sort_by_key(|r| std::cmp::Reverse(position(r)));
        let roles: Vec<Value> = roles
            .iter()
            .map(|r| {
                let color = r.get("color").and_then(Value::as_u64).filter(|c| *c != 0);
                json!({
                    "id": r.get("id"),
                    "name": r.get("name"),
                    "color": color.map(|c| format!("#{c:06x}")),
                    "position": position(r),
                    "mentionable": r.get("mentionable").and_then(Value::as_bool).unwrap_or(false),
                    "managed": r.get("managed").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect();
        let (roles, omitted) = keep_front(roles);
        Ok(listing("roles", roles, omitted))
    }

    async fn member_info(&self, input: &Value) -> std::result::Result<Value, String> {
        const ACTION: &str = "member_info";
        let user_id = require_id(input, "user_id")?;
        if !self.allowed.contains(&user_id) && user_id != self.me(ACTION).await?.id {
            return Err("対象外のユーザーです(allowed_users のユーザーだけ参照できます)".into());
        }
        let m = self
            .get(
                ACTION,
                &format!("/guilds/{}/members/{user_id}", self.guild_id),
                &[],
            )
            .await?;
        let user = m.get("user").unwrap_or(&Value::Null);
        Ok(json!({
            "user_id": user.get("id"),
            "username": user.get("username"),
            "display_name": user.get("global_name"),
            "nickname": m.get("nick"),
            "bot": user.get("bot").and_then(Value::as_bool).unwrap_or(false),
            "roles": m.get("roles"),
            "joined_at": m.get("joined_at"),
        }))
    }
}

fn channel_type_name(kind: i64) -> &'static str {
    match kind {
        0 => "text",
        2 => "voice",
        4 => "category",
        5 => "announcement",
        10..=12 => "thread",
        13 => "stage",
        15 => "forum",
        16 => "media",
        _ => "other",
    }
}

#[async_trait]
impl Tool for DiscordTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL_NAME.into(),
            description: manifest::description(&self.actions),
            input_schema: manifest::input_schema(&self.actions),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        Ok(match self.dispatch(&input, ctx).await {
            Ok(v) => ToolOutput {
                text: v.to_string(),
                is_error: false,
            },
            Err(text) => ToolOutput {
                text,
                is_error: true,
            },
        })
    }

    async fn health_check(&self) -> Result<String> {
        let me = self.me("health_check").await.map_err(Error::Other)?;
        let guild = self
            .get("health_check", &format!("/guilds/{}", self.guild_id), &[])
            .await
            .map_err(Error::Other)?;
        Ok(format!(
            "{} として {} を参照できます",
            me.name,
            guild.get("name").and_then(Value::as_str).unwrap_or("?")
        ))
    }
}

#[cfg(test)]
mod tests;
