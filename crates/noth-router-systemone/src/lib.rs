mod preset;
mod retry;
mod validate;
mod wire;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use noth_core::registry::{ModuleCtx, Registry};
use noth_core::traits::{Decision, Question, Router};
use noth_core::types::RouterState;
use noth_core::{Error, Result};
use serde_json::{Value, json};

use preset::{Preset, from_section};
use retry::{retry_after, retry_delay};
use validate::validated_answers;
use wire::{build_request, error_body, parse_response};

pub fn register(reg: &mut Registry) {
    reg.register_router("router-systemone", factory);
}

fn factory(ctx: &ModuleCtx) -> Result<Arc<dyn Router>> {
    from_section(&ctx.section)
}

pub struct SystemOne {
    client: reqwest::Client,
    preset: Preset,
    model: String,
    api_key: String,
    url: String,
    max_questions: usize,
}

impl SystemOne {
    async fn call(&self, state: &Value, questions: &[(String, Question)]) -> Result<Decision> {
        // API の上限を超えた質問はサーバー側で弾かれる。再試行しても同じなので送る前に落とす
        if questions.len() > self.max_questions {
            return Err(Error::Other(format!(
                "質問が {} 件あり、1 リクエストの上限 {} を超えています",
                questions.len(),
                self.max_questions
            )));
        }
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
