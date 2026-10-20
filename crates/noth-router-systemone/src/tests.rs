use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use noth_core::Result;
use noth_core::traits::{Answer, Question, Router};
use serde_json::{Value, json};

use crate::preset::from_section;
use crate::retry::retry_delay;
use crate::validate::validated_answers;
use crate::wire::{build_request, error_body, parse_response};

fn qs() -> Vec<(String, Question)> {
    vec![
        (
            "k1".into(),
            Question::Noul {
                instructions: "n".into(),
            },
        ),
        (
            "k2".into(),
            Question::Choice {
                instructions: "c".into(),
                criteria: vec![
                    ("silent".into(), "黙る".into()),
                    ("reply_only".into(), "返信".into()),
                ],
            },
        ),
        (
            "k3".into(),
            Question::Score {
                instructions: "s".into(),
                levels: vec!["Minor".into(), "Major".into()],
            },
        ),
    ]
}

fn sample_response() -> Value {
    json!({"model":"clef-flash","answers":{
        "k1":{"type":"noul","noul":0.9},
        "k2":{"type":"choice","choice":"silent","probabilities":{"silent":0.8,"reply_only":0.2},"confidence":0.78},
        "k3":{"type":"score","score":1.6,"legend":{"0":"Minor","1":"Major"},"probabilities":{"0":0.4,"1":0.6},"confidence":0.7}
    },"usage":{"input_tokens":10}})
}

#[test]
fn request_json_shape() {
    let r = build_request("clef-flash", &json!({"trigger": []}), &qs());
    assert_eq!(r["model"], "clef-flash");
    assert_eq!(r["state"], json!({"trigger": []}));
    assert_eq!(
        r["questions"]["k1"],
        json!({"type":"noul","instructions":"n"})
    );
    assert_eq!(r["questions"]["k2"]["criteria"]["reply_only"], "返信");
    assert_eq!(
        r["questions"]["k3"],
        json!({"type":"score","instructions":"s","criteria":["Minor","Major"]})
    );
}

fn check(answers: &HashMap<String, Answer>) {
    assert_eq!(answers["k1"], Answer::Noul { p: 0.9 });
    let Answer::Choice {
        choice,
        probabilities,
        confidence,
    } = &answers["k2"]
    else {
        panic!()
    };
    assert_eq!(choice, "silent");
    assert_eq!(probabilities["reply_only"], 0.2);
    assert_eq!(*confidence, 0.78);
    let Answer::Score {
        score,
        probabilities,
        confidence,
    } = &answers["k3"]
    else {
        panic!()
    };
    assert_eq!(*score, 1.6);
    assert_eq!(probabilities[&1], 0.6);
    assert_eq!(*confidence, 0.7);
}

#[test]
fn parses_plain_response() {
    check(&parse_response(&sample_response()).unwrap());
}

#[test]
fn parses_result_wrapped_response() {
    let wrapped =
        json!({"result": sample_response(), "success": true, "errors": [], "messages": []});
    check(&parse_response(&wrapped).unwrap());
}

#[test]
fn rejects_failure_and_malformed() {
    let failed =
        json!({"result": null, "success": false, "errors": [{"code": 10000, "message": "auth"}]});
    assert!(
        parse_response(&failed)
            .unwrap_err()
            .to_string()
            .contains("10000")
    );
    assert!(parse_response(&json!({"success": true})).is_err());
    assert!(parse_response(&json!({"answers": {"k": {"type": "noul"}}})).is_err());
    assert!(parse_response(&json!({"answers": {"k": {"type": "zzz"}}})).is_err());
}

#[test]
fn error_body_is_one_line_truncated_without_status_prefix() {
    assert_eq!(error_body(500, "abc"), "abc");
    assert_eq!(error_body(401, "401: Unauthorized"), "Unauthorized");
    assert_eq!(error_body(400, "a\n b"), "a b");
    let long = "あ".repeat(600);
    assert_eq!(error_body(500, &long).chars().count(), 203);
}

#[test]
fn retry_policy() {
    let s = Duration::from_secs;
    assert_eq!(retry_delay(429, None, 0), Some(Duration::from_millis(500)));
    assert_eq!(retry_delay(503, None, 1), Some(Duration::from_secs(1)));
    assert_eq!(retry_delay(503, None, 2), None);
    assert_eq!(retry_delay(400, None, 0), None);
    assert_eq!(retry_delay(401, None, 0), None);
    assert_eq!(retry_delay(429, Some(s(3)), 0), Some(s(3)));
    assert_eq!(retry_delay(429, Some(s(6)), 0), None);
}

#[test]
fn invalid_answers_are_dropped() {
    let body = json!({"answers":{
        "k1":{"type":"noul","noul":1.5},
        "k2":{"type":"choice","choice":"other","probabilities":{"silent":0.8},"confidence":0.7},
        "k3":{"type":"score","score":1.6,"probabilities":{"0":0.4,"1":0.6},"confidence":0.7},
        "extra":{"type":"noul","noul":0.1}
    }});
    let answers = validated_answers(parse_response(&body).unwrap(), &qs());
    assert_eq!(answers.keys().collect::<Vec<_>>(), vec!["k3"]);

    let bad_prob = json!({"answers":{
        "k2":{"type":"choice","choice":"silent","probabilities":{"silent":-0.1},"confidence":0.7},
        "k3":{"type":"score","score":-0.5,"probabilities":{"0":0.4},"confidence":0.7}
    }});
    assert!(validated_answers(parse_response(&bad_prob).unwrap(), &qs()).is_empty());

    let ok = validated_answers(parse_response(&sample_response()).unwrap(), &qs());
    assert_eq!(ok.len(), 3);
}

fn ctx_for(section: &str) -> Result<Arc<dyn Router>> {
    from_section(&toml::from_str(section).unwrap())
}

#[test]
fn factory_validates_required_keys() {
    assert!(ctx_for("").is_err());
    assert!(ctx_for("api_key = \"k\"").is_ok());
    assert!(ctx_for("preset = \"clef\"\napi_key = \"k\"").is_err());
    assert!(ctx_for("preset = \"clef\"\napi_key = \"k\"\naccount_id = \"a\"").is_ok());
    assert!(ctx_for("preset = \"jev\"\napi_key = \"k\"").is_ok());
    assert!(ctx_for("preset = \"custom\"\napi_key = \"k\"").is_err());
    assert!(ctx_for("preset = \"custom\"\napi_key = \"k\"\nendpoint = \"http://x\"").is_ok());
    assert!(ctx_for("preset = \"nope\"\napi_key = \"k\"").is_err());
}
