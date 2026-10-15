//! router に渡す質問セットと state の構築、回答の二値化。context-layers.md §4
use std::collections::{HashMap, HashSet};

use jiff::Timestamp;
use serde_json::{Value, json};

use crate::config::{Reaction, RouterSection, Thresholds};
use crate::store::{ChunkRow, MessageRow};
use crate::text::contains_any_name;
use crate::traits::{Answer, Decision, Question};
use crate::types::*;

pub const KEY_NOISE: &str = "is_noise_or_meme";
pub const KEY_TOPIC_SHIFT: &str = "is_topic_shift";
pub const KEY_ADDRESSED: &str = "addressed_to_noth";
pub const KEY_ACTION: &str = "intervention_action";
pub const KEY_TASK_CATEGORY: &str = "task_category";
pub const KEY_DELIVERY: &str = "delivery_style";
pub const KEY_REACTION: &str = "reaction";

/// `is_noise_or_meme` など固定の質問の数。候補チャンクの上限は `max_questions() - FIXED_QUESTIONS`
pub const FIXED_QUESTIONS: usize = 6;

pub fn relevant_key(chunk_id: i64) -> String {
    format!("relevant_chunk_{chunk_id}")
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Silent,
    /// 発言はせず、トリガーの最後の発言に絵文字リアクションだけ付ける
    React,
    ReplyOnly,
    SpawnTask,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Silent => "silent",
            Action::React => "react",
            Action::ReplyOnly => "reply_only",
            Action::SpawnTask => "spawn_task",
        }
    }
}

/// Worker 合流時の報告の仕方(§4.3)
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

const ACTIONS: [&str; 4] = ["silent", "react", "reply_only", "spawn_task"];
const CATEGORIES: [&str; 4] = ["none", "web_research", "fact_check", "code_or_tool"];

#[derive(Debug, Clone, PartialEq)]
pub struct Binarized {
    pub is_noise: bool,
    pub is_topic_shift: bool,
    pub action: Action,
    pub task_category: TaskCategory,
    /// 関連と判定された候補チャンクの ID(候補の並び順)
    pub relevant: Vec<i64>,
    /// action が React のときに付ける絵文字
    pub reaction: Option<String>,
}

impl Binarized {
    /// router が使えないときの値。何も起こさず、バッファにも通常どおり入る
    pub fn safe() -> Self {
        Self {
            is_noise: false,
            is_topic_shift: false,
            action: Action::Silent,
            task_category: TaskCategory::None,
            relevant: Vec::new(),
            reaction: None,
        }
    }

    /// 名指しされたのに確率や confidence 次第で無視すると利用者には故障に見えるので、router の判定より優先して応答する。
    /// リアクションは無視ではなく応答なので、お礼への 🙏 のような判定はそのまま活かす
    pub fn answer_direct_call(&mut self) {
        self.is_noise = false;
        if self.action == Action::Silent {
            self.action = Action::ReplyOnly;
        }
    }

    /// worker 無効、または category が none の spawn_task は reply_only に読み替える(§4.2, パターン C)
    pub fn effective_action(&self, worker_enabled: bool) -> Action {
        match self.action {
            Action::SpawnTask if !worker_enabled || self.task_category == TaskCategory::None => {
                Action::ReplyOnly
            }
            a => a,
        }
    }
}

fn reaction_key(index: usize) -> String {
    format!("r{index}")
}

