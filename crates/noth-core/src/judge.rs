//! router に渡す質問セットと state の構築、回答の二値化、行動の決定。context-layers.md §4
use std::collections::HashSet;

use jiff::Timestamp;
use serde_json::{Value, json};

use crate::config::{QuestionTexts, Reaction, RouterSection, Thresholds};
use crate::markers::{self, Folded};
use crate::store::MessageRow;
use crate::text::contains_any_name;
use crate::traits::{Answer, Decision, Question};
use crate::types::*;

pub const KEY_NOISE: &str = "is_noise_or_meme";
pub const KEY_ADDRESSEE: &str = "addressee";
pub const KEY_NEEDS_OUTSIDE_WORK: &str = "needs_outside_work";
pub const KEY_CORRECTS_NOTH: &str = "corrects_noth";
pub const KEY_WORTH_SUPPLEMENT: &str = "worth_supplement";
pub const KEY_REACTION: &str = "reaction";
pub const KEY_TOPIC_SHIFT: &str = "is_topic_shift";
pub const KEY_DELIVERY: &str = "delivery_style";

/// `reaction` の「付けない」。先頭に置く(同点は先頭が選ばれ、付けない側が安全なため)
pub const REACTION_NONE: &str = "none";

/// デバウンス後の発言群。ingest がチャンネル単位でまとめるので、複数チャンネルにまたがらない
#[derive(Debug, Clone)]
pub struct JudgeBatch {
    pub channel_id: ChannelId,
    pub channel_name: String,
    pub messages: Vec<PendingMessage>,
}

#[derive(Debug, Clone)]
pub struct PendingMessage {
    /// `messages` の行 ID
    pub row_id: i64,
    pub message_id: MessageId,
    pub author_name: String,
    pub content: String,
    pub at: Timestamp,
    /// 再起動で復元した分も messages に保存した値を使うので、名指しへの応答は失われない
    pub mentions_self: bool,
    pub reply_to_self: bool,
    pub reply_to: Option<ReplyTo>,
}

/// 最後の発言は誰に向けたものか。宣言順が `addressee` の選択肢の並びで、同点は先頭(`Other`)が採られる
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressee {
    Other,
    AboutNoth,
    OtherBot,
    NothRequest,
}

impl Addressee {
    pub const ALL: [Addressee; 4] = [
        Addressee::Other,
        Addressee::AboutNoth,
        Addressee::OtherBot,
        Addressee::NothRequest,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Addressee::Other => "other",
            Addressee::AboutNoth => "about_noth",
            Addressee::OtherBot => "other_bot",
            Addressee::NothRequest => "noth_request",
        }
    }
}

/// router の回答を二値化したもの。回答が欠けている・型が違う項目は安全側(`safe()`)の値になる
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeAnswers {
    pub is_noise: bool,
    pub addressee: Addressee,
    pub needs_outside_work: bool,
    pub corrects_noth: bool,
    pub worth_supplement: bool,
    /// 付ける絵文字。`none` が選ばれたとき、絵文字が決まらないとき、`[[router.reactions]]` が空のときは None
    pub reaction: Option<String>,
    pub is_topic_shift: bool,
}

impl JudgeAnswers {
    /// 名指しされた発言をノイズとして落とすと、応答した発言が Layer 3 に無く会話が成り立たないので、Noth への依頼はノイズにしない
    pub fn is_noise_for(&self, cued: bool) -> bool {
        self.is_noise && !cued && self.addressee != Addressee::NothRequest
    }

    /// router が使えないときの値。何も起こさず、バッファには通常どおり入る
    pub fn safe() -> Self {
        Self {
            is_noise: false,
            addressee: Addressee::Other,
            needs_outside_work: false,
            corrects_noth: false,
            worth_supplement: false,
            reaction: None,
            is_topic_shift: false,
        }
    }
}

/// 二値化した回答から Core が決める動き方(§4.3)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 何もしない
    Silent,
    /// 発言はせず、トリガーの最後の発言に絵文字リアクションだけ付ける
    React,
    /// main が返事をする(パターン B)
    ReplyOnly,
    /// Worker に回す。Ack を出して調べる(パターン C)
    SpawnTask,
    /// 黙って Worker を起動し、人どうしの発言の事実を確かめる(パターン C'。`Origin::Supplement`)
    SilentSupplement,
    /// 黙って Worker を起動し、Noth の誤りの指摘を調べ直す(パターン C'。`Origin::Correction`)
    SilentCorrection,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Silent => "silent",
            Action::React => "react",
            Action::ReplyOnly => "reply_only",
            Action::SpawnTask => "spawn_task",
            Action::SilentSupplement => "silent_supplement",
            Action::SilentCorrection => "silent_correction",
        }
    }

    /// Worker を起こす動きなら、その `Origin`
    pub fn origin(self) -> Option<Origin> {
        match self {
            Action::SpawnTask => Some(Origin::Request),
            Action::SilentSupplement => Some(Origin::Supplement),
            Action::SilentCorrection => Some(Origin::Correction),
            _ => None,
        }
    }
}

/// `decide_action` の入力。閾値は二値化(`binarize`)で適用済み
#[derive(Debug, Clone, Copy)]
pub struct ActionInput<'a> {
    pub answers: &'a JudgeAnswers,
    /// Noth へのメンション、または Noth の発言へのリプライ。続きの窓は含まない(router への合図で、決定的には扱わない)。
    /// router の回答や失敗に関係なく `noth_request` として扱う
    pub cued: bool,
    /// `JudgeAnswers::is_noise_for(cued)` の結果。ノイズには補足も訂正もリアクションも起こさない
    pub noise: bool,
    pub worker_enabled: bool,
    /// 補足の最短間隔と 1 日の上限に当たっていないか。当たっていれば補足せず、訂正の判定に進む
    pub supplement_allowed: bool,
}

/// §4.3 の表。Worker を起こす行は、`worker_enabled` が false なら `ReplyOnly`(noth_request)か、
/// それ以外の行(リアクションの判定を含む)に進む
pub fn decide_action(input: &ActionInput) -> Action {
    let a = input.answers;
    if input.noise {
        return Action::Silent;
    }
    let addressee = if input.cued {
        Addressee::NothRequest
    } else {
        a.addressee
    };
    let worker = input.worker_enabled;
    match addressee {
        Addressee::NothRequest if worker && (a.needs_outside_work || a.corrects_noth) => {
            Action::SpawnTask
        }
        Addressee::NothRequest => Action::ReplyOnly,
        Addressee::AboutNoth if worker && a.corrects_noth => Action::SilentCorrection,
        Addressee::AboutNoth if a.reaction.is_some() => Action::React,
        Addressee::Other if worker && a.worth_supplement && input.supplement_allowed => {
            Action::SilentSupplement
        }
        Addressee::Other if worker && a.corrects_noth => Action::SilentCorrection,
        // other_bot は corrects_noth が高くても調べ直さない
        _ => Action::Silent,
    }
}

