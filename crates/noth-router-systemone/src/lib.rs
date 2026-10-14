use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use noth_core::registry::{ModuleCtx, Registry};
use noth_core::traits::{Answer, Decision, Question, Router};
use noth_core::types::RouterState;
use noth_core::{Error, Result};
use serde_json::{Map, Value, json};

const JEV_URL: &str = "https://api.typesafe.ai/v1/systemone";
const ERROR_BODY_LIMIT: usize = 200;
const MAX_RETRIES: u32 = 2;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(5);

pub fn register(reg: &mut Registry) {
    reg.register_router("router-systemone", factory);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Preset {
    Jev,
    Clef,
    Custom,
}

impl Preset {
    fn as_str(self) -> &'static str {
        match self {
            Preset::Jev => "jev",
            Preset::Clef => "clef",
            Preset::Custom => "custom",
        }
    }
}

fn factory(ctx: &ModuleCtx) -> Result<Arc<dyn Router>> {
    from_section(&ctx.section)
}

fn from_section(s: &toml::Value) -> Result<Arc<dyn Router>> {
    let text = |key: &str| {
        s.get(key)
            .and_then(toml::Value::as_str)
            .filter(|v| !v.is_empty())
    };
    let need = |key: &str| {
        text(key)
            .map(str::to_string)
            .ok_or_else(|| Error::Config(format!("router-systemone.{key} が必要です")))
    };
    let int = |key: &str, default: i64| match s.get(key) {
        None => Ok(default),
        Some(v) => v.as_integer().filter(|n| *n > 0).ok_or_else(|| {
            Error::Config(format!(
                "router-systemone.{key} は正の整数で指定してください"
            ))
        }),
    };

    let preset = match text("preset").unwrap_or("jev") {
        "jev" => Preset::Jev,
        "clef" => Preset::Clef,
        "custom" => Preset::Custom,
        other => {
            return Err(Error::Config(format!(
                "router-systemone.preset `{other}` は未知です(jev|clef|custom)"
            )));
        }
    };
    let default_model = if preset == Preset::Jev {
        "jev-latest"
    } else {
        "clef"
    };
    let model = text("model").unwrap_or(default_model).to_string();
    let api_key = need("api_key")?;
    let url = match preset {
        Preset::Jev => JEV_URL.to_string(),
        Preset::Clef => format!(
            "https://api.cloudflare.com/client/v4/accounts/{}/ai/run/@cf/cloudflare/{model}",
            need("account_id")?
        ),
        Preset::Custom => need("endpoint")?,
    };
    let timeout = Duration::from_secs(int("timeout_secs", 10)? as u64);
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| {
            Error::Config(format!(
                "router-systemone: HTTP クライアントを作れません: {e}"
            ))
        })?;
    Ok(Arc::new(SystemOne {
        client,
        preset,
        model,
        api_key,
        url,
        max_questions: int("max_questions", 64)? as usize,
    }))
}

pub struct SystemOne {
    client: reqwest::Client,
    preset: Preset,
    model: String,
    api_key: String,
    url: String,
    max_questions: usize,
}

