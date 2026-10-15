//! OpenAI の Chat Completions / Responses API の provider。リクエスト構築・応答解析は `wire` の純粋関数に分けてある
pub mod codex_auth;
mod pricing;
mod wire;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use noth_core::registry::{ModuleCtx, Registry};
use noth_core::traits::{Completion, LayeredPrompt, Provider};
use noth_core::types::Billing;
use noth_core::types::CompletionParams;
use noth_core::{Error, Result};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai/inference/go/v1";
const BACKOFF_CAP: Duration = Duration::from_secs(30);

pub fn register(reg: &mut Registry) {
    reg.register_provider("provider-openai", factory);
}

#[derive(Deserialize)]
struct Settings {
    /// codex preset では使わない(トークンは codex-auth.json から読む)
    api_key: Option<String>,
    model: String,
    format: Option<String>,
    preset: Option<String>,
    base_url: Option<String>,
    timeout_secs: Option<u64>,
    effort: Option<String>,
    #[serde(default)]
    pricing: pricing::Overrides,
}

const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Codex は SSE を最後まで読み切ってから解析する。長い reasoning の間は何も届かないが全体の
/// タイムアウトは進むので、通常より長くとる
const CODEX_TIMEOUT_SECS: u64 = 300;

fn resolve_base_url(preset: Option<&str>, base_url: Option<&str>) -> Result<String> {
    let preset_url = match preset {
        None => DEFAULT_BASE_URL,
        Some("codex") => CODEX_BASE_URL,
        Some("opencode-go") => OPENCODE_GO_BASE_URL,
        Some(other) => {
            return Err(Error::Config(format!(
                "provider.openai: 未知の preset `{other}`"
            )));
        }
    };
    Ok(base_url
        .unwrap_or(preset_url)
        .trim_end_matches('/')
        .to_string())
}

fn factory(ctx: &ModuleCtx) -> Result<Arc<dyn Provider>> {
    let s: Settings = ctx
        .section
        .clone()
        .try_into()
        .map_err(|e| Error::Config(format!("provider.openai: {e}")))?;
    let codex = s.preset.as_deref() == Some("codex");
    let format = match (codex, s.format.as_deref()) {
        (true, None | Some("responses")) => wire::Format::Responses,
        (true, Some(other)) => {
            return Err(Error::Config(format!(
                "provider.openai: preset \"codex\" は format = \"responses\" 固定です(`{other}` が指定されています)"
            )));
        }
        (false, f) => wire::Format::parse(f.unwrap_or("chat"))?,
    };
    // トークンを任意の URL へ送らないよう、Codex の宛先は固定する
    if codex && s.base_url.is_some() {
        return Err(Error::Config(
            "provider.openai: preset \"codex\" では base_url を指定できません".into(),
        ));
    }
    let effort = s.effort.as_deref().map(wire::parse_effort).transpose()?;
    let base_url = resolve_base_url(s.preset.as_deref(), s.base_url.as_deref())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(s.timeout_secs.unwrap_or(if codex {
            CODEX_TIMEOUT_SECS
        } else {
            DEFAULT_TIMEOUT_SECS
        })))
        .build()
        .map_err(|e| {
            Error::Config(format!(
                "provider.openai: HTTP クライアントを作れません: {e}"
            ))
        })?;
    let auth = if codex {
        let path = ctx.config.paths.codex_auth();
        // 未ログインを実行時の初回リクエストまで持ち越さず、起動時に気づかせる
        codex_auth::load(&path)?;
        Auth::Codex(codex_auth::TokenStore::new(path, client.clone()))
    } else {
        let key = s
            .api_key
            .ok_or_else(|| Error::Config("provider.openai: api_key が設定されていません".into()))?;
        Auth::ApiKey(key)
    };
    Ok(Arc::new(OpenAiProvider {
        client,
        base_url,
        auth,
        pricing: s.pricing,
        model: s.model,
        format,
        effort,
        retry_max: ctx.config.context.retry_max,
        compat: s.preset.is_some() || s.base_url.is_some(),
    }))
}

struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    auth: Auth,
    pricing: pricing::Overrides,
    model: String,
    format: wire::Format,
    effort: Option<&'static str>,
    retry_max: u32,
    /// preset / base_url 指定時。互換サーバー向けの挙動に切り替える
    compat: bool,
}

enum Auth {
    ApiKey(String),
    Codex(codex_auth::TokenStore),
}

enum Attempt {
    Retry {
        err: Error,
        retry_after: Option<Duration>,
    },
    Fatal(Error),
}