fn reaction_key(index: usize) -> String {
    format!("r{index}")
}

/// `reactions` が空ならリアクションの質問は出さない
pub fn build_questions(texts: &QuestionTexts, reactions: &[Reaction]) -> Vec<(String, Question)> {
    let t = &texts.addressee;
    let criteria_of = |a: Addressee| {
        let text = match a {
            Addressee::Other => &t.other,
            Addressee::AboutNoth => &t.about_noth,
            Addressee::OtherBot => &t.other_bot,
            Addressee::NothRequest => &t.noth_request,
        };
        (a.as_str().to_string(), text.clone())
    };
    let noul = |key: &str, instructions: &str| {
        (
            key.to_string(),
            Question::Noul {
                instructions: instructions.to_string(),
            },
        )
    };
    let mut qs = vec![
        noul(KEY_NOISE, &texts.is_noise_or_meme),
        (
            KEY_ADDRESSEE.to_string(),
            Question::Choice {
                instructions: t.instructions.clone(),
                criteria: Addressee::ALL.into_iter().map(criteria_of).collect(),
            },
        ),
        noul(KEY_NEEDS_OUTSIDE_WORK, &texts.needs_outside_work),
        noul(KEY_CORRECTS_NOTH, &texts.corrects_noth),
        noul(KEY_WORTH_SUPPLEMENT, &texts.worth_supplement),
    ];
    if !reactions.is_empty() {
        let mut criteria = vec![(REACTION_NONE.to_string(), texts.reaction.none.clone())];
        criteria.extend(
            reactions
                .iter()
                .enumerate()
                .map(|(i, r)| (reaction_key(i), format!("{} {}", r.emoji, r.meaning))),
        );
        qs.push((
            KEY_REACTION.to_string(),
            Question::Choice {
                instructions: texts.reaction.instructions.clone(),
                criteria,
            },
        ));
    }
    qs.push(noul(KEY_TOPIC_SHIFT, &texts.is_topic_shift));
    qs
}

/// Worker 合流時の報告の仕方(§4.5)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStyle {
    NormalReport,
    BriefSupplement,
    ReplyAnchor,
    Abort,
}

impl DeliveryStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NormalReport => "normal_report",
            Self::BriefSupplement => "brief_supplement",
            Self::ReplyAnchor => "reply_anchor",
            Self::Abort => "abort",
        }
    }
}

const DELIVERY_STYLES: [&str; 4] = ["normal_report", "brief_supplement", "reply_anchor", "abort"];

pub fn join_questions(origin: Origin) -> Vec<(String, Question)> {
    let (instructions, criteria) = match origin {
        Origin::Request => (
            "Noth はメンバーに頼まれた調べものや作業を背景で進めていて、それが終わりました。\
                task は依頼の内容(goal)と結果です。result が concluded なら memo は結論メモ、failed なら memo は失敗の理由です。\
                since_fork は作業を始めてから今までに届いた全チャンネルの発言で、is_self が true のものは Noth 自身の発言です。\
                since_fork を読んで、この結果をどう伝えるかを選んでください。\
                依頼した人たちが自分で答えにたどり着いた、または別の人が答えたなど、人間側で自己解決しているなら brief_supplement です。\
                task.channel の会話がすでに別の話題に移っていて、普通に報告すると流れを遮るなら reply_anchor です。\
                依頼が取り消された、もう誰も気にしていない、結果に伝える価値がないなら abort です。\
                調べられなかった(failed)ことも、依頼した人が答えを待っているなら伝える価値があります。\
                どれにも当たらず、会話がまだその話題の上にあるなら normal_report です。",
            [
                ("normal_report", "会話の流れの中で結果を普通に報告する"),
                (
                    "brief_supplement",
                    "人間側で自己解決しているので、補足として短く伝える",
                ),
                (
                    "reply_anchor",
                    "話題が移っているので、元の依頼へのリプライとして報告する",
                ),
                ("abort", "報告しない"),
            ],
        ),
        // 誰にも頼まれていない調べ直しなので、人どうしの会話がすでに解決していれば黙る側に倒す
        Origin::Supplement => (
            "Noth は、人どうしの会話の中の発言を、誰にも頼まれずに黙って調べました。\
                task は調べた発言(goal)と結果で、memo は結論メモです。\
                since_fork は調べ始めてから今までに届いた全チャンネルの発言で、is_self が true のものは Noth 自身の発言です。\
                since_fork を読んで、補足や訂正を投稿する価値がまだあるかを選んでください。\
                人どうしですでに解決した、誰かが同じ指摘をした、話題が移って割り込むと流れを遮るなら abort です。\
                会話がまだその話題の上にあり、この結果が役に立つなら normal_report です。\
                迷うときは abort にしてください。",
            [
                ("normal_report", "会話の流れの中で補足を投稿する"),
                (
                    "brief_supplement",
                    "すでに一部は出ているので、足りない点だけを短く投稿する",
                ),
                (
                    "reply_anchor",
                    "話題は移ったが、元の発言へのリプライなら通じるので投稿する",
                ),
                ("abort", "投稿しない"),
            ],
        ),
        // 指摘した人は答えを待っている。後続の発言が無いことは黙る理由にならない
        Origin::Correction => (
            "Noth は、自分が前に述べたことへの指摘を受けて、黙って調べ直しました。\
                task は指摘の発言(goal)と結果で、memo は結論メモです。\
                since_fork は調べ始めてから今までに届いた全チャンネルの発言で、is_self が true のものは Noth 自身の発言です。\
                指摘した人は、Noth の答えを待っています。\
                abort にするのは、since_fork で別の人がすでに同じ訂正をしたとき、または指摘が取り下げられたときだけです。\
                後続の発言が無いことは、abort の理由になりません。\
                abort の理由(別の人がすでに同じ訂正をした、または指摘が取り下げられた)がなければ normal_report です。",
            [
                ("normal_report", "会話の流れの中で訂正を投稿する"),
                (
                    "brief_supplement",
                    "すでに一部は出ているので、足りない点だけを短く投稿する",
                ),
                (
                    "reply_anchor",
                    "話題は移ったが、元の発言へのリプライなら通じるので投稿する",
                ),
                ("abort", "投稿しない"),
            ],
        ),
    };
    vec![(
        KEY_DELIVERY.to_string(),
        Question::Choice {
            instructions: instructions.into(),
            criteria: criteria
                .into_iter()
                .map(|(n, d)| (n.to_string(), d.to_string()))
                .collect(),
        },
    )]
}

