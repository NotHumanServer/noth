//! Gemini の generateContent の provider。リクエスト構築・応答解析は `wire` の純粋関数に分けてある
mod wire;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gcp_auth::{CustomServiceAccount, TokenProvider};
use noth_core::registry::{ModuleCtx, Registry};
use noth_core::traits::{Completion, LayeredPrompt, Provider};
use noth_core::types::CompletionParams;
use noth_core::{Error, Result};
use reqwest::StatusCode;
use serde::Deserialize;

const AI_STUDIO_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
// 未検証: OpenCode Go が Google 形式をどのパスで受けるかは資料に無い。AI Studio と同じ構造を仮定している
const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai/inference/go/v1";
const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const BACKOFF_CAP: Duration = Duration::from_secs(30);

pub fn register(reg: &mut Registry) {
    reg.register_provider("provider-google", factory);
}

#[derive(Deserialize)]
struct Settings {
    model: String,
    api_key: Option<String>,
    preset: Option<String>,
    project: Option<String>,
    #[serde(default = "default_location")]
    location: String,
    credentials_file: Option<String>,
    thinking_level: Option<String>,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
}

fn default_location() -> String {
    "global".into()
}

fn default_timeout_secs() -> u64 {
    120
}

/// 接続先と認証に要る設定。ファイルの読み込みは含めず、検証と URL 組み立てを純粋に保つ
#[derive(PartialEq, Eq)]
enum Target {
    ApiKey {
        base_url: &'static str,
        api_key: String,
    },
    AgentPlatform {
        project: String,
        location: String,
        credentials_file: String,
    },
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey { base_url, .. } => f
                .debug_struct("ApiKey")
                .field("base_url", base_url)
                .field("api_key", &"***")
                .finish(),
            Self::AgentPlatform {
                project,
                location,
                credentials_file,
            } => f
                .debug_struct("AgentPlatform")
                .field("project", project)
                .field("location", location)
                .field("credentials_file", credentials_file)
                .finish(),
        }
    }
}

/// URL に埋め込む値は検証する。`@` や `/` を含む値で認証付きリクエストが別ホストへ向くのを防ぐ
fn validate_part(name: &str, v: &str, allowed: impl Fn(char) -> bool) -> Result<()> {
    if v.is_empty() || !v.chars().all(allowed) {
        return Err(Error::Config(format!(
            "provider.google: {name} に使えない文字が含まれています(`{v}`)"
        )));
    }
    Ok(())
}

fn normalize_model(model: &str) -> Result<String> {
    let model = model.strip_prefix("models/").unwrap_or(model);
    validate_part("model", model, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
    })?;
    Ok(model.to_string())
}

impl Target {
    fn from_settings(s: &Settings) -> Result<Self> {
        let require = |name: &str, v: &Option<String>| match v.as_deref() {
            Some(v) if !v.is_empty() => Ok(v.to_string()),
            _ => Err(Error::Config(format!("provider.google: {name} は必須です"))),
        };
        match s.preset.as_deref() {
            None => Ok(Self::ApiKey {
                base_url: AI_STUDIO_BASE_URL,
                api_key: require("api_key", &s.api_key)?,
            }),
            Some("opencode-go") => Ok(Self::ApiKey {
                base_url: OPENCODE_GO_BASE_URL,
                api_key: require("api_key", &s.api_key)?,
            }),
            Some("agent-platform") => {
                let project = require("project", &s.project)?;
                let id_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
                validate_part("project", &project, id_char)?;
                validate_part("location", &s.location, id_char)?;
                Ok(Self::AgentPlatform {
                    project,
                    location: s.location.clone(),
                    credentials_file: require("credentials_file", &s.credentials_file)?,
                })
            }
            Some(other) => Err(Error::Config(format!(
                "provider.google: 未知の preset `{other}`"
            ))),
        }
    }

    fn model_url(&self, model: &str) -> String {
        match self {
            Self::ApiKey { base_url, .. } => format!("{base_url}/models/{model}"),
            Self::AgentPlatform {
                project, location, ..
            } => {
                let host = if location == "global" {
                    "aiplatform.googleapis.com".to_string()
                } else {
                    format!("{location}-aiplatform.googleapis.com")
                };
                format!(
                    "https://{host}/v1/projects/{project}/locations/{location}/publishers/google/models/{model}"
                )
            }
        }
    }
}

enum Auth {
    ApiKey(String),
    ServiceAccount(Box<CustomServiceAccount>),
}

