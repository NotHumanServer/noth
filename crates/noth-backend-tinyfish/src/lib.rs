//! TinyFish の HTTP クライアント。tool-web-* の3 module が共有する(modules.md §4)
use std::time::Duration;

use noth_core::config::Config;
use noth_core::{Error, Result};
use reqwest::header::HeaderValue;
use reqwest::{Method, Url, redirect};
use serde::Deserialize;
use serde_json::{Value, json};

pub mod meter;
#[cfg(feature = "testing")]
pub mod testing;

const SEARCH_URL: &str = "https://api.search.tinyfish.ai";
const FETCH_URL: &str = "https://api.fetch.tinyfish.ai";
const AGENT_URL: &str = "https://agent.tinyfish.ai";
const DEFAULT_TIMEOUT_SECS: u64 = 60;
const ERROR_BODY_LIMIT: usize = 200;
const MAX_RETRIES: u32 = 2;
/// これより長い待ちを求められたら、worker の timeout を食い潰すので待たずに失敗させる
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);
/// health_check 用の固定クエリ。内容に意味はない
const HEALTH_QUERY: &str = "noth";

/// tool の `[tool.*] backend` を検査する。未指定は tinyfish
pub fn require_backend(section: &toml::Value, tool: &str) -> Result<()> {
    match section.get("backend").map(toml::Value::as_str) {
        None | Some(Some("tinyfish")) => Ok(()),
        Some(Some(other)) => Err(Error::Config(format!(
            "[tool.{tool}] backend `{other}` は未対応です(tinyfish のみ)"
        ))),
        Some(None) => Err(Error::Config(format!(
            "[tool.{tool}] backend は文字列で指定してください"
        ))),
    }
}

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub search: String,
    pub fetch: String,
    pub agent: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            search: SEARCH_URL.into(),
            fetch: FETCH_URL.into(),
            agent: AGENT_URL.into(),
        }
    }
}

#[derive(Clone)]
pub struct TinyFish {
    client: reqwest::Client,
    api_key: HeaderValue,
    endpoints: Endpoints,
}