/// Join の判定材料。`concluded` が false なら memo は失敗理由
pub struct JoinInput<'a> {
    pub run_id: &'a str,
    pub goal: &'a str,
    pub channel: &'a str,
    pub concluded: bool,
    pub memo: &'a str,
    pub since_fork: &'a [MessageRow],
}

/// `state_max_chars` を超える間は since_fork の古い発言から捨て、それでも超えるなら memo を削る
pub fn build_join_state(input: &JoinInput, cfg: &RouterSection) -> RouterState {
    let since_fork = fold_entries(input.since_fork);
    let mut state = json!({
        "task": {
            "run_id": input.run_id,
            "goal": input.goal,
            "channel": input.channel,
            "result": if input.concluded { "concluded" } else { "failed" },
            "memo": input.memo,
        },
        "since_fork": since_fork,
    });
    while char_len(&state) > cfg.state_max_chars {
        let items = state["since_fork"]
            .as_array_mut()
            .expect("since_fork は配列");
        if items.is_empty() {
            break;
        }
        items.remove(0);
    }
    for field in ["memo", "goal"] {
        let excess = char_len(&state).saturating_sub(cfg.state_max_chars);
        if excess == 0 {
            break;
        }
        let text = state["task"][field].as_str().unwrap_or_default();
        let keep = text.chars().count().saturating_sub(excess);
        let shortened: String = text.chars().take(keep).collect();
        state["task"][field] = Value::String(shortened);
    }
    RouterState(state)
}

/// 回答が無い・confidence が足りないときは abort。報告しないのが安全側(§4)
pub fn binarize_delivery(decision: &Decision, min_confidence: f32) -> DeliveryStyle {
    match pick_choice(decision, KEY_DELIVERY, &DELIVERY_STYLES, min_confidence).as_deref() {
        Some("normal_report") => DeliveryStyle::NormalReport,
        Some("brief_supplement") => DeliveryStyle::BriefSupplement,
        Some("reply_anchor") => DeliveryStyle::ReplyAnchor,
        _ => DeliveryStyle::Abort,
    }
}

fn rfc3339(ts: Timestamp) -> String {
    ts.to_string()
}

/// リプライでない発言には key ごと付けない(state を無駄に膨らませない)
fn insert_reply_to(v: &mut Value, reply_to: Option<&ReplyTo>) {
    if let Some(r) = reply_to {
        v["reply_to"] = json!({
            "author": r.author_name,
            "is_self": r.is_self,
            "excerpt": r.excerpt,
        });
    }
}

/// state の `noth` と `members` に入れる、router に Noth と仲間を教える材料
#[derive(Debug, Clone, Copy, Default)]
pub struct Roster<'a> {
    /// MEMORY.md の `## Noth について` の本文
    pub noth: &'a str,
    /// Layer 1 のメンバー一覧
    pub members: &'a [Member],
    /// MEMORY.md の `## メンバー` の小見出しごとの本文(ユーザー ID と本文)。小見出しの外の項目は誰のものか決められないので含めない
    pub member_memory: &'a [(UserId, String)],
}

/// recent は trigger を除いて直近 `recent_messages` 件に絞り、`state_max_chars` を超える間は古い方から捨てる。
/// それでも超えるときは各メンバーの記憶(長いものから)、`noth` の順に削る
pub fn build_state(
    batch: &JudgeBatch,
    recent: &[MessageRow],
    roster: &Roster,
    cfg: &RouterSection,
    in_window: &dyn Fn(&PendingMessage) -> bool,
) -> RouterState {
    let trigger_ids: HashSet<i64> = batch.messages.iter().map(|m| m.row_id).collect();
    let trigger: Vec<Value> = batch
        .messages
        .iter()
        .map(|m| {
            let mut v = json!({
                "channel_id": batch.channel_id.to_string(),
                "channel": batch.channel_name,
                "author": m.author_name,
                "content": m.content,
                "at": rfc3339(m.at),
                "mentions_self": m.mentions_self,
                "reply_to_self": m.reply_to_self,
                "calls_noth_by_name": contains_any_name(&m.content, &cfg.call_names),
                "in_conversation_window": in_window(m),
            });
            insert_reply_to(&mut v, m.reply_to.as_ref());
            v
        })
        .collect();

    let others: Vec<MessageRow> = recent
        .iter()
        .filter(|r| !trigger_ids.contains(&r.id))
        .cloned()
        .collect();
    let mut recent = fold_entries(&others);
    if recent.len() > cfg.recent_messages {
        recent.drain(..recent.len() - cfg.recent_messages);
    }

    let mut state = json!({});
    if !roster.noth.is_empty() {
        state["noth"] = json!(roster.noth);
    }
    if !roster.members.is_empty() {
        let list: Vec<Value> = roster
            .members
            .iter()
            .map(|m| {
                let mut v = json!({ "name": m.display_name, "roles": m.roles });
                // 同じ id の小見出しが複数あっても、その人の記憶としてまとめる
                let memory: Vec<&str> = roster
                    .member_memory
                    .iter()
                    .filter(|(id, body)| *id == m.user_id && !body.is_empty())
                    .map(|(_, body)| body.as_str())
                    .collect();
                if !memory.is_empty() {
                    v["memory"] = json!(memory.join("\n"));
                }
                v
            })
            .collect();
        state["members"] = json!({ "list": list });
    }
    state["trigger"] = json!(trigger);
    state["recent"] = json!(recent);
    while char_len(&state) > cfg.state_max_chars {
        let recent = state["recent"].as_array_mut().expect("recent は配列");
        if recent.is_empty() {
            break;
        }
        recent.remove(0);
    }
    loop {
        let excess = char_len(&state).saturating_sub(cfg.state_max_chars);
        if excess == 0 || !shrink_longest_member_memory(&mut state, excess) {
            break;
        }
    }
    let excess = char_len(&state).saturating_sub(cfg.state_max_chars);
    if excess > 0 {
        shrink_text_at(&mut state, &["noth"], excess);
    }
    // recent を空にしても収まらないのは、trigger が単独で大きいとき。router に上限を超えて渡さない
    loop {
        let excess = char_len(&state).saturating_sub(cfg.state_max_chars);
        if excess == 0 || !shrink_longest_text(&mut state, excess) {
            break;
        }
    }
    RouterState(state)
}