impl OpenAiProvider {
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let req = self
            .client
            .request(method, format!("{}{path}", self.base_url));
        Ok(match &self.auth {
            Auth::ApiKey(key) => req.bearer_auth(key),
            Auth::Codex(store) => {
                let token = store.access_token().await?;
                codex_auth::request_headers(&token)
                    .into_iter()
                    .fold(req.bearer_auth(&token), |r, (k, v)| r.header(k, v))
            }
        })
    }

    async fn post_once(
        &self,
        path: &str,
        extra_headers: &[(&str, String)],
        body: &Value,
    ) -> std::result::Result<Vec<u8>, Attempt> {
        let req = self
            .request(reqwest::Method::POST, path)
            .await
            .map_err(|err| match err {
                // トークン更新の一時的な失敗は待てば直る。再ログインが要る失敗(Other)は直らない
                Error::Transport(_) => Attempt::Retry {
                    err,
                    retry_after: None,
                },
                Error::Api { status, .. } if status == 429 || status >= 500 => Attempt::Retry {
                    err,
                    retry_after: None,
                },
                err => Attempt::Fatal(err),
            })?;
        let resp = extra_headers
            .iter()
            .fold(req, |r, (k, v)| r.header(*k, v))
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
            return Ok(bytes.to_vec());
        }
        let raw = resp.text().await.unwrap_or_default();
        let err = Error::Api {
            status: status.as_u16(),
            body: error_body(status.as_u16(), &raw),
        };
        if is_retryable(status, &raw) {
            Err(Attempt::Retry { err, retry_after })
        } else {
            Err(Attempt::Fatal(err))
        }
    }
}

/// 429 のうち残高・クォータ切れは待っても直らないので再試行しない
fn is_retryable(status: StatusCode, raw: &str) -> bool {
    match status.as_u16() {
        429 => {
            let v: Value = serde_json::from_str(raw).unwrap_or_default();
            let hint = format!(
                "{} {} {}",
                v["error"]["code"].as_str().unwrap_or_default(),
                v["error"]["type"].as_str().unwrap_or_default(),
                v["error"]["message"].as_str().unwrap_or_default()
            )
            .to_lowercase();
            // Codex のサブスクリプション上限は usage_limit_reached で返り、待っても直らない
            !["quota", "usage_limit", "billing", "credit"]
                .iter()
                .any(|w| hint.contains(w))
        }
        500 | 502 | 503 | 504 => true,
        _ => false,
    }
}

/// Retry-After が上限より長いなら、早めに再送しても弾かれるだけなので待たずに諦める
fn backoff(attempt: u32, retry_after: Option<Duration>) -> Option<Duration> {
    match retry_after {
        Some(d) if d > BACKOFF_CAP => None,
        Some(d) => Some(d),
        None => Some(Duration::from_secs(1u64 << attempt.min(5)).min(BACKOFF_CAP)),
    }
}