impl std::fmt::Debug for TinyFish {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TinyFish")
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

/// 応答本文と、同じ応答のヘッダから読んだレート制限
#[derive(Debug, Clone)]
pub struct Metered<T> {
    pub body: T,
    pub rate_limit: Option<RateLimit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimit {
    pub limit: u64,
    pub remaining: u64,
    /// UNIX 秒
    pub reset_at: Option<i64>,
}

impl RateLimit {
    fn from_headers(h: &reqwest::header::HeaderMap) -> Option<Self> {
        let get = |name: &str| h.get(name)?.to_str().ok()?.trim().parse::<i64>().ok();
        Some(Self {
            limit: u64::try_from(get("x-ratelimit-limit")?).ok()?,
            remaining: u64::try_from(get("x-ratelimit-remaining")?).ok()?,
            reset_at: get("x-ratelimit-reset"),
        })
    }
}

/// 金額は文字列で返ってくる
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Wallet {
    #[serde(default)]
    pub available_balance: String,
    #[serde(default)]
    pub currency: String,
    #[serde(default)]
    pub as_of: Option<String>,
    #[serde(default)]
    pub rates: WalletRates,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WalletRates {
    #[serde(default)]
    pub meters: Vec<WalletMeter>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WalletMeter {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub unit_amount: String,
    #[serde(default)]
    pub per: String,
}

impl Wallet {
    /// Agent の1ステップあたり USD。見つからない・読めないなら None
    pub fn agent_step_usd(&self) -> Option<f64> {
        self.rates
            .meters
            .iter()
            .find(|m| m.per == "step" && m.label.contains("Agent"))?
            .unit_amount
            .trim()
            .parse()
            .ok()
    }

    fn status_line(&self) -> String {
        let amount = if self.currency.is_empty() || self.currency == "USD" {
            format!("${}", self.available_balance)
        } else {
            format!("{} {}", self.available_balance, self.currency)
        };
        let at = self.as_of.as_deref().map(|raw| {
            let local = raw.parse::<jiff::Timestamp>().ok().map(|t| {
                t.to_zoned(jiff::tz::TimeZone::system())
                    .strftime("%Y-%m-%d %H:%M")
                    .to_string()
            });
            format!("({} 時点)", local.as_deref().unwrap_or(raw))
        });
        format!("TinyFish 残高 {amount}{}", at.unwrap_or_default())
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub domain_type: Option<String>,
    pub recency_minutes: Option<u32>,
    pub include_domains: Vec<String>,
    pub page: Option<u32>,
    pub purpose: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    pub purpose: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchResponse {
    #[serde(default)]
    pub results: Vec<SearchResult>,
    #[serde(default)]
    pub total_results: Option<u64>,
    #[serde(default)]
    pub page: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchResult {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub publisher: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FetchResponse {
    #[serde(default)]
    pub results: Vec<FetchResult>,
    #[serde(default)]
    pub errors: Vec<FetchError>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FetchResult {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub final_url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// 本文が取れなかったページは null で返る
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FetchError {
    #[serde(default)]
    pub url: String,
    #[serde(default, alias = "code")]
    pub error: String,
    #[serde(default)]
    pub status: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    /// 仕様に無い値。ポーリングを続けず打ち切るため終端扱いにはしない
    Unknown,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentRun {
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub num_of_steps: Option<u32>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RunError>,
}

impl AgentRun {
    pub fn run_status(&self) -> RunStatus {
        match self.status.as_str() {
            "PENDING" => RunStatus::Pending,
            "RUNNING" => RunStatus::Running,
            "COMPLETED" => RunStatus::Completed,
            "FAILED" => RunStatus::Failed,
            "CANCELLED" => RunStatus::Cancelled,
            _ => RunStatus::Unknown,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RunError {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

impl TinyFish {
    /// `[backend.tinyfish]` から作る。`search_url` / `fetch_url` / `agent_url` はテストの差し替え用
    pub fn from_config(config: &Config) -> Result<Self> {
        let section = config.backend_section("tinyfish");
        let api_key = section
            .get("api_key")
            .and_then(toml::Value::as_str)
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| Error::Config("[backend.tinyfish] api_key が必要です".into()))?
            .to_string();
        let timeout = match section.get("timeout_secs") {
            None => DEFAULT_TIMEOUT_SECS,
            Some(v) => v.as_integer().filter(|n| *n > 0).ok_or_else(|| {
                Error::Config("[backend.tinyfish] timeout_secs は正の整数で指定してください".into())
            })? as u64,
        };
        let url = |key: &str, default: &str| {
            section
                .get(key)
                .and_then(toml::Value::as_str)
                .map_or_else(|| default.to_string(), |s| s.trim_end_matches('/').into())
        };
        let endpoints = Endpoints {
            search: url("search_url", SEARCH_URL),
            fetch: url("fetch_url", FETCH_URL),
            agent: url("agent_url", AGENT_URL),
        };
        Self::new(api_key, Duration::from_secs(timeout), endpoints)
    }

    pub fn new(api_key: String, timeout: Duration, endpoints: Endpoints) -> Result<Self> {
        let mut api_key = HeaderValue::from_str(&api_key).map_err(|_| {
            Error::Config(
                "[backend.tinyfish] api_key にヘッダに使えない文字が含まれています".into(),
            )
        })?;
        // ログやデバッグ出力にキーが出ないようにする
        api_key.set_sensitive(true);
        // リダイレクト先に X-API-Key を送らないため追従しない
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(redirect::Policy::none())
            .build()
            .map_err(|e| Error::Config(format!("tinyfish: HTTP クライアントを作れません: {e}")))?;
        Ok(Self {
            client,
            api_key,
            endpoints,
        })
    }

    pub async fn search(&self, query: &str, opts: &SearchOptions) -> Result<SearchResponse> {
        Ok(self.search_metered(query, opts).await?.body)
    }

    pub async fn search_metered(
        &self,
        query: &str,
        opts: &SearchOptions,
    ) -> Result<Metered<SearchResponse>> {
        let mut url = parse_url(&self.endpoints.search)?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("query", query);
            if let Some(v) = &opts.domain_type {
                q.append_pair("domain_type", v);
            }
            if let Some(v) = opts.recency_minutes {
                q.append_pair("recency_minutes", &v.to_string());
            }
            if !opts.include_domains.is_empty() {
                q.append_pair("include_domains", &opts.include_domains.join(","));
            }
            if let Some(v) = opts.page {
                q.append_pair("page", &v.to_string());
            }
            if let Some(v) = &opts.purpose {
                q.append_pair("purpose", v);
            }
        }
        self.request_metered(Method::GET, url, None, true).await
    }

    pub async fn fetch(&self, urls: &[String], opts: &FetchOptions) -> Result<FetchResponse> {
        Ok(self.fetch_metered(urls, opts).await?.body)
    }

    pub async fn fetch_metered(
        &self,
        urls: &[String],
        opts: &FetchOptions,
    ) -> Result<Metered<FetchResponse>> {
        let mut body = json!({ "urls": urls, "format": "markdown" });
        if let Some(p) = &opts.purpose {
            body["purpose"] = json!(p);
        }
        let url = parse_url(&self.endpoints.fetch)?;
        self.request_metered(Method::POST, url, Some(body), true)
            .await
    }

    /// 戻り値は run_id
    pub async fn agent_run_async(
        &self,
        url: &str,
        goal: &str,
        max_steps: Option<u32>,
        max_duration_secs: u64,
    ) -> Result<String> {
        let mut config = json!({ "max_duration_seconds": max_duration_secs });
        if let Some(n) = max_steps {
            config["max_steps"] = json!(n);
        }
        let body = json!({ "url": url, "goal": goal, "agent_config": config });
        let endpoint = parse_url(&format!("{}/v1/automation/run-async", self.endpoints.agent))?;
        let resp: Value = self
            .request(Method::POST, endpoint, Some(body), false)
            .await?;
        match resp.get("run_id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => Ok(id.to_string()),
            _ => Err(Error::Other(format!(
                "TinyFish が run_id を返しませんでした: {}",
                truncate(&resp.to_string())
            ))),
        }
    }

    pub async fn agent_run(&self, id: &str) -> Result<AgentRun> {
        let url = parse_url(&format!("{}/v1/runs/{id}", self.endpoints.agent))?;
        self.request(Method::GET, url, None, true).await
    }

    pub async fn agent_cancel(&self, id: &str) -> Result<()> {
        let url = parse_url(&format!("{}/v1/runs/{id}/cancel", self.endpoints.agent))?;
        let _: Value = self.request(Method::POST, url, None, true).await?;
        Ok(())
    }

    /// 無料の Search を1回だけ呼ぶ。Fetch / Agent は呼ぶと課金や枠消費があるので疎通確認に使わない
    pub async fn health_check(&self) -> Result<String> {
        self.search(HEALTH_QUERY, &SearchOptions::default()).await?;
        Ok("TinyFish: ok".into())
    }

    pub async fn wallet(&self) -> Result<Wallet> {
        let url = parse_url(&format!("{}/v1/wallet", self.endpoints.agent))?;
        self.request(Method::GET, url, None, true).await
    }

    /// `noth cost` 用の1行。残高が取れなければ Err
    pub async fn account_status(&self) -> Result<String> {
        Ok(self.wallet().await?.status_line())
    }

    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        retry: bool,
    ) -> Result<T> {
        Ok(self.request_metered(method, url, body, retry).await?.body)
    }

    async fn request_metered<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        retry: bool,
    ) -> Result<Metered<T>> {
        let mut attempt = 0;
        loop {
            let mut req = self
                .client
                .request(method.clone(), url.clone())
                .header("X-API-Key", self.api_key.clone());
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| Error::Transport(e.without_url().to_string()))?;
            let status = resp.status();
            let wait = retry_after(&resp);
            let rate_limit = RateLimit::from_headers(resp.headers());
            let text = resp
                .text()
                .await
                .map_err(|e| Error::Transport(e.without_url().to_string()))?;
            if status.is_success() {
                let body = serde_json::from_str(&text)
                    .map_err(|e| Error::Other(format!("TinyFish の応答を解釈できません: {e}")))?;
                return Ok(Metered { body, rate_limit });
            }
            let code = status.as_u16();
            // 非冪等な呼び出し(run-async)を再送すると run が二重に作られて課金される
            let retryable = retry && !(wait.is_none() && quota_exhausted(&text));
            if let Some(delay) = retryable
                .then(|| retry_delay(code, wait, attempt))
                .flatten()
            {
                tracing::warn!(
                    status = code,
                    attempt,
                    ?delay,
                    "TinyFish が失敗したため再試行します"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }
            return Err(Error::Api {
                status: code,
                body: error_message(&text),
            });
        }
    }
}

/// 日次枠や無料枠の枯渇は待っても回復しない(Retry-After も付かない)ので再試行しない
fn quota_exhausted(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/code")?.as_str().map(str::to_string))
        .is_some_and(|c| {
            matches!(
                c.as_str(),
                "DAILY_LIMIT_EXCEEDED" | "FREE_ALLOWANCE_EXHAUSTED"
            )
        })
}

fn parse_url(s: &str) -> Result<Url> {
    Url::parse(s).map_err(|e| Error::Config(format!("tinyfish: URL `{s}` が不正です: {e}")))
}

/// `{error:{code,message}}` を1行にする。形が違えば本文をそのまま切り詰める
fn error_message(body: &str) -> String {
    let parsed = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        let e = v.get("error")?;
        let code = e.get("code").and_then(Value::as_str).unwrap_or("UNKNOWN");
        let message = e.get("message").and_then(Value::as_str).unwrap_or("");
        Some(format!("TinyFish エラー {code}: {message}"))
    });
    truncate(&parsed.unwrap_or_else(|| format!("TinyFish エラー: {body}")))
}

fn truncate(s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(ERROR_BODY_LIMIT) {
        Some((i, _)) => format!("{}...", &one_line[..i]),
        None => one_line,
    }
}

/// 再試行するなら待ち時間を返す。400 などクライアント側の誤りは繰り返しても枠を消費するだけなので対象外
fn retry_delay(status: u16, retry_after: Option<Duration>, attempt: u32) -> Option<Duration> {
    if attempt >= MAX_RETRIES || !matches!(status, 429 | 502 | 503 | 504) {
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

#[cfg(test)]
mod tests;