fn factory(ctx: &ModuleCtx) -> Result<Arc<dyn Provider>> {
    let s: Settings = ctx
        .section
        .clone()
        .try_into()
        .map_err(|e| Error::Config(format!("provider.google: {e}")))?;
    let thinking_level = s
        .thinking_level
        .as_deref()
        .map(wire::parse_thinking_level)
        .transpose()?;
    let model = normalize_model(&s.model)?;
    let target = Target::from_settings(&s)?;
    let auth = match &target {
        Target::ApiKey { api_key, .. } => Auth::ApiKey(api_key.clone()),
        // gcp_auth::provider() は GOOGLE_APPLICATION_CREDENTIALS を環境変数から読む。
        // Noth は NOTH_HOME 以外の環境変数を読まない方針なので、設定のパスから直接読み込む。
        // 起動時に読んでおくと、パスや JSON の誤りが最初のリクエストでなく起動時に分かる
        Target::AgentPlatform {
            credentials_file, ..
        } => Auth::ServiceAccount(Box::new(
            CustomServiceAccount::from_file(Path::new(credentials_file)).map_err(|e| {
                Error::Config(format!(
                    "provider.google: credentials_file を読み込めません: {e}"
                ))
            })?,
        )),
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(s.timeout_secs))
        .build()
        .map_err(|e| {
            Error::Config(format!(
                "provider.google: HTTP クライアントを作れません: {e}"
            ))
        })?;
    Ok(Arc::new(GoogleProvider {
        client,
        target,
        auth,
        model,
        thinking_level,
        retry_max: ctx.config.context.retry_max,
        preset: s.preset,
    }))
}

struct GoogleProvider {
    client: reqwest::Client,
    target: Target,
    auth: Auth,
    model: String,
    thinking_level: Option<&'static str>,
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

impl GoogleProvider {
    async fn request(
        &self,
        method: reqwest::Method,
        url: String,
    ) -> Result<reqwest::RequestBuilder> {
        let req = self.client.request(method, url);
        Ok(match &self.auth {
            Auth::ApiKey(key) => req.header("x-goog-api-key", key),
            // トークンの再利用と更新は gcp_auth 側が持つので、リクエストごとに取る
            Auth::ServiceAccount(sa) => {
                let token = sa.token(&[CLOUD_PLATFORM_SCOPE]).await.map_err(|e| {
                    Error::Transport(format!("Google の認証トークンを取れません: {e}"))
                })?;
                req.bearer_auth(token.as_str())
            }
        })
    }