fn question_json(q: &Question) -> Value {
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

fn build_request(model: &str, state: &Value, questions: &[(String, Question)]) -> Value {
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
fn parse_response(body: &Value) -> Result<HashMap<String, Answer>> {
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
fn error_body(status: u16, s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = one_line
        .strip_prefix(&format!("{status}:"))
        .map_or(one_line.as_str(), str::trim_start);
    match trimmed.char_indices().nth(ERROR_BODY_LIMIT) {
        Some((i, _)) => format!("{}...", &trimmed[..i]),
        None => trimmed.to_string(),
    }
}

/// 再試行するなら待ち時間を返す。Retry-After が長すぎる場合は待たずに失敗させる
fn retry_delay(status: u16, retry_after: Option<Duration>, attempt: u32) -> Option<Duration> {
    if attempt >= MAX_RETRIES || !matches!(status, 429 | 500..=599) {
        return None;
    }
    match retry_after {
        Some(d) if d > MAX_RETRY_AFTER => None,
        Some(d) => Some(d),
        None => Some(Duration::from_millis(500 << attempt)),
    }
}

fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let secs = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

fn in_unit(p: f32) -> bool {
    (0.0..=1.0).contains(&p)
}

/// 要求していない形・範囲外の値は不正として理由を返す
fn validate(answer: &Answer, question: &Question) -> std::result::Result<(), String> {
    match (answer, question) {
        (Answer::Noul { p }, Question::Noul { .. }) if in_unit(*p) => Ok(()),
        (Answer::Noul { p }, Question::Noul { .. }) => Err(format!("noul {p} が範囲外")),
        (
            Answer::Choice {
                choice,
                probabilities,
                confidence,
            },
            Question::Choice { criteria, .. },
        ) => {
            if !criteria.iter().any(|(n, _)| n == choice) {
                return Err(format!("選択肢にない choice `{choice}`"));
            }
            if !in_unit(*confidence) || !probabilities.values().all(|p| in_unit(*p)) {
                return Err("確率または confidence が範囲外".into());
            }
            Ok(())
        }
        (
            Answer::Score {
                score,
                probabilities,
                confidence,
            },
            Question::Score { .. },
        ) => {
            // score の値域は仕様が不明(既存のサンプルは 1.6)なので、負値と非有限値だけ弾く
            if !score.is_finite() || *score < 0.0 {
                return Err(format!("score {score} が範囲外"));
            }
            if !in_unit(*confidence) || !probabilities.values().all(|p| in_unit(*p)) {
                return Err("確率または confidence が範囲外".into());
            }
            Ok(())
        }
        _ => Err("質問と回答の type が一致しない".into()),
    }
}

/// 欠落・不正な回答は map に入れない。core の二値化は欠けた回答を安全側に倒す
fn validated_answers(
    mut parsed: HashMap<String, Answer>,
    questions: &[(String, Question)],
) -> HashMap<String, Answer> {
    let mut out = HashMap::new();
    for (key, question) in questions {
        match parsed.remove(key) {
            None => tracing::warn!(key, "systemone の回答に要求したキーがありません"),
            Some(answer) => match validate(&answer, question) {
                Ok(()) => {
                    out.insert(key.clone(), answer);
                }
                Err(reason) => tracing::warn!(key, reason, "systemone の回答を破棄します"),
            },
        }
    }
    out
}

impl SystemOne {
    async fn call(&self, state: &Value, questions: &[(String, Question)]) -> Result<Decision> {
        let request = build_request(&self.model, state, questions);
        let start = Instant::now();
        let mut attempt = 0;
        let (text, latency) = loop {
            let resp = self
                .client
                .post(&self.url)
                .bearer_auth(&self.api_key)
                .json(&request)
                .send()
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
            let status = resp.status();
            let wait = retry_after(&resp);
            let text = resp
                .text()
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
            if status.is_success() {
                break (text, start.elapsed());
            }
            let code = status.as_u16();
            if let Some(delay) = retry_delay(code, wait, attempt) {
                tracing::warn!(
                    status = code,
                    attempt,
                    ?delay,
                    "systemone が失敗したため再試行します"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }
            return Err(Error::Api {
                status: code,
                body: error_body(code, &text),
            });
        };
        let body: Value = serde_json::from_str(&text)?;
        Ok(Decision {
            answers: validated_answers(parse_response(&body)?, questions),
            latency,
        })
    }
}

#[async_trait]
impl Router for SystemOne {
    async fn decide(
        &self,
        state: &RouterState,
        questions: &[(String, Question)],
    ) -> Result<Decision> {
        self.call(&state.0, questions).await
    }

    fn max_questions(&self) -> usize {
        self.max_questions
    }

    fn model_label(&self) -> String {
        format!("{}/{}", self.preset.as_str(), self.model)
    }

    async fn health_check(&self) -> Result<String> {
        let probe = [(
            "ping".to_string(),
            Question::Noul {
                instructions: "Is this text a greeting?".into(),
            },
        )];
        let d = self.call(&json!("hello"), &probe).await?;
        Ok(format!(
            "preset {} / model {} / {}ms",
            self.preset.as_str(),
            self.model,
            d.latency.as_millis()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let failed = json!({"result": null, "success": false, "errors": [{"code": 10000, "message": "auth"}]});
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
}