/// API のエラー JSON があれば message に code / type を添え、無ければ生の本文を 1 行に潰して短くする。
/// status は Error::Api の表示と二重になるため含めない
fn error_body(status: u16, raw: &str) -> String {
    const MAX_CHARS: usize = 200;
    let v: Value = serde_json::from_str(raw).unwrap_or_default();
    let err = &v["error"];
    let message = match err["message"].as_str() {
        Some(m) => {
            let tags: Vec<String> = [("code", "コード"), ("type", "種別")]
                .iter()
                .filter_map(|(k, label)| err[*k].as_str().map(|s| format!("{label}: {s}")))
                .collect();
            if tags.is_empty() {
                m.to_string()
            } else {
                format!("{m}({})", tags.join(", "))
            }
        }
        None => raw.to_string(),
    };
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
impl Provider for OpenAiProvider {
    async fn complete(
        &self,
        prompt: &LayeredPrompt,
        params: &CompletionParams,
    ) -> Result<Completion> {
        let codex = matches!(self.auth, Auth::Codex(_));
        let (path, body) = match self.format {
            wire::Format::Chat => (
                "/chat/completions",
                wire::build_chat_request(&self.model, prompt, params, self.effort, self.compat)?,
            ),
            wire::Format::Responses if codex => (
                "/responses",
                wire::build_codex_request(&self.model, prompt, params, self.effort)?,
            ),
            wire::Format::Responses => (
                "/responses",
                wire::build_responses_request(&self.model, prompt, params, self.effort)?,
            ),
        };
        // Codex はキャッシュのルーティングに session_id と x-client-request-id を使う。body の
        // prompt_cache_key と同じ値にそろえ、プロセスをまたいでも同じプレフィックスが同じ値になるようにする
        let extra_headers: Vec<(&str, String)> = if codex {
            let key = wire::codex_cache_key(&body);
            vec![("session_id", key.clone()), ("x-client-request-id", key)]
        } else {
            Vec::new()
        };
        let mut attempt = 0;
        loop {
            match self.post_once(path, &extra_headers, &body).await {
                Ok(bytes) => {
                    let cost_ctx = wire::CostContext {
                        request_model: &self.model,
                        billing: if codex {
                            Billing::Subscription
                        } else {
                            Billing::Metered
                        },
                        // preset / base_url の先は OpenAI 本家と価格が違うので、表の単価を当てない
                        use_table: !self.compat,
                        overrides: &self.pricing,
                    };
                    if codex {
                        return wire::assemble_sse(&String::from_utf8_lossy(&bytes), &cost_ctx);
                    }
                    let resp: Value = serde_json::from_slice(&bytes)?;
                    return match self.format {
                        wire::Format::Chat => wire::parse_chat_response(&resp, &cost_ctx),
                        wire::Format::Responses => wire::parse_responses_response(&resp, &cost_ctx),
                    };
                }
                Err(Attempt::Fatal(err)) => return Err(err),
                Err(Attempt::Retry { err, retry_after }) => {
                    if attempt >= self.retry_max {
                        return Err(err);
                    }
                    let Some(wait) = backoff(attempt, retry_after) else {
                        return Err(err);
                    };
                    tracing::warn!(%err, attempt, ?wait, "openai リクエストに失敗したため再試行します");
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn health_check(&self) -> Result<String> {
        if let Auth::Codex(store) = &self.auth {
            // 推論は課金枠を消費するため叩かず、トークンの読み込みと更新だけ確かめる
            let token = store.access_token().await?;
            let account = codex_auth::account_id(&token)
                .map_or_else(|| "不明".to_string(), |id| codex_auth::mask_account_id(&id));
            return Ok(format!(
                "Codex 認証 ok(アカウント {account}, model {})",
                self.model
            ));
        }
        let resp = self
            .request(reqwest::Method::GET, &format!("/models/{}", self.model))
            .await?
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(format!("model {} ok", self.model));
        }
        let body = error_body(status.as_u16(), &resp.text().await.unwrap_or_default());
        let hint = if status == StatusCode::NOT_FOUND && self.compat {
            " (preset / base_url のエンドポイントでは models 確認が使えない可能性があります)"
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
        assert_eq!(backoff(0, Some(Duration::from_secs(300))), None);
    }

    #[test]
    fn preset_base_url() {
        assert_eq!(
            resolve_base_url(None, None).unwrap(),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            resolve_base_url(Some("opencode-go"), None).unwrap(),
            "https://opencode.ai/inference/go/v1"
        );
        assert_eq!(
            resolve_base_url(Some("opencode-go"), Some("http://localhost:1/v1/")).unwrap(),
            "http://localhost:1/v1"
        );
        assert!(resolve_base_url(Some("nope"), None).is_err());
    }

    #[test]
    fn error_body_formats() {
        assert_eq!(error_body(401, "401: Unauthorized"), "Unauthorized");
        assert_eq!(
            error_body(
                400,
                r#"{"error":{"message":"bad\nmodel","type":"invalid_request_error","code":"model_not_found","param":null}}"#
            ),
            "bad model(コード: model_not_found, 種別: invalid_request_error)"
        );
        assert_eq!(error_body(500, &"x".repeat(300)).chars().count(), 203);
    }

    #[test]
    fn retryable_statuses() {
        let ok = |code: u16, body: &str| is_retryable(StatusCode::from_u16(code).unwrap(), body);
        for code in [500u16, 502, 503, 504] {
            assert!(ok(code, ""), "{code}");
        }
        for code in [400u16, 401, 404, 529] {
            assert!(!ok(code, ""), "{code}");
        }
        assert!(ok(
            429,
            r#"{"error":{"code":"rate_limit_exceeded","type":"requests"}}"#
        ));
        assert!(ok(429, "not json"));
        assert!(!ok(
            429,
            r#"{"error":{"code":"insufficient_quota","type":"insufficient_quota"}}"#
        ));
        assert!(!ok(
            429,
            r#"{"error":{"code":"billing_hard_limit_reached"}}"#
        ));
        assert!(!ok(
            429,
            r#"{"error":{"type":"usage_limit_reached","message":"limit"}}"#
        ));
        assert!(!ok(
            429,
            r#"{"error":{"message":"You exceeded your current Quota"}}"#
        ));
    }
}