/// 連続した目印はまとめ、まとめた後の数が上限を超えたら古い方を落とす。router が「Bot とのやりとりの最中だ」と分かればよい
fn fold_entries(rows: &[MessageRow]) -> Vec<Value> {
    markers::fold(rows, markers::MAX_MARKER_RUNS)
        .into_iter()
        .map(|item| match item {
            Folded::Row(r) => {
                let mut v = json!({
                    "channel_id": r.channel_id.to_string(),
                    "channel": r.channel_name,
                    "author": r.author_name,
                    "content": r.content,
                    "at": rfc3339(r.created_at),
                    "is_self": r.is_self,
                });
                insert_reply_to(&mut v, r.reply_to.as_ref());
                v
            }
            // 目印は本文を持たないので `content` の代わりに `bot_marker` を入れる
            Folded::Markers(run) => {
                let mut marker = json!({ "chars": run.chars, "count": run.count });
                if let Some(reply_to) = &run.reply_to_author {
                    marker["reply_to"] = json!(reply_to);
                }
                json!({
                    "channel_id": run.channel_id.to_string(),
                    "channel": run.channel_name,
                    "author": run.author_name,
                    "at": rfc3339(run.at),
                    "is_self": false,
                    "bot_marker": marker,
                })
            }
            Folded::Omitted {
                count,
                channel_name,
                at,
            } => json!({
                "channel": channel_name,
                "at": rfc3339(at),
                "omitted_bot_messages": count,
            }),
        })
        .collect()
}

/// `path` の文字列を末尾から `excess` 文字ぶん削る。空になったら key ごと外す
fn shrink_text_at(state: &mut Value, path: &[&str], excess: usize) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut parent = &mut *state;
    for key in parents {
        let Some(next) = parent.get_mut(*key) else {
            return;
        };
        parent = next;
    }
    let Some(text) = parent.get(*last).and_then(Value::as_str) else {
        return;
    };
    let keep = text.chars().count().saturating_sub(excess);
    if keep == 0 {
        if let Some(map) = parent.as_object_mut() {
            map.remove(*last);
        }
    } else {
        parent[*last] = Value::String(text.chars().take(keep).collect());
    }
}

