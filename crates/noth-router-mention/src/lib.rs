use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use jiff::Timestamp;
use noth_core::registry::{ModuleCtx, Registry};
use noth_core::text::contains_word;
use noth_core::traits::{Answer, Decision, Question, Router};
use noth_core::types::RouterState;
use noth_core::{Error, Result};
use serde_json::Value;

const DEFAULT_TOPIC_GAP_MINS: i64 = 30;

pub fn register(reg: &mut Registry) {
    reg.register_router("router-mention", factory);
}

fn factory(ctx: &ModuleCtx) -> Result<Arc<dyn Router>> {
    let keywords = match ctx.section.get("keywords") {
        None => Vec::new(),
        Some(v) => v
            .as_array()
            .ok_or_else(|| {
                Error::Config("router-mention.keywords は配列で指定してください".into())
            })?
            .iter()
            .map(|k| {
                k.as_str().map(str::to_lowercase).ok_or_else(|| {
                    Error::Config("router-mention.keywords の要素は文字列で指定してください".into())
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    let topic_gap_mins = match ctx.section.get("topic_gap_mins") {
        None => DEFAULT_TOPIC_GAP_MINS,
        Some(v) => v.as_integer().ok_or_else(|| {
            Error::Config("router-mention.topic_gap_mins は整数で指定してください".into())
        })?,
    };
    Ok(Arc::new(MentionRouter {
        keywords,
        topic_gap_mins,
    }))
}

pub struct MentionRouter {
    /// 小文字化済み
    keywords: Vec<String>,
    topic_gap_mins: i64,
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn items<'a>(state: &'a Value, key: &str) -> &'a [Value] {
    state
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

impl MentionRouter {
    fn addressed(&self, state: &Value) -> bool {
        items(state, "trigger").iter().any(|t| {
            if flag(t, "mentions_self") || flag(t, "reply_to_self") {
                return true;
            }
            let content = str_of(t, "content").unwrap_or("").to_lowercase();
            self.keywords.iter().any(|k| contains_word(&content, k))
        })
    }

    fn topic_shift(&self, state: &Value) -> bool {
        let trigger = items(state, "trigger");
        let Some(first) = trigger.first() else {
            return true;
        };
        let (Some(channel_id), Some(now)) = (
            str_of(first, "channel_id"),
            str_of(first, "at").and_then(|s| s.parse::<Timestamp>().ok()),
        ) else {
            return true;
        };
        let last = items(state, "recent")
            .iter()
            .rev()
            .find(|m| str_of(m, "channel_id") == Some(channel_id))
            .and_then(|m| str_of(m, "at"))
            .and_then(|s| s.parse::<Timestamp>().ok());
        match last {
            None => true,
            Some(last) => now.as_second() - last.as_second() >= self.topic_gap_mins * 60,
        }
    }

    fn answer(&self, state: &Value, key: &str, q: &Question) -> Answer {
        match key {
            "addressee" => choice(
                q,
                if self.addressed(state) {
                    "noth_request"
                } else {
                    "other"
                },
            ),
            "is_topic_shift" => Answer::Noul {
                p: if self.topic_shift(state) { 1.0 } else { 0.0 },
            },
            _ => match q {
                Question::Noul { .. } => Answer::Noul { p: 0.0 },
                Question::Choice { criteria, .. } => {
                    choice(q, criteria.first().map_or("", |(n, _)| n))
                }
                Question::Score { levels, .. } => score_first(levels.len()),
            },
        }
    }
}

/// 設計書の選択肢に無い名前を渡されても、criteria の先頭へ倒して全選択肢を埋める
fn choice(q: &Question, want: &str) -> Answer {
    let Question::Choice { criteria, .. } = q else {
        return Answer::Noul { p: 0.0 };
    };
    let chosen = if criteria.iter().any(|(n, _)| n == want) {
        want.to_string()
    } else {
        criteria.first().map(|(n, _)| n.clone()).unwrap_or_default()
    };
    let probabilities: HashMap<String, f32> = criteria
        .iter()
        .map(|(n, _)| (n.clone(), if *n == chosen { 1.0 } else { 0.0 }))
        .collect();
    Answer::Choice {
        choice: chosen,
        probabilities,
        confidence: 1.0,
    }
}

fn score_first(levels: usize) -> Answer {
    let probabilities = (0..levels.min(255) as u8)
        .map(|i| (i, if i == 0 { 1.0 } else { 0.0 }))
        .collect();
    Answer::Score {
        score: 0.0,
        probabilities,
        confidence: 1.0,
    }
}

#[async_trait]
impl Router for MentionRouter {
    async fn decide(
        &self,
        state: &RouterState,
        questions: &[(String, Question)],
    ) -> Result<Decision> {
        let start = Instant::now();
        let answers = questions
            .iter()
            .map(|(k, q)| (k.clone(), self.answer(&state.0, k, q)))
            .collect();
        Ok(Decision {
            answers,
            latency: start.elapsed(),
        })
    }

    fn model_label(&self) -> String {
        "router-mention".into()
    }

    async fn health_check(&self) -> Result<String> {
        Ok("rule-based".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn router(keywords: &[&str]) -> MentionRouter {
        MentionRouter {
            keywords: keywords.iter().map(|k| k.to_lowercase()).collect(),
            topic_gap_mins: 30,
        }
    }

    fn addressee() -> Question {
        Question::Choice {
            instructions: String::new(),
            criteria: ["other", "about_noth", "other_bot", "noth_request"]
                .map(|n| (n.to_string(), String::new()))
                .to_vec(),
        }
    }

    fn noul() -> Question {
        Question::Noul {
            instructions: String::new(),
        }
    }

    async fn run(r: &MentionRouter, state: Value, qs: Vec<(&str, Question)>) -> Decision {
        let qs: Vec<_> = qs.into_iter().map(|(k, q)| (k.to_string(), q)).collect();
        r.decide(&RouterState(state), &qs).await.unwrap()
    }

    fn trigger(content: &str, mentions: bool, reply: bool) -> Value {
        json!({"channel_id":"1","channel":"g","author":"a","content":content,"at":"2026-10-15T01:00:00Z",
               "mentions_self":mentions,"reply_to_self":reply})
    }

    fn action(d: &Decision) -> String {
        match &d.answers["addressee"] {
            Answer::Choice { choice, .. } => choice.clone(),
            _ => panic!("not choice"),
        }
    }

    #[tokio::test]
    async fn mention_and_reply_are_noth_request() {
        for t in [trigger("hi", true, false), trigger("hi", false, true)] {
            let d = run(
                &router(&[]),
                json!({"trigger":[t]}),
                vec![("addressee", addressee())],
            )
            .await;
            assert_eq!(action(&d), "noth_request");
        }
    }

    #[tokio::test]
    async fn keyword_is_case_insensitive() {
        let d = run(
            &router(&["Noth"]),
            json!({"trigger":[trigger("hey NOTH!", false, false)]}),
            vec![("addressee", addressee())],
        )
        .await;
        assert_eq!(action(&d), "noth_request");
    }

    #[test]
    fn keyword_respects_word_boundaries() {
        assert!(contains_word("hey noth!", "noth"));
        assert!(contains_word("noth", "noth"));
        assert!(contains_word("noth x", "noth"));
        assert!(!contains_word("nothing", "noth"));
        assert!(!contains_word("a_noth", "noth"));
        assert!(!contains_word("noth2", "noth"));
        assert!(contains_word("nothさん", "noth"));
        assert!(contains_word("ノスさん", "ノス"));
        assert!(contains_word("ノス、", "ノス"));
        assert!(contains_word("やあノス", "ノス"));
        assert!(!contains_word("ノスタルジー", "ノス"));
        assert!(!contains_word("ノスー", "ノス"));
        assert!(!contains_word("ノス", ""));
        // 2 回目の出現が境界を満たすケース
        assert!(contains_word("nothing noth", "noth"));
    }

    #[tokio::test]
    async fn otherwise_other_with_full_probabilities() {
        let d = run(
            &router(&["noth"]),
            json!({"trigger":[trigger("hello", false, false)]}),
            vec![("addressee", addressee())],
        )
        .await;
        let Answer::Choice {
            choice,
            probabilities,
            confidence,
        } = &d.answers["addressee"]
        else {
            panic!()
        };
        assert_eq!(choice, "other");
        assert_eq!(probabilities.len(), 4);
        assert_eq!(probabilities["other"], 1.0);
        assert_eq!(probabilities["noth_request"], 0.0);
        assert_eq!(*confidence, 1.0);
    }

    fn recent(channel_id: &str, at: &str, is_self: bool) -> Value {
        json!({"channel_id":channel_id,"channel":"g","author":"a","content":"x","at":at,"is_self":is_self})
    }

    async fn shift(recent: Vec<Value>) -> f32 {
        let d = run(
            &router(&[]),
            json!({"trigger":[trigger("hi", true, false)],"recent":recent}),
            vec![("is_topic_shift", noul())],
        )
        .await;
        let Answer::Noul { p } = d.answers["is_topic_shift"] else {
            panic!()
        };
        p
    }

    #[tokio::test]
    async fn topic_shift_rules() {
        assert_eq!(shift(vec![]).await, 1.0);
        assert_eq!(
            shift(vec![recent("2", "2026-10-15T00:59:00Z", false)]).await,
            1.0
        );
        assert_eq!(
            shift(vec![recent("1", "2026-10-15T00:50:00Z", false)]).await,
            0.0
        );
        assert_eq!(
            shift(vec![recent("1", "2026-10-15T00:30:00Z", false)]).await,
            1.0
        );
        // 末尾が Noth 自身の発言でも直近の発言として数える
        assert_eq!(
            shift(vec![
                recent("1", "2026-10-15T00:10:00Z", false),
                recent("1", "2026-10-15T00:55:00Z", true)
            ])
            .await,
            0.0
        );
    }

    #[tokio::test]
    async fn fixed_and_unknown_keys() {
        let score = Question::Score {
            instructions: String::new(),
            levels: vec!["a".into(), "b".into()],
        };
        let unknown_choice = Question::Choice {
            instructions: String::new(),
            criteria: vec![("x".into(), String::new()), ("y".into(), String::new())],
        };
        let reaction = Question::Choice {
            instructions: String::new(),
            criteria: ["none", "r0"]
                .map(|n| (n.to_string(), String::new()))
                .to_vec(),
        };
        let d = run(
            &router(&[]),
            json!({}),
            vec![
                ("needs_outside_work", noul()),
                ("worth_supplement", noul()),
                ("reaction", reaction),
                ("mystery", noul()),
                ("mystery_choice", unknown_choice),
                ("mystery_score", score),
            ],
        )
        .await;
        assert_eq!(d.answers.len(), 6);
        assert_eq!(d.answers["needs_outside_work"], Answer::Noul { p: 0.0 });
        assert_eq!(d.answers["worth_supplement"], Answer::Noul { p: 0.0 });
        assert_eq!(d.answers["mystery"], Answer::Noul { p: 0.0 });
        assert!(
            matches!(&d.answers["reaction"], Answer::Choice { choice, .. } if choice == "none")
        );
        assert!(
            matches!(&d.answers["mystery_choice"], Answer::Choice { choice, .. } if choice == "x")
        );
        assert!(
            matches!(&d.answers["mystery_score"], Answer::Score { score, .. } if *score == 0.0)
        );
    }

    #[tokio::test]
    async fn addressee_follows_mentions_replies_and_keywords() {
        let ask = |trigger: serde_json::Value| async move {
            let d = run(
                &router(&["noth"]),
                json!({ "trigger": [trigger] }),
                vec![("addressee", addressee())],
            )
            .await;
            let Answer::Choice { choice, .. } = &d.answers["addressee"] else {
                panic!("choice ではない");
            };
            choice == "noth_request"
        };
        assert!(ask(json!({"content": "こんにちは", "mentions_self": true})).await);
        assert!(ask(json!({"content": "はい", "reply_to_self": true})).await);
        assert!(ask(json!({"content": "noth、いる?"})).await);
        assert!(!ask(json!({"content": "ただの雑談"})).await);
    }

    #[tokio::test]
    async fn health_check() {
        let r = router(&[]);
        assert_eq!(r.health_check().await.unwrap(), "rule-based");
    }
}
