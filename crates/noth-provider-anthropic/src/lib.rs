//! Anthropic Messages API の provider。リクエスト構築・応答解析は `wire` の純粋関数に分けてある
mod wire;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use noth_core::registry::{ModuleCtx, Registry};
use noth_core::traits::{Completion, LayeredPrompt, Provider};
use noth_core::types::CompletionParams;
use noth_core::{Error, Result};
use reqwest::StatusCode;
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai/inference/go/v1";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const BACKOFF_CAP: Duration = Duration::from_secs(30);

pub fn register(reg: &mut Registry) {
    reg.register_provider("provider-anthropic", factory);
}

#[derive(Deserialize)]
struct Settings {
    api_key: String,
    model: String,
    #[serde(default = "default_cache_ttl")]
    cache_ttl: String,
    preset: Option<String>,
    base_url: Option<String>,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    effort: Option<String>,
}

fn default_cache_ttl() -> String {
    "5m".into()
}

fn default_timeout_secs() -> u64 {
    120
}

fn factory(ctx: &ModuleCtx) -> Result<Arc<dyn Provider>> {
    let s: Settings = ctx
        .section
        .clone()
        .try_into()
        .map_err(|e| Error::Config(format!("provider.anthropic: {e}")))?;
    let ttl = wire::CacheTtl::parse(&s.cache_ttl)?;
    let effort = s.effort.as_deref().map(wire::parse_effort).transpose()?;
    let preset_url = match s.preset.as_deref() {
        None => DEFAULT_BASE_URL,
        Some("opencode-go") => OPENCODE_GO_BASE_URL,
        Some(other) => {
            return Err(Error::Config(format!(
                "provider.anthropic: 未知の preset `{other}`"
            )));
        }
    };
    let base_url = s
        .base_url
        .unwrap_or_else(|| preset_url.to_string())
        .trim_end_matches('/')
        .to_string();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(s.timeout_secs))
        .build()
        .map_err(|e| {
            Error::Config(format!(
                "provider.anthropic: HTTP クライアントを作れません: {e}"
            ))
        })?;
    Ok(Arc::new(AnthropicProvider {
        client,
        base_url,
        api_key: s.api_key,
        model: s.model,
        ttl,
        effort,
        retry_max: ctx.config.context.retry_max,
        preset: s.preset,
    }))
}

struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    ttl: wire::CacheTtl,
    effort: Option<&'static str>,
    retry_max: u32,
    preset: Option<String>,
}

enum Attempt {
    Retry {
        err: Error,
        retry_after: Option<Duration>,
    },
    Fatal(Error),
}

impl AnthropicProvider {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
    }

    async fn post_once(
        &self,
        body: &serde_json::Value,
    ) -> std::result::Result<serde_json::Value, Attempt> {
        let resp = self
            .request(reqwest::Method::POST, "/messages")
            .json(body)
            .send()
            .await
            .map_err(|e| {
                let err = Error::Transport(e.to_string());
                // リクエストを組み立てられない失敗は何度送っても直らない
                if e.is_builder() {
                    Attempt::Fatal(err)
                } else {
                    Attempt::Retry {
                        err,
                        retry_after: None,
                    }
                }
            })?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        if status.is_success() {
            // 本文を読み切れない通信断は再試行で直るが、200 で JSON が壊れているのは直らない
            let bytes = resp.bytes().await.map_err(|e| Attempt::Retry {
                err: Error::Transport(e.to_string()),
                retry_after: None,
            })?;
            return serde_json::from_slice(&bytes).map_err(|e| Attempt::Fatal(Error::Json(e)));
        }
        let err = Error::Api {
            status: status.as_u16(),
            body: error_body(status.as_u16(), &resp.text().await.unwrap_or_default()),
        };
        if is_retryable(status) {
            Err(Attempt::Retry { err, retry_after })
        } else {
            Err(Attempt::Fatal(err))
        }
    }
}

// 529 は Anthropic 固有の overloaded。標準の 5xx 判定に含まれる
fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Retry-After が上限より長いなら、早めに再送しても弾かれるだけなので待たずに諦める
fn backoff(attempt: u32, retry_after: Option<Duration>) -> Option<Duration> {
    match retry_after {
        Some(d) if d > BACKOFF_CAP => None,
        Some(d) => Some(d),
        None => Some(Duration::from_secs(1u64 << attempt.min(5)).min(BACKOFF_CAP)),
    }
}

/// API のエラー JSON があれば message を使い、無ければ生の本文を 1 行に潰して短くする。
/// status 接頭辞は Error::Api の表示と二重になるため外す
fn error_body(status: u16, raw: &str) -> String {
    const MAX_CHARS: usize = 200;
    let message = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| raw.to_string());
    let one_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = one_line
        .strip_prefix(&format!("{status}:"))
        .map_or(one_line.as_str(), str::trim_start);
    match trimmed.char_indices().nth(MAX_CHARS) {
        Some((i, _)) => format!("{}...", &trimmed[..i]),
        None => trimmed.to_string(),
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn complete(
        &self,
        prompt: &LayeredPrompt,
        params: &CompletionParams,
    ) -> Result<Completion> {
        let mut body = wire::build_request(&self.model, prompt, params, self.ttl)?;
        if let Some(effort) = self.effort {
            body["output_config"] = serde_json::json!({ "effort": effort });
        }
        let mut attempt = 0;
        loop {
            match self.post_once(&body).await {
                Ok(resp) => return wire::parse_response(&resp),
                Err(Attempt::Fatal(err)) => return Err(err),
                Err(Attempt::Retry { err, retry_after }) => {
                    if attempt >= self.retry_max {
                        return Err(err);
                    }
                    let Some(wait) = backoff(attempt, retry_after) else {
                        return Err(err);
                    };
                    tracing::warn!(%err, attempt, ?wait, "anthropic request failed, retrying");
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn health_check(&self) -> Result<String> {
        let resp = self
            .request(reqwest::Method::GET, &format!("/models/{}", self.model))
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(format!("model {} ok", self.model));
        }
        let body = error_body(status.as_u16(), &resp.text().await.unwrap_or_default());
        let hint = if status == StatusCode::NOT_FOUND && self.preset.is_some() {
            " (preset のエンドポイントでは models 確認が使えない可能性があります)"
        } else {
            ""
        };
        Err(Error::Api {
            status: status.as_u16(),
            body: format!("{body}{hint}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff(0, None), Some(Duration::from_secs(1)));
        assert_eq!(backoff(2, None), Some(Duration::from_secs(4)));
        assert_eq!(backoff(10, None), Some(Duration::from_secs(30)));
        assert_eq!(
            backoff(0, Some(Duration::from_secs(7))),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            backoff(0, Some(Duration::from_secs(30))),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn retry_after_over_cap_gives_up() {
        assert_eq!(backoff(0, Some(Duration::from_secs(300))), None);
    }

    #[test]
    fn error_body_formats() {
        assert_eq!(error_body(401, "401: Unauthorized"), "Unauthorized");
        assert_eq!(
            error_body(
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad\nmodel"}}"#
            ),
            "bad model"
        );
        assert_eq!(error_body(500, &"x".repeat(300)).chars().count(), 203);
    }

    #[test]
    fn retryable_statuses() {
        for code in [429u16, 500, 503, 529] {
            assert!(is_retryable(StatusCode::from_u16(code).unwrap()), "{code}");
        }
        for code in [400u16, 401, 404] {
            assert!(!is_retryable(StatusCode::from_u16(code).unwrap()), "{code}");
        }
    }
}