/// `reactions` が空ならリアクションの選択肢も質問も出さない
pub fn build_questions(candidates: &[ChunkRow], reactions: &[Reaction]) -> Vec<(String, Question)> {
    let mut action_criteria = vec![(
        "silent".to_string(),
        "静観する。Noth は何も発言しない".to_string(),
    )];
    if !reactions.is_empty() {
        action_criteria.push((
            "react".into(),
            "発言はせず、絵文字リアクションだけ付ける。話すほどではないが気になる・興味があるとき"
                .into(),
        ));
    }
    action_criteria.extend([
        (
            "reply_only".to_string(),
            "Noth が会話に返答する。調査や作業は要らない".to_string(),
        ),
        (
            "spawn_task".to_string(),
            "Noth が返答し、同時にウェブ調査やツールを使った作業を背景で始める".to_string(),
        ),
    ]);
    let react_hint = if reactions.is_empty() {
        ""
    } else {
        "口を挟むほどではないが、Noth が興味を持ちそうな話題、気になる知らせ、思わず反応したくなる発言には react を選びます。\
            ただし何にでも反応すると鬱陶しいので、ありふれた雑談や事務的なやりとりには付けません。"
    };
    let mut qs = vec![
        (
            KEY_NOISE.to_string(),
            Question::Noul {
                instructions: "今回のトリガー発言群が、スタンプや絵文字の連打、同じ文面のコピペ連投、意味のない短い反応の羅列など、\
                    後から読み返しても文脈上の価値がないノイズやミームかを判定してください。\
                    短くても質問・意見・情報・感情の表明を含む発言はノイズではありません。迷うときはノイズではないとしてください。"
                    .into(),
            },
        ),
        (
            KEY_TOPIC_SHIFT.to_string(),
            Question::Noul {
                instructions: "今回のトリガー発言群が、同じチャンネルの直前の会話(recent のうち同じ channel_id の発言)から\
                    話題が切り替わったかを判定してください。同じ話題の続きや、少し脱線しただけの発言は切り替わりではありません。\
                    別のチャンネルの発言は無関係として扱い、そのチャンネルに直前の発言がなければ切り替わりではないとしてください。"
                    .into(),
            },
        ),
        (
            KEY_ADDRESSED.to_string(),
            Question::Noul {
                instructions: "今回のトリガー発言群が、Noth に向けたものかを判定してください。\
                    Noth への質問や依頼、Noth 自身(その仕組みや設定、できることを含む)についての問いかけ、\
                    Noth の発言への返事は、Noth に向けたものです。Noth が何者かは state.noth を参考にしてください。\
                    人間同士が Noth を話題にしているだけで、Noth に話しかけていない発言は含みません。"
                    .into(),
            },
        ),
        (
            KEY_ACTION.to_string(),
            Question::Choice {
                instructions: format!("Noth はこのサーバーの仲間の一人で、少人数の自然な雑談には口を挟みすぎない存在です。\
                    トリガー発言群に対して Noth が何をするかを選んでください。\
                    Noth への呼びかけ(メンション、Noth の発言へのリプライ、Noth に向けた質問や依頼)には応えます。\
                    calls_noth_by_name が true の発言は本文に Noth の呼び名を含みます。Noth に話しかけているなら応え、\
                    Noth の開発や設定について人間同士で話しているだけなら静観します。\
                    Noth が直前に答えた話題の続きで、明らかに Noth に向けた返事のときも応えます。\
                    人間同士の雑談や独り言、すでに会話が成立している流れには入らず、静観します。{react_hint}\
                    迷うときは静観を選んでください。"
                ),
                criteria: action_criteria,
            },
        ),
        (
            KEY_TASK_CATEGORY.to_string(),
            Question::Choice {
                instructions: "トリガー発言群が Noth に求めている作業の種類を選んでください。\
                    ウェブでの調べもの、事実確認、コードの実行やツールを使った作業のいずれにも当たらない会話は none です。"
                    .into(),
                criteria: vec![
                    ("none".into(), "調査や作業は不要".into()),
                    ("web_research".into(), "ウェブで情報を調べる必要がある".into()),
                    ("fact_check".into(), "発言内容の事実確認が必要".into()),
                    ("code_or_tool".into(), "コードの実行やツールを使った作業が必要".into()),
                ],
            },
        ),
    ];
    if !reactions.is_empty() {
        qs.push((
            KEY_REACTION.to_string(),
            Question::Choice {
                instructions: "Noth がトリガー発言群の最後の発言に絵文字リアクションを付けるとしたら、どれが一番自然かを選んでください。\
                    発言の内容と、それを読んだ Noth の気持ちに合うものを選びます。"
                    .into(),
                criteria: reactions
                    .iter()
                    .enumerate()
                    .map(|(i, r)| (reaction_key(i), format!("{} {}", r.emoji, r.meaning)))
                    .collect(),
            },
        ));
    }
    for c in candidates {
        qs.push((
            relevant_key(c.id),
            Question::Noul {
                instructions: format!(
                    "candidates のうち id={} (要約: {}) の過去の会話が、今回のトリガー発言群に答えたり文脈を補ったりするために\
                     読み返す価値があるほど関連しているかを判定してください。話題が同じでも、今回の発言の理解に役立たないなら関連なしとしてください。",
                    c.id, c.summary
                ),
            },
        ));
    }
    qs
}