/// メンバーの `memory` のうち最長のものを `excess` 文字ぶん削る。空になったら key ごと外す。削れるものが無ければ false
fn shrink_longest_member_memory(state: &mut Value, excess: usize) -> bool {
    // IndexMut で引くと無い key を作ってしまうので get_mut で辿る
    let Some(list) = state
        .get_mut("members")
        .and_then(|m| m.get_mut("list"))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let longest = list
        .iter()
        .enumerate()
        .map(|(i, m)| (i, m["memory"].as_str().map_or(0, |t| t.chars().count())))
        .filter(|(_, len)| *len > 0)
        .max_by_key(|(_, len)| *len);
    let Some((i, len)) = longest else {
        return false;
    };
    let keep = len.saturating_sub(excess);
    if keep == 0 {
        if let Some(map) = list[i].as_object_mut() {
            map.remove("memory");
        }
    } else {
        let text: String = list[i]["memory"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .take(keep)
            .collect();
        list[i]["memory"] = Value::String(text);
    }
    true
}

/// trigger の本文のうち最長のものを `excess` 文字ぶん削る。削れるものが無ければ false
fn shrink_longest_text(state: &mut Value, excess: usize) -> bool {
    let Some(items) = state["trigger"].as_array() else {
        return false;
    };
    let longest = items
        .iter()
        .enumerate()
        .map(|(i, item)| (i, item["content"].as_str().map_or(0, |t| t.chars().count())))
        .filter(|(_, len)| *len > 0)
        .max_by_key(|(_, len)| *len);
    let Some((i, len)) = longest else {
        return false;
    };
    let text = state["trigger"][i]["content"].as_str().unwrap_or_default();
    let shortened: String = text.chars().take(len.saturating_sub(excess)).collect();
    state["trigger"][i]["content"] = Value::String(shortened);
    true
}

fn char_len(v: &Value) -> usize {
    v.to_string().chars().count()
}

fn pick_choice(
    decision: &Decision,
    key: &str,
    options: &[&str],
    min_confidence: f32,
) -> Option<String> {
    let Some(Answer::Choice {
        choice,
        probabilities,
        confidence,
    }) = decision.answers.get(key)
    else {
        return None;
    };
    if *confidence < min_confidence {
        return None;
    }
    let mut best: Option<(&str, f32)> = None;
    for opt in options {
        if let Some(p) = probabilities.get(*opt)
            && best.is_none_or(|(_, bp)| *p > bp)
        {
            best = Some((opt, *p));
        }
    }
    match best {
        Some((opt, _)) => Some(opt.to_string()),
        None => options
            .iter()
            .find(|o| **o == choice)
            .map(|o| o.to_string()),
    }
}

/// 回答が欠けている・型が違う項目は安全側の値になる。
/// `addressee` と `reaction` に confidence の足切りはかけない(Jev の confidence は1位の確率の言い直しで効かないため)
pub fn binarize(decision: &Decision, reactions: &[Reaction], th: &Thresholds) -> JudgeAnswers {
    let noul = |key: &str, threshold: f32| matches!(decision.answers.get(key), Some(Answer::Noul { p }) if *p >= threshold);

    let addressee_names: Vec<&str> = Addressee::ALL.iter().map(|a| a.as_str()).collect();
    let addressee = pick_choice(decision, KEY_ADDRESSEE, &addressee_names, 0.0)
        .and_then(|name| Addressee::ALL.into_iter().find(|a| a.as_str() == name))
        .unwrap_or(Addressee::Other);

    let reaction_keys: Vec<String> = (0..reactions.len()).map(reaction_key).collect();
    let mut reaction_options: Vec<&str> = vec![REACTION_NONE];
    reaction_options.extend(reaction_keys.iter().map(String::as_str));
    let reaction = pick_choice(decision, KEY_REACTION, &reaction_options, 0.0)
        .and_then(|key| reaction_keys.iter().position(|k| *k == key))
        .map(|i| reactions[i].emoji.clone());

    JudgeAnswers {
        is_noise: noul(KEY_NOISE, th.is_noise_or_meme),
        addressee,
        needs_outside_work: noul(KEY_NEEDS_OUTSIDE_WORK, th.needs_outside_work),
        corrects_noth: noul(KEY_CORRECTS_NOTH, th.corrects_noth),
        worth_supplement: noul(KEY_WORTH_SUPPLEMENT, th.worth_supplement),
        reaction,
        is_topic_shift: noul(KEY_TOPIC_SHIFT, th.is_topic_shift),
    }
}

/// `decisions.answers` に入れる JSON。閾値の調整や分析のため、二値化前の確率と confidence をそのまま残す
pub fn answers_json(decision: &Decision) -> Value {
    let mut map = serde_json::Map::new();
    for (key, answer) in &decision.answers {
        let v = match answer {
            Answer::Noul { p } => json!({ "type": "noul", "p": p }),
            Answer::Choice {
                choice,
                probabilities,
                confidence,
            } => {
                json!({ "type": "choice", "choice": choice, "probabilities": probabilities, "confidence": confidence })
            }
            Answer::Score {
                score,
                probabilities,
                confidence,
            } => {
                let probs: serde_json::Map<String, Value> = probabilities
                    .iter()
                    .map(|(k, p)| (k.to_string(), json!(p)))
                    .collect();
                json!({ "type": "score", "score": score, "probabilities": probs, "confidence": confidence })
            }
        };
        map.insert(key.clone(), v);
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn choice(name: &str, p: f32, confidence: f32, options: &[&str]) -> Answer {
        let probabilities = options
            .iter()
            .map(|o| (o.to_string(), if *o == name { p } else { (1.0 - p) / 2.0 }))
            .collect();
        Answer::Choice {
            choice: name.into(),
            probabilities,
            confidence,
        }
    }

    fn decision(answers: Vec<(&str, Answer)>) -> Decision {
        Decision {
            answers: answers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            latency: Duration::ZERO,
        }
    }

    #[test]
    fn oversized_trigger_is_truncated_to_fit() {
        let cfg = RouterSection {
            state_max_chars: 500,
            ..serde_json::from_value(json!({})).unwrap()
        };
        let batch = JudgeBatch {
            channel_id: 1,
            channel_name: "general".into(),
            messages: vec![PendingMessage {
                row_id: 1,
                message_id: 1,
                author_name: "alice".into(),
                content: "あ".repeat(5_000),
                at: Timestamp::UNIX_EPOCH,
                mentions_self: false,
                reply_to_self: false,
                reply_to: None,
            }],
        };
        let state = build_state(&batch, &[], &Roster::default(), &cfg, &|_| false);
        assert!(char_len(&state.0) <= 500);
        assert!(
            !state.0["trigger"][0]["content"]
                .as_str()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn delivery_style_falls_back_to_abort() {
        let d = decision(vec![(
            KEY_DELIVERY,
            choice("reply_anchor", 0.9, 0.9, &DELIVERY_STYLES),
        )]);
        assert_eq!(binarize_delivery(&d, 0.15), DeliveryStyle::ReplyAnchor);
        let low = decision(vec![(
            KEY_DELIVERY,
            choice("reply_anchor", 0.9, 0.05, &DELIVERY_STYLES),
        )]);
        assert_eq!(binarize_delivery(&low, 0.15), DeliveryStyle::Abort);
        assert_eq!(
            binarize_delivery(&decision(vec![]), 0.15),
            DeliveryStyle::Abort
        );
    }

    #[test]
    fn join_state_fits_max_chars() {
        let cfg = RouterSection {
            state_max_chars: 400,
            ..serde_json::from_value(json!({})).unwrap()
        };
        let input = JoinInput {
            run_id: "r1",
            goal: "天気",
            channel: "general",
            concluded: true,
            memo: &"あ".repeat(2_000),
            since_fork: &[],
        };
        let state = build_join_state(&input, &cfg);
        assert!(char_len(&state.0) <= 400);
        assert_eq!(state.0["task"]["result"], "concluded");
    }

    fn reactions() -> Vec<Reaction> {
        vec![
            Reaction {
                emoji: "👍".into(),
                meaning: "同意".into(),
            },
            Reaction {
                emoji: "👀".into(),
                meaning: "気になる".into(),
            },
        ]
    }

    #[test]
    fn reply_to_appears_in_state_only_when_present() {
        let reply = ReplyTo {
            message_id: 7,
            author_name: "bob".into(),
            is_self: true,
            excerpt: "元の発言".into(),
        };
        let pending = |row_id, reply_to| PendingMessage {
            row_id,
            message_id: row_id as u64,
            author_name: "alice".into(),
            content: "はい".into(),
            at: Timestamp::UNIX_EPOCH,
            mentions_self: false,
            reply_to_self: false,
            reply_to,
        };
        let batch = JudgeBatch {
            channel_id: 1,
            channel_name: "general".into(),
            messages: vec![pending(1, Some(reply)), pending(2, None)],
        };
        let cfg: RouterSection = serde_json::from_value(json!({})).unwrap();
        let state = build_state(&batch, &[], &Roster::default(), &cfg, &|_| false);
        assert_eq!(
            state.0["trigger"][0]["reply_to"],
            json!({"author": "bob", "is_self": true, "excerpt": "元の発言"})
        );
        assert!(state.0["trigger"][1].get("reply_to").is_none());
    }

    fn one_message_batch() -> JudgeBatch {
        JudgeBatch {
            channel_id: 1,
            channel_name: "general".into(),
            messages: vec![PendingMessage {
                row_id: 1,
                message_id: 1,
                author_name: "alice".into(),
                content: "はい".into(),
                at: Timestamp::UNIX_EPOCH,
                mentions_self: false,
                reply_to_self: false,
                reply_to: None,
            }],
        }
    }

    fn member(name: &str, roles: &[&str]) -> Member {
        Member {
            user_id: match name {
                "alice" => 1,
                "bob" => 2,
                _ => 9,
            },
            display_name: name.into(),
            roles: roles.iter().map(|r| r.to_string()).collect(),
        }
    }

    #[test]
    fn state_carries_noth_and_members() {
        let cfg: RouterSection = serde_json::from_value(json!({})).unwrap();
        let members = [member("alice", &["admin"]), member("bob", &[])];
        let roster = Roster {
            noth: "Noth は Bot",
            members: &members,
            member_memory: &[
                (1, "- 開発者".into()),
                (1, "- 猫好き".into()),
                (3, "- 他人".into()),
            ],
        };
        let state = build_state(&one_message_batch(), &[], &roster, &cfg, &|_| false);
        assert_eq!(state.0["noth"], "Noth は Bot");
        assert_eq!(
            state.0["members"],
            json!({
                "list": [
                    {"name": "alice", "roles": ["admin"], "memory": "- 開発者\n- 猫好き"},
                    {"name": "bob", "roles": []},
                ],
            })
        );

        // 記憶もメンバーも無ければ key ごと省く
        let state = build_state(&one_message_batch(), &[], &Roster::default(), &cfg, &|_| {
            false
        });
        assert!(state.0.get("noth").is_none());
        assert!(state.0.get("members").is_none());
    }

    #[test]
    fn state_truncates_recent_then_longest_member_memory_then_noth() {
        let recent: Vec<MessageRow> = (1..=3)
            .map(|i| MessageRow {
                id: 100 + i,
                guild_id: 1,
                channel_id: 1,
                channel_name: "general".into(),
                message_id: i as u64,
                user_id: 1,
                author_name: "bob".into(),
                is_self: false,
                mentions_self: false,
                reply_to_self: false,
                reply_to: None,
                content: "い".repeat(100),
                created_at: Timestamp::UNIX_EPOCH,
                chunk_id: None,
                is_noise: None,
                decision_id: None,
                promoted_turn_id: None,
                bot_marker: None,
            })
            .collect();
        let members = [member("alice", &[]), member("bob", &[])];
        let memory = [(1, "a".repeat(300)), (2, "b".repeat(200))];
        let roster = Roster {
            noth: &"n".repeat(300),
            members: &members,
            member_memory: &memory,
        };
        let build = |max: usize, recent: &[MessageRow]| {
            let cfg = RouterSection {
                state_max_chars: max,
                ..serde_json::from_value(json!({})).unwrap()
            };
            build_state(&one_message_batch(), recent, &roster, &cfg, &|_| false).0
        };
        let full = char_len(&build(usize::MAX, &recent));
        let base = char_len(&build(usize::MAX, &[]));
        let len = |v: &Value, path: &[&str]| {
            path.iter()
                .try_fold(v, |v, k| v.get(*k))
                .and_then(Value::as_str)
                .map(|s| s.chars().count())
        };

        let mem = |v: &Value, i: usize| {
            v["members"]["list"][i]["memory"]
                .as_str()
                .map(|s| s.chars().count())
        };

        // 少し超えるだけなら recent だけが減る
        let v = build(full - 50, &recent);
        assert!(v["recent"].as_array().unwrap().len() < 3);
        assert_eq!(mem(&v, 0), Some(300));
        assert_eq!(mem(&v, 1), Some(200));
        assert_eq!(len(&v, &["noth"]), Some(300));

        // recent を空にしても超えるなら、最長のメンバーの記憶から削れ、noth は残る
        let v = build(base - 100, &recent);
        assert!(v["recent"].as_array().unwrap().is_empty());
        assert_eq!(mem(&v, 0), Some(200));
        assert_eq!(mem(&v, 1), Some(200));
        assert_eq!(len(&v, &["noth"]), Some(300));

        // 記憶が尽きるまで削っても超えるなら noth が削れる
        let v = build(base - 600, &recent);
        assert!(char_len(&v) <= base - 600);
        assert!(mem(&v, 0).is_none() && mem(&v, 1).is_none());
        assert!(len(&v, &["noth"]).unwrap() < 300);
        assert_eq!(v["members"]["list"][0]["name"], "alice");
    }

    #[test]
    fn recent_carries_reply_to_when_present() {
        let row = |id: i64, reply_to: Option<ReplyTo>| MessageRow {
            id,
            guild_id: 1,
            channel_id: 1,
            channel_name: "general".into(),
            message_id: id as u64,
            user_id: 1,
            author_name: "bob".into(),
            is_self: false,
            mentions_self: false,
            reply_to_self: false,
            reply_to,
            content: "x".into(),
            created_at: Timestamp::UNIX_EPOCH,
            chunk_id: None,
            is_noise: None,
            decision_id: None,
            promoted_turn_id: None,
            bot_marker: None,
        };
        let reply = ReplyTo {
            message_id: 7,
            author_name: "alice".into(),
            is_self: false,
            excerpt: "元".into(),
        };
        let recent = [row(101, Some(reply)), row(102, None)];
        let cfg: RouterSection = serde_json::from_value(json!({})).unwrap();
        let state = build_state(
            &one_message_batch(),
            &recent,
            &Roster::default(),
            &cfg,
            &|_| false,
        );
        assert_eq!(
            state.0["recent"][0]["reply_to"],
            json!({"author": "alice", "is_self": false, "excerpt": "元"})
        );
        assert!(state.0["recent"][1].get("reply_to").is_none());
    }

    #[test]
    fn shrinking_does_not_invent_missing_keys() {
        let cfg = RouterSection {
            state_max_chars: 600,
            ..serde_json::from_value(json!({})).unwrap()
        };
        let mut batch = one_message_batch();
        batch.messages[0].content = "あ".repeat(2000);
        let state = build_state(&batch, &[], &Roster::default(), &cfg, &|_| false);
        assert!(state.0.get("members").is_none());
        assert!(state.0.get("noth").is_none());
        assert!(char_len(&state.0) <= 600);
    }

    const ADDRESSEES: [&str; 4] = ["other", "about_noth", "other_bot", "noth_request"];

    fn texts() -> QuestionTexts {
        QuestionTexts::default()
    }

    fn answers(f: impl FnOnce(&mut JudgeAnswers)) -> JudgeAnswers {
        let mut a = JudgeAnswers::safe();
        f(&mut a);
        a
    }

    fn act(a: &JudgeAnswers, cued: bool, worker_enabled: bool) -> Action {
        decide_action(&ActionInput {
            answers: a,
            cued,
            noise: false,
            worker_enabled,
            supplement_allowed: true,
        })
    }

    #[test]
    fn questions_follow_the_set_and_reaction_starts_with_none() {
        let qs = build_questions(&texts(), &reactions());
        let keys: Vec<&str> = qs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "is_noise_or_meme",
                "addressee",
                "needs_outside_work",
                "corrects_noth",
                "worth_supplement",
                "reaction",
                "is_topic_shift"
            ]
        );
        assert_eq!(qs.len(), 7);
        let Some((_, Question::Choice { criteria, .. })) =
            qs.iter().find(|(k, _)| k == KEY_ADDRESSEE)
        else {
            panic!("addressee は choice");
        };
        let names: Vec<&str> = criteria.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ADDRESSEES);
        let Some((_, Question::Choice { criteria, .. })) =
            qs.iter().find(|(k, _)| k == KEY_REACTION)
        else {
            panic!("reaction は choice");
        };
        let names: Vec<&str> = criteria.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["none", "r0", "r1"]);
        assert_eq!(criteria[1].1, "👍 同意");

        let without = build_questions(&texts(), &[]);
        assert!(!without.iter().any(|(k, _)| k == KEY_REACTION));
        assert_eq!(without.len(), 6);
    }

    #[test]
    fn question_texts_can_be_overridden_from_config() {
        let cfg: RouterSection = serde_json::from_value(json!({
            "questions": { "addressee": { "other_bot": "別の Bot" }, "corrects_noth": "訂正？" }
        }))
        .unwrap();
        let qs = build_questions(&cfg.questions, &[]);
        let Some((
            _,
            Question::Choice {
                criteria,
                instructions,
            },
        )) = qs.iter().find(|(k, _)| k == KEY_ADDRESSEE)
        else {
            panic!("addressee は choice");
        };
        assert_eq!(criteria[2].1, "別の Bot");
        assert_eq!(instructions, "最後の発言は誰に向けたものか");
        assert!(matches!(
            qs.iter().find(|(k, _)| k == KEY_CORRECTS_NOTH),
            Some((_, Question::Noul { instructions })) if instructions == "訂正？"
        ));
    }

    #[test]
    fn binarize_applies_thresholds_and_picks_first_on_tie() {
        let th = Thresholds::default();
        let d = decision(vec![
            (
                KEY_ADDRESSEE,
                choice("noth_request", 0.9, 0.05, &ADDRESSEES),
            ),
            (KEY_NEEDS_OUTSIDE_WORK, Answer::Noul { p: 0.5 }),
            (KEY_CORRECTS_NOTH, Answer::Noul { p: 0.49 }),
            (KEY_WORTH_SUPPLEMENT, Answer::Noul { p: 0.7 }),
            (KEY_TOPIC_SHIFT, Answer::Noul { p: 0.59 }),
            (KEY_REACTION, choice("r1", 0.9, 0.0, &["none", "r0", "r1"])),
        ]);
        let b = binarize(&d, &reactions(), &th);
        // confidence が低くても addressee は足切りしない
        assert_eq!(b.addressee, Addressee::NothRequest);
        assert!(b.needs_outside_work && !b.corrects_noth && b.worth_supplement);
        assert!(!b.is_topic_shift);
        assert_eq!(b.reaction.as_deref(), Some("👀"));

        let tied = Answer::Choice {
            choice: "noth_request".into(),
            probabilities: ADDRESSEES.iter().map(|a| (a.to_string(), 0.25)).collect(),
            confidence: 0.5,
        };
        let b = binarize(&decision(vec![(KEY_ADDRESSEE, tied)]), &[], &th);
        assert_eq!(b.addressee, Addressee::Other);
    }

    #[test]
    fn reaction_none_and_empty_reactions_give_no_emoji() {
        let th = Thresholds::default();
        let d = decision(vec![(
            KEY_REACTION,
            choice("none", 0.9, 0.9, &["none", "r0", "r1"]),
        )]);
        assert_eq!(binarize(&d, &reactions(), &th).reaction, None);
        let d = decision(vec![(
            KEY_REACTION,
            choice("r0", 0.9, 0.9, &["none", "r0"]),
        )]);
        assert_eq!(binarize(&d, &[], &th).reaction, None);
    }

    #[test]
    fn missing_answers_are_safe() {
        assert_eq!(
            binarize(&decision(vec![]), &reactions(), &Thresholds::default()),
            JudgeAnswers::safe()
        );
    }

    #[test]
    fn cue_makes_noth_request_regardless_of_router() {
        let safe = JudgeAnswers::safe();
        assert_eq!(act(&safe, true, true), Action::ReplyOnly);
        let outside = answers(|a| a.needs_outside_work = true);
        assert_eq!(act(&outside, true, true), Action::SpawnTask);
        assert_eq!(act(&outside, true, false), Action::ReplyOnly);
        assert_eq!(act(&outside, false, true), Action::Silent);
        let corrects = answers(|a| a.corrects_noth = true);
        assert_eq!(act(&corrects, true, true), Action::SpawnTask);
    }

    #[test]
    fn about_noth_reacts_or_corrects_but_other_does_not_react() {
        let about = |f: fn(&mut JudgeAnswers)| {
            answers(|a| {
                a.addressee = Addressee::AboutNoth;
                f(a);
            })
        };
        assert_eq!(act(&about(|_| {}), false, true), Action::Silent);
        assert_eq!(
            act(&about(|a| a.reaction = Some("👍".into())), false, true),
            Action::React
        );
        assert_eq!(
            act(&about(|a| a.corrects_noth = true), false, true),
            Action::SilentCorrection
        );
        // worker が無ければ訂正はできないので、リアクションの判定に進む
        assert_eq!(
            act(&about(|a| a.corrects_noth = true), false, false),
            Action::Silent
        );
        assert_eq!(
            act(
                &about(|a| {
                    a.corrects_noth = true;
                    a.reaction = Some("👍".into());
                }),
                false,
                false
            ),
            Action::React
        );
        let other_with_reaction = answers(|a| a.reaction = Some("👍".into()));
        assert_eq!(act(&other_with_reaction, false, true), Action::Silent);
    }

    #[test]
    fn other_supplements_within_budget_and_other_bot_stays_silent() {
        let supplement = answers(|a| a.worth_supplement = true);
        assert_eq!(act(&supplement, false, true), Action::SilentSupplement);
        assert_eq!(act(&supplement, false, false), Action::Silent);
        let over_budget = ActionInput {
            answers: &supplement,
            cued: false,
            noise: false,
            worker_enabled: true,
            supplement_allowed: false,
        };
        assert_eq!(decide_action(&over_budget), Action::Silent);

        let both = answers(|a| {
            a.worth_supplement = true;
            a.corrects_noth = true;
        });
        assert_eq!(act(&both, false, true), Action::SilentSupplement);
        let over_budget = ActionInput {
            answers: &both,
            ..over_budget
        };
        assert_eq!(decide_action(&over_budget), Action::SilentCorrection);

        let bot = answers(|a| {
            a.addressee = Addressee::OtherBot;
            a.corrects_noth = true;
            a.worth_supplement = true;
        });
        assert_eq!(act(&bot, false, true), Action::Silent);
    }

    #[test]
    fn action_maps_to_origin() {
        assert_eq!(Action::SpawnTask.origin(), Some(Origin::Request));
        assert_eq!(Action::SilentSupplement.origin(), Some(Origin::Supplement));
        assert_eq!(Action::SilentCorrection.origin(), Some(Origin::Correction));
        assert_eq!(Action::ReplyOnly.origin(), None);
    }

    #[test]
    fn recent_carries_bot_marker_without_content() {
        let marker = MessageRow {
            id: 101,
            guild_id: 1,
            channel_id: 1,
            channel_name: "general".into(),
            message_id: 9,
            user_id: 5,
            author_name: "秘書ちゃん".into(),
            is_self: false,
            mentions_self: false,
            reply_to_self: false,
            reply_to: None,
            content: String::new(),
            created_at: Timestamp::UNIX_EPOCH,
            chunk_id: None,
            is_noise: None,
            decision_id: None,
            promoted_turn_id: None,
            bot_marker: Some(BotMarker {
                author_name: "秘書ちゃん".into(),
                reply_to_author: Some("くらず".into()),
                chars: 1200,
            }),
        };
        let cfg: RouterSection = serde_json::from_value(json!({})).unwrap();
        let state = build_state(
            &one_message_batch(),
            &[marker],
            &Roster::default(),
            &cfg,
            &|_| false,
        );
        let entry = &state.0["recent"][0];
        assert_eq!(entry["author"], "秘書ちゃん");
        assert_eq!(
            entry["bot_marker"],
            json!({"chars": 1200, "count": 1, "reply_to": "くらず"})
        );
        assert!(entry.get("content").is_none());
        assert!(state.0.get("candidates").is_none());
    }

    #[test]
    fn join_state_since_fork_carries_bot_marker_without_content() {
        let marker = MessageRow {
            id: 101,
            guild_id: 1,
            channel_id: 1,
            channel_name: "general".into(),
            message_id: 9,
            user_id: 5,
            author_name: "秘書ちゃん".into(),
            is_self: false,
            mentions_self: false,
            reply_to_self: false,
            reply_to: None,
            content: String::new(),
            created_at: Timestamp::UNIX_EPOCH,
            chunk_id: None,
            is_noise: None,
            decision_id: None,
            promoted_turn_id: None,
            bot_marker: Some(BotMarker {
                author_name: "秘書ちゃん".into(),
                reply_to_author: None,
                chars: 300,
            }),
        };
        let cfg: RouterSection = serde_json::from_value(json!({})).unwrap();
        let state = build_join_state(
            &JoinInput {
                run_id: "r1",
                goal: "g",
                channel: "general",
                concluded: true,
                memo: "m",
                since_fork: &[marker],
            },
            &cfg,
        );
        let entry = &state.0["since_fork"][0];
        assert_eq!(entry["bot_marker"]["chars"], 300);
        assert!(entry.get("content").is_none());
    }

    #[test]
    fn noise_is_dropped_unless_addressed_to_noth() {
        let th = Thresholds::default();
        let d = decision(vec![
            (KEY_NOISE, Answer::Noul { p: 0.5 }),
            (KEY_ADDRESSEE, choice("other", 0.9, 0.9, &ADDRESSEES)),
        ]);
        let b = binarize(&d, &[], &th);
        assert!(b.is_noise_for(false));
        assert!(
            !b.is_noise_for(true),
            "メンションやリプライはノイズにしない"
        );

        let d = decision(vec![
            (KEY_NOISE, Answer::Noul { p: 0.9 }),
            (KEY_ADDRESSEE, choice("noth_request", 0.9, 0.9, &ADDRESSEES)),
        ]);
        assert!(!binarize(&d, &[], &th).is_noise_for(false));
        let d = decision(vec![(KEY_NOISE, Answer::Noul { p: 0.49 })]);
        assert!(!binarize(&d, &[], &th).is_noise);
    }

    #[test]
    fn window_hint_is_carried_per_trigger_message() {
        let cfg: RouterSection = serde_json::from_value(json!({})).unwrap();
        let mut batch = one_message_batch();
        let mut second = batch.messages[0].clone();
        second.row_id = 2;
        second.author_name = "bob".into();
        batch.messages.push(second);
        let state = build_state(&batch, &[], &Roster::default(), &cfg, &|m| {
            m.author_name == "alice"
        });
        assert_eq!(state.0["trigger"][0]["in_conversation_window"], true);
        assert_eq!(state.0["trigger"][1]["in_conversation_window"], false);
    }

    #[test]
    fn noise_wakes_nothing_up() {
        let a = answers(|a| {
            a.worth_supplement = true;
            a.corrects_noth = true;
            a.addressee = Addressee::AboutNoth;
            a.reaction = Some("👍".into());
        });
        let input = |noise, cued| ActionInput {
            answers: &a,
            cued,
            noise,
            worker_enabled: true,
            supplement_allowed: true,
        };
        assert_eq!(decide_action(&input(true, false)), Action::Silent);
        // 依頼はノイズにならない(`is_noise_for` が false を返す)ので、noise=false で呼ばれ、行動する
        assert_eq!(decide_action(&input(false, true)), Action::SpawnTask);
    }

    #[test]
    fn join_questions_for_silent_runs_default_to_abort_wording() {
        let text = |o| match &join_questions(o)[0].1 {
            Question::Choice { instructions, .. } => instructions.clone(),
            _ => panic!("choice"),
        };
        assert!(text(Origin::Request).contains("頼まれた"));
        assert!(text(Origin::Supplement).contains("迷うときは abort"));
        let c = text(Origin::Correction);
        assert!(c.contains("指摘を受けて"));
        assert!(c.contains("答えを待っています"));
        assert!(c.contains("別の人がすでに同じ訂正をしたとき、または指摘が取り下げられたときだけ"));
        assert!(c.contains("後続の発言が無いことは、abort の理由になりません"));
        assert!(!c.contains("迷うときは abort"));
        assert!(c.contains(
            "理由(別の人がすでに同じ訂正をした、または指摘が取り下げられた)がなければ normal_report"
        ));
    }
}