    async fn post_once(
        &self,
        body: &serde_json::Value,
    ) -> std::result::Result<serde_json::Value, Attempt> {
        let url = format!("{}:generateContent", self.target.model_url(&self.model));
        let req = self
            .request(reqwest::Method::POST, url)
            .await
            // request が失敗するのはトークン取得だけで、通信断なら再試行で直る
            .map_err(|err| Attempt::Retry {
                err,
                retry_after: None,
            })?;
        let resp = req.json(body).send().await.map_err(|e| {
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
        let header_retry_after = resp
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
        let raw = resp.text().await.unwrap_or_default();
        let retry_after = header_retry_after.or_else(|| wire::parse_error(&raw).retry_delay());
        let err = Error::Api {
            status: status.as_u16(),
            body: wire::format_error(status.as_u16(), &raw),
        };
        if is_retryable(status, &raw) {
            Err(Attempt::Retry { err, retry_after })
        } else {
            Err(Attempt::Fatal(err))
        }
    }
}

/// 429 のうち上限到達(クォータ枯渇)は待っても回復しないため再試行しない。
/// RESOURCE_EXHAUSTED は短期のレート制限も同じ名前で返すので、枯渇と判定できないものだけ再試行する
fn is_retryable(status: StatusCode, raw: &str) -> bool {
    match status.as_u16() {
        429 => !wire::parse_error(raw).is_quota_exhausted(),
        408 | 500 | 502 | 503 | 504 => true,
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

#[async_trait]
impl Provider for GoogleProvider {
    async fn complete(
        &self,
        prompt: &LayeredPrompt,
        params: &CompletionParams,
    ) -> Result<Completion> {
        let body = wire::build_request(prompt, params, self.thinking_level)?;
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
                    tracing::warn!(%err, attempt, ?wait, "google request failed, retrying");
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn health_check(&self) -> Result<String> {
        // agent-platform には課金なしで叩ける軽い GET が無いため、トークン取得だけで認証を確かめる
        if let Target::AgentPlatform {
            project, location, ..
        } = &self.target
        {
            let _ = self.request(reqwest::Method::GET, String::new()).await?;
            return Ok(format!("project {project} / location {location} 認証 ok"));
        }
        let req = self
            .request(reqwest::Method::GET, self.target.model_url(&self.model))
            .await?;
        let resp = req
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(format!("model {} ok", self.model));
        }
        let body = wire::format_error(status.as_u16(), &resp.text().await.unwrap_or_default());
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

    fn settings(toml_src: &str) -> Settings {
        toml::from_str(toml_src).unwrap()
    }

    fn target(toml_src: &str) -> Result<Target> {
        Target::from_settings(&settings(toml_src))
    }

    #[test]
    fn ai_studio_url_and_requirements() {
        let t = target("model = \"gemini-x\"\napi_key = \"k\"").unwrap();
        assert_eq!(
            t.model_url("gemini-x"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-x"
        );
        assert!(target("model = \"m\"").is_err());
        assert!(target("model = \"m\"\napi_key = \"\"").is_err());
    }

    #[test]
    fn opencode_go_url() {
        let t = target("model = \"m\"\napi_key = \"k\"\npreset = \"opencode-go\"").unwrap();
        assert_eq!(
            t.model_url("m"),
            "https://opencode.ai/inference/go/v1/models/m"
        );
        assert!(target("model = \"m\"\npreset = \"opencode-go\"").is_err());
    }

    #[test]
    fn agent_platform_global_and_regional_urls() {
        let base = "model = \"m\"\npreset = \"agent-platform\"\nproject = \"p\"\ncredentials_file = \"/c.json\"\n";
        assert_eq!(
            target(base).unwrap().model_url("m"),
            "https://aiplatform.googleapis.com/v1/projects/p/locations/global/publishers/google/models/m"
        );
        let regional = format!("{base}location = \"us-central1\"");
        assert_eq!(
            target(&regional).unwrap().model_url("m"),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google/models/m"
        );
    }

    #[test]
    fn agent_platform_requires_project_and_credentials() {
        assert!(
            target("model = \"m\"\npreset = \"agent-platform\"\ncredentials_file = \"/c\"")
                .is_err()
        );
        assert!(target("model = \"m\"\npreset = \"agent-platform\"\nproject = \"p\"").is_err());
    }

    #[test]
    fn unknown_preset_is_rejected() {
        assert!(target("model = \"m\"\napi_key = \"k\"\npreset = \"x\"").is_err());
    }

    #[test]
    fn model_is_required() {
        assert!(toml::from_str::<Settings>("api_key = \"k\"").is_err());
    }

    #[test]
    fn url_parts_are_validated() {
        let ap = |extra: &str| {
            target(&format!(
                "model = \"m\"\npreset = \"agent-platform\"\ncredentials_file = \"/c\"\n{extra}"
            ))
        };
        assert!(ap("project = \"p\"\nlocation = \"x@evil.com/\"").is_err());
        assert!(ap("project = \"p/../q\"").is_err());
        assert!(ap("project = \"P\"").is_err());
        assert!(ap("project = \"p-1\"\nlocation = \"us-central1\"").is_ok());
    }

    #[test]
    fn model_is_normalized_and_validated() {
        assert_eq!(
            normalize_model("models/gemini-3.1-pro").unwrap(),
            "gemini-3.1-pro"
        );
        assert_eq!(normalize_model("gemini_x").unwrap(), "gemini_x");
        assert!(normalize_model("a/b").is_err());
        assert!(normalize_model("a@b").is_err());
        assert!(normalize_model("a:generateContent").is_err());
        assert!(normalize_model("").is_err());
    }

    #[test]
    fn debug_masks_api_key() {
        let t = target("model = \"m\"\napi_key = \"secret-key\"").unwrap();
        let s = format!("{t:?}");
        assert!(!s.contains("secret-key"));
        assert!(s.contains("***"));
    }

    #[test]
    fn retryable_statuses() {
        let r = |code: u16, raw: &str| is_retryable(StatusCode::from_u16(code).unwrap(), raw);
        for code in [408u16, 429, 500, 502, 503, 504] {
            assert!(r(code, ""), "{code}");
        }
        for code in [400u16, 401, 402, 403, 404] {
            assert!(!r(code, ""), "{code}");
        }
        assert!(r(
            429,
            r#"{"error":{"code":429,"message":"m","status":"RESOURCE_EXHAUSTED"}}"#
        ));
        assert!(!r(
            429,
            r#"{"error":{"code":"quota_exceeded","message":"m"}}"#
        ));
        assert!(!r(
            429,
            r#"{"error":{"code":429,"message":"m","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.QuotaFailure"}]}}"#
        ));
        assert!(!r(
            429,
            r#"{"error":{"code":429,"message":"You exceeded your current quota","status":"RESOURCE_EXHAUSTED"}}"#
        ));
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff(0, None), Some(Duration::from_secs(1)));
        assert_eq!(backoff(10, None), Some(Duration::from_secs(30)));
        assert_eq!(backoff(0, Some(Duration::from_secs(300))), None);
    }
}