pub fn join_questions() -> Vec<(String, Question)> {
    vec![(
        KEY_DELIVERY.to_string(),
        Question::Choice {
            instructions: "Noth はメンバーに頼まれた調べものや作業を背景で進めていて、それが終わりました。\
                task は依頼の内容(goal)と結果です。result が concluded なら memo は結論メモ、failed なら memo は失敗の理由です。\
                since_fork は作業を始めてから今までに届いた全チャンネルの発言で、is_self が true のものは Noth 自身の発言です。\
                since_fork を読んで、この結果をどう伝えるかを選んでください。\
                依頼した人たちが自分で答えにたどり着いた、または別の人が答えたなど、人間側で自己解決しているなら brief_supplement です。\
                task.channel の会話がすでに別の話題に移っていて、普通に報告すると流れを遮るなら reply_anchor です。\
                依頼が取り消された、もう誰も気にしていない、結果に伝える価値がないなら abort です。\
                調べられなかった(failed)ことも、依頼した人が答えを待っているなら伝える価値があります。\
                どれにも当たらず、会話がまだその話題の上にあるなら normal_report です。"
                .into(),
            criteria: vec![
                ("normal_report".into(), "会話の流れの中で結果を普通に報告する".into()),
                ("brief_supplement".into(), "人間側で自己解決しているので、補足として短く伝える".into()),
                ("reply_anchor".into(), "話題が移っているので、元の依頼へのリプライとして報告する".into()),
                ("abort".into(), "報告しない".into()),
            ],
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
    let since_fork: Vec<Value> = input
        .since_fork
        .iter()
        .map(|r| {
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
        })
        .collect();
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
    candidates: &[ChunkRow],
    channel_names: &HashMap<ChannelId, String>,
    roster: &Roster,
    cfg: &RouterSection,
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
            });
            insert_reply_to(&mut v, m.reply_to.as_ref());
            v
        })
        .collect();

    let mut recent: Vec<Value> = recent
        .iter()
        .filter(|r| !trigger_ids.contains(&r.id))
        .map(|r| {
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
        })
        .collect();
    if recent.len() > cfg.recent_messages {
        recent.drain(..recent.len() - cfg.recent_messages);
    }

    let candidates: Vec<Value> = candidates
        .iter()
        .map(|c| {
            let channel = channel_names
                .get(&c.channel_id)
                .cloned()
                .unwrap_or_else(|| c.channel_id.to_string());
            json!({ "id": c.id, "channel": channel, "summary": c.summary })
        })
        .collect();

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
    state["candidates"] = json!(candidates);
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
    // recent を空にしても収まらないのは、trigger か候補要約が単独で大きいとき。router に上限を超えて渡さない
    loop {
        let excess = char_len(&state).saturating_sub(cfg.state_max_chars);
        if excess == 0 || !shrink_longest_text(&mut state, excess) {
            break;
        }
    }
    RouterState(state)
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

