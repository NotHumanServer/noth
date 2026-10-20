use std::collections::HashMap;

use noth_core::traits::{Answer, Question};
use noth_core::{Error, Result};
use serde_json::{Map, Value, json};

const ERROR_BODY_LIMIT: usize = 200;

pub(crate) fn question_json(q: &Question) -> Value {
    match q {
        Question::Noul { instructions } => json!({"type": "noul", "instructions": instructions}),
        Question::Choice {
            instructions,
            criteria,
        } => {
            let criteria: Map<String, Value> = criteria
                .iter()
                .map(|(n, d)| (n.clone(), Value::String(d.clone())))
                .collect();
            json!({"type": "choice", "instructions": instructions, "criteria": criteria})
        }
        Question::Score {
            instructions,
            levels,
        } => {
            json!({"type": "score", "instructions": instructions, "criteria": levels})
        }
    }
}

pub(crate) fn build_request(model: &str, state: &Value, questions: &[(String, Question)]) -> Value {
    let questions: Map<String, Value> = questions
        .iter()
        .map(|(k, q)| (k.clone(), question_json(q)))
        .collect();
    json!({"model": model, "state": state, "questions": questions})
}

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::Other(format!("systemone のレスポンス: {msg}"))
}

fn prob(v: &Value, what: &str) -> Result<f32> {
    v.as_f64()
        .map(|f| f as f32)
        .ok_or_else(|| bad(format!("{what} is not a number")))
}

fn parse_answer(key: &str, v: &Value) -> Result<Answer> {
    let ty = v
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| bad(format!("{key}: missing type")))?;
    let field = |name: &str| {
        v.get(name)
            .ok_or_else(|| bad(format!("{key}: missing {name}")))
    };
    let confidence = || prob(field("confidence")?, "confidence");
    match ty {
        "noul" => Ok(Answer::Noul {
            p: prob(field("noul")?, "noul")?,
        }),
        "choice" => {
            let choice = field("choice")?
                .as_str()
                .ok_or_else(|| bad(format!("{key}: choice is not a string")))?;
            let probabilities = field("probabilities")?
                .as_object()
                .ok_or_else(|| bad(format!("{key}: probabilities is not an object")))?
                .iter()
                .map(|(k, p)| Ok((k.clone(), prob(p, "probability")?)))
                .collect::<Result<HashMap<_, _>>>()?;
            Ok(Answer::Choice {
                choice: choice.to_string(),
                probabilities,
                confidence: confidence()?,
            })
        }
        "score" => {
            let probabilities = field("probabilities")?
                .as_object()
                .ok_or_else(|| bad(format!("{key}: probabilities is not an object")))?
                .iter()
                .map(|(k, p)| {
                    let level = k
                        .parse::<u8>()
                        .map_err(|_| bad(format!("{key}: level `{k}` is not an integer")))?;
                    Ok((level, prob(p, "probability")?))
                })
                .collect::<Result<HashMap<_, _>>>()?;
            Ok(Answer::Score {
                score: prob(field("score")?, "score")?,
                probabilities,
                confidence: confidence()?,
            })
        }
        other => Err(bad(format!("{key}: unknown type `{other}`"))),
    }
}

/// Cloudflare REST は結果を `result` で包むことがあるが公式に明記が無いため、両方の形を受ける
pub(crate) fn parse_response(body: &Value) -> Result<HashMap<String, Answer>> {
    if body.get("answers").is_none() {
        if body.get("success").and_then(Value::as_bool) == Some(false) {
            return Err(bad(format!(
                "success=false: {}",
                body.get("errors").unwrap_or(&Value::Null)
            )));
        }
        if let Some(errors) = body
            .get("errors")
            .and_then(Value::as_array)
            .filter(|e| !e.is_empty())
        {
            return Err(bad(format!("errors: {}", Value::Array(errors.clone()))));
        }
    }
    let inner = if body.get("answers").is_some() {
        body
    } else {
        body.get("result").unwrap_or(body)
    };
    let answers = inner
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| bad("answers not found"))?;
    answers
        .iter()
        .map(|(k, v)| Ok((k.clone(), parse_answer(k, v)?)))
        .collect()
}

/// 改行を潰して 1 行にし、`401: Unauthorized` のような status 接頭辞は
/// Error::Api の表示と二重になるため外す
pub(crate) fn error_body(status: u16, s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = one_line
        .strip_prefix(&format!("{status}:"))
        .map_or(one_line.as_str(), str::trim_start);
    match trimmed.char_indices().nth(ERROR_BODY_LIMIT) {
        Some((i, _)) => format!("{}...", &trimmed[..i]),
        None => trimmed.to_string(),
    }
}