/// trigger の本文と候補の要約のうち最長のものを `excess` 文字ぶん削る。削れるものが無ければ false
fn shrink_longest_text(state: &mut Value, excess: usize) -> bool {
    let mut longest: Option<(&str, usize, &str, usize)> = None;
    for (section, field) in [("trigger", "content"), ("candidates", "summary")] {
        let Some(items) = state[section].as_array() else {
            continue;
        };
        for (i, item) in items.iter().enumerate() {
            let len = item[field].as_str().map_or(0, |t| t.chars().count());
            if len > 0 && longest.is_none_or(|(_, _, _, l)| len > l) {
                longest = Some((section, i, field, len));
            }
        }
    }
    let Some((section, i, field, len)) = longest else {
        return false;
    };
    let text = state[section][i][field].as_str().unwrap_or_default();
    let shortened: String = text.chars().take(len.saturating_sub(excess)).collect();
    state[section][i][field] = Value::String(shortened);
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

/// `none` が他の category と同点のとき、none 以外のうち先頭を採る。
/// Noth 宛ての依頼で none を採ると依頼が直答に読み替えられ、作業が始まらない
fn pick_task_category(decision: &Decision, min_confidence: f32, addressed: bool) -> Option<String> {
    let picked = pick_choice(decision, KEY_TASK_CATEGORY, &CATEGORIES, min_confidence);
    if !addressed || picked.as_deref() != Some("none") {
        return picked;
    }
    let Some(Answer::Choice { probabilities, .. }) = decision.answers.get(KEY_TASK_CATEGORY) else {
        return picked;
    };
    let none_p = probabilities.get("none").copied().unwrap_or(0.0);
    CATEGORIES[1..]
        .iter()
        .find(|c| {
            probabilities
                .get(**c)
                .is_some_and(|p| *p > 0.0 && *p >= none_p)
        })
        .map(|c| c.to_string())
        .or(picked)
}

/// 回答が欠けている・型が違う・confidence が足りない項目は安全側の値になる
/// `called_by_name` のときは介入判定の confidence を問わず、応答側の確率の合計が `name_call_reply` 以上なら応答する。
/// Clef は「のすくーん？」のような呼びかけでも静観を最大確率にしがちなため
pub fn binarize(
    decision: &Decision,
    candidate_ids: &[i64],
    reactions: &[Reaction],
    th: &Thresholds,
    called_by_name: bool,
    addressed_to_noth: bool,
) -> Binarized {
    let noul = |key: &str, threshold: f32| matches!(decision.answers.get(key), Some(Answer::Noul { p }) if *p >= threshold);

    let action_min_confidence = if called_by_name {
        0.0
    } else {
        th.min_confidence
    };
    let mut action =
        match pick_choice(decision, KEY_ACTION, &ACTIONS, action_min_confidence).as_deref() {
            Some("react") if !reactions.is_empty() => Action::React,
            Some("reply_only") => Action::ReplyOnly,
            Some("spawn_task") => Action::SpawnTask,
            _ => Action::Silent,
        };
    let reaction_keys: Vec<String> = (0..reactions.len()).map(reaction_key).collect();
    let reaction_options: Vec<&str> = reaction_keys.iter().map(String::as_str).collect();
    let reaction = pick_choice(decision, KEY_REACTION, &reaction_options, th.min_confidence)
        .and_then(|key| reaction_keys.iter().position(|k| *k == key))
        .map(|i| reactions[i].emoji.clone());
    // 絵文字が決まらないなら付けようがないので、何もしない側に倒す
    if action == Action::React && reaction.is_none() {
        action = Action::Silent;
    }
    if called_by_name
        && matches!(action, Action::Silent | Action::React)
        && let Some(Answer::Choice { probabilities, .. }) = decision.answers.get(KEY_ACTION)
    {
        let p = |k: &str| probabilities.get(k).copied().unwrap_or(0.0);
        if p("reply_only") + p("spawn_task") >= th.name_call_reply {
            action = Action::ReplyOnly;
        }
    }
    let addressed =
        called_by_name || addressed_to_noth || noul(KEY_ADDRESSED, th.addressed_to_noth);
    let task_category = match pick_task_category(decision, th.min_confidence, addressed).as_deref()
    {
        Some("web_research") => TaskCategory::WebResearch,
        Some("fact_check") => TaskCategory::FactCheck,
        Some("code_or_tool") => TaskCategory::CodeOrTool,
        _ => TaskCategory::None,
    };
    let mut binarized = Binarized {
        is_noise: noul(KEY_NOISE, th.is_noise_or_meme),
        is_topic_shift: noul(KEY_TOPIC_SHIFT, th.is_topic_shift),
        action,
        task_category,
        relevant: candidate_ids
            .iter()
            .copied()
            .filter(|id| noul(&relevant_key(*id), th.relevant_chunk))
            .collect(),
        reaction,
    };
    // 「誰に向けた発言か」と「割り込むべきか」を 1 問で聞くと Noth の仕組みの質問が静観されやすいので、宛先だけを別に聞く
    if noul(KEY_ADDRESSED, th.addressed_to_noth) {
        binarized.answer_direct_call();
    }
    binarized
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
    fn name_call_replies_when_reply_side_is_likely_enough() {
        let th = Thresholds::default();
        let silent_top = |reply: f32| {
            let mut probabilities = HashMap::new();
            probabilities.insert("silent".to_string(), 1.0 - reply);
            probabilities.insert("reply_only".to_string(), reply);
            probabilities.insert("spawn_task".to_string(), 0.0);
            decision(vec![(
                KEY_ACTION,
                Answer::Choice {
                    choice: "silent".into(),
                    probabilities,
                    confidence: 0.3,
                },
            )])
        };
        assert_eq!(
            binarize(&silent_top(0.29), &[], &[], &th, true, false).action,
            Action::ReplyOnly
        );
        assert_eq!(
            binarize(&silent_top(0.29), &[], &[], &th, false, false).action,
            Action::Silent
        );
        assert_eq!(
            binarize(&silent_top(0.1), &[], &[], &th, true, false).action,
            Action::Silent
        );
    }

    #[test]
    fn category_tie_with_none_yields_to_task_when_addressed() {
        let th = Thresholds::default();
        let tied = || Answer::Choice {
            choice: "none".into(),
            probabilities: [
                ("none", 0.45),
                ("web_research", 0.1),
                ("code_or_tool", 0.45),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
            confidence: 0.9,
        };
        let d = decision(vec![(KEY_TASK_CATEGORY, tied())]);
        assert_eq!(
            binarize(&d, &[], &[], &th, false, false).task_category,
            TaskCategory::None
        );
        assert_eq!(
            binarize(&d, &[], &[], &th, false, true).task_category,
            TaskCategory::CodeOrTool
        );
        assert_eq!(
            binarize(&d, &[], &[], &th, true, false).task_category,
            TaskCategory::CodeOrTool
        );
        // 宛先の確率が足りていれば、引数なしでも同じ
        let d = decision(vec![
            (KEY_TASK_CATEGORY, tied()),
            (KEY_ADDRESSED, Answer::Noul { p: 0.9 }),
        ]);
        assert_eq!(
            binarize(&d, &[], &[], &th, false, false).task_category,
            TaskCategory::CodeOrTool
        );
        // 全部 0 なら採るものが無いので none のまま
        let zero = Answer::Choice {
            choice: "none".into(),
            probabilities: CATEGORIES.iter().map(|c| (c.to_string(), 0.0)).collect(),
            confidence: 0.9,
        };
        let d = decision(vec![(KEY_TASK_CATEGORY, zero)]);
        assert_eq!(
            binarize(&d, &[], &[], &th, false, true).task_category,
            TaskCategory::None
        );
        // none の方が高ければ none のまま
        let d = decision(vec![(
            KEY_TASK_CATEGORY,
            choice("none", 0.9, 0.9, &CATEGORIES),
        )]);
        assert_eq!(
            binarize(&d, &[], &[], &th, false, true).task_category,
            TaskCategory::None
        );
    }

    #[test]
    fn thresholds_and_low_confidence() {
        let th = Thresholds::default();
        let d = decision(vec![
            (KEY_NOISE, Answer::Noul { p: 0.7 }),
            (KEY_TOPIC_SHIFT, Answer::Noul { p: 0.59 }),
            (KEY_ACTION, choice("reply_only", 0.9, 0.05, &ACTIONS)),
            (
                KEY_TASK_CATEGORY,
                choice("web_research", 0.9, 0.9, &CATEGORIES),
            ),
            ("relevant_chunk_5", Answer::Noul { p: 0.5 }),
            ("relevant_chunk_6", Answer::Noul { p: 0.1 }),
        ]);
        let b = binarize(&d, &[5, 6], &[], &th, false, false);
        assert!(b.is_noise);
        assert!(!b.is_topic_shift);
        assert_eq!(b.action, Action::Silent);
        assert_eq!(b.task_category, TaskCategory::WebResearch);
        assert_eq!(b.relevant, vec![5]);
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
        let state = build_state(&batch, &[], &[], &HashMap::new(), &Roster::default(), &cfg);
        assert!(char_len(&state.0) <= 500);
        assert!(
            !state.0["trigger"][0]["content"]
                .as_str()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn missing_answers_are_safe() {
        let b = binarize(
            &decision(vec![]),
            &[1],
            &[],
            &Thresholds::default(),
            false,
            false,
        );
        assert_eq!(b, Binarized::safe());
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

    #[test]
    fn spawn_task_reads_as_reply_only_without_worker() {
        let mut b = Binarized::safe();
        b.action = Action::SpawnTask;
        b.task_category = TaskCategory::WebResearch;
        assert_eq!(b.effective_action(false), Action::ReplyOnly);
        assert_eq!(b.effective_action(true), Action::SpawnTask);
        b.task_category = TaskCategory::None;
        assert_eq!(b.effective_action(true), Action::ReplyOnly);
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
    fn react_is_offered_only_with_reactions() {
        let with = build_questions(&[], &reactions());
        assert!(with.iter().any(|(k, _)| k == KEY_REACTION));
        let Some((_, Question::Choice { criteria, .. })) =
            with.iter().find(|(k, _)| k == KEY_ACTION)
        else {
            panic!("intervention_action が無い");
        };
        assert!(criteria.iter().any(|(n, _)| n == "react"));
        assert_eq!(with.len(), FIXED_QUESTIONS);

        let without = build_questions(&[], &[]);
        assert!(!without.iter().any(|(k, _)| k == KEY_REACTION));
        let Some((_, Question::Choice { criteria, .. })) =
            without.iter().find(|(k, _)| k == KEY_ACTION)
        else {
            panic!("intervention_action が無い");
        };
        assert!(!criteria.iter().any(|(n, _)| n == "react"));
    }

    #[test]
    fn react_picks_emoji_and_survives_direct_call() {
        let d = decision(vec![
            (KEY_ACTION, choice("react", 0.9, 0.9, &ACTIONS)),
            (KEY_REACTION, choice("r1", 0.9, 0.9, &["r0", "r1"])),
        ]);
        let mut b = binarize(&d, &[], &reactions(), &Thresholds::default(), false, false);
        assert_eq!(b.action, Action::React);
        assert_eq!(b.reaction.as_deref(), Some("👀"));
        b.answer_direct_call();
        assert_eq!(b.action, Action::React);

        // 設定でリアクションを空にしていれば react の回答は静観になる
        let b = binarize(&d, &[], &[], &Thresholds::default(), false, false);
        assert_eq!(b.action, Action::Silent);
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
        let state = build_state(&batch, &[], &[], &HashMap::new(), &Roster::default(), &cfg);
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
        let state = build_state(
            &one_message_batch(),
            &[],
            &[],
            &HashMap::new(),
            &roster,
            &cfg,
        );
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
        let state = build_state(
            &one_message_batch(),
            &[],
            &[],
            &HashMap::new(),
            &Roster::default(),
            &cfg,
        );
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
            build_state(
                &one_message_batch(),
                recent,
                &[],
                &HashMap::new(),
                &roster,
                &cfg,
            )
            .0
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
    fn addressed_to_noth_lifts_silent_to_reply() {
        let th = Thresholds::default();
        let silent = || (KEY_ACTION, choice("silent", 0.9, 0.9, &ACTIONS));
        let b = binarize(
            &decision(vec![silent(), (KEY_ADDRESSED, Answer::Noul { p: 0.6 })]),
            &[],
            &[],
            &th,
            false,
            false,
        );
        assert_eq!(b.action, Action::ReplyOnly);

        let b = binarize(
            &decision(vec![silent(), (KEY_ADDRESSED, Answer::Noul { p: 0.59 })]),
            &[],
            &[],
            &th,
            false,
            false,
        );
        assert_eq!(b.action, Action::Silent);

        // ノイズ判定より優先し、react はそのまま残す
        let d = decision(vec![
            (KEY_NOISE, Answer::Noul { p: 0.9 }),
            (KEY_ADDRESSED, Answer::Noul { p: 0.9 }),
            (KEY_ACTION, choice("react", 0.9, 0.9, &ACTIONS)),
            (KEY_REACTION, choice("r0", 0.9, 0.9, &["r0", "r1"])),
        ]);
        let b = binarize(&d, &[], &reactions(), &th, false, false);
        assert!(!b.is_noise);
        assert_eq!(b.action, Action::React);
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
            &[],
            &HashMap::new(),
            &Roster::default(),
            &cfg,
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
        let state = build_state(&batch, &[], &[], &HashMap::new(), &Roster::default(), &cfg);
        assert!(state.0.get("members").is_none());
        assert!(state.0.get("noth").is_none());
        assert!(char_len(&state.0) <= 600);
    }

    #[test]
    fn addressed_question_is_a_noul() {
        let qs = build_questions(&[], &[]);
        assert!(matches!(
            qs.iter().find(|(k, _)| k == KEY_ADDRESSED),
            Some((_, Question::Noul { .. }))
        ));
    }
}
