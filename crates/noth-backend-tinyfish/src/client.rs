use std::time::Duration;

use noth_core::config::Config;
use noth_core::{Error, Result};
use reqwest::header::HeaderValue;
use reqwest::{Method, Url, redirect};
use serde_json::{Value, json};

use crate::errors::{
    error_message, parse_url, quota_exhausted, retry_after, retry_delay, truncate,
};
use crate::models::{
    AgentRun, FetchOptions, FetchResponse, Metered, RateLimit, SearchOptions, SearchResponse,
};
use crate::wallet::Wallet;

const SEARCH_URL: &str = "https://api.search.tinyfish.ai";
pub(crate) const FETCH_URL: &str = "https://api.fetch.tinyfish.ai";
const AGENT_URL: &str = "https://agent.tinyfish.ai";
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// health_check 用の固定クエリ。内容に意味はない
const HEALTH_QUERY: &str = "noth";

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
    pub(crate) api_key: HeaderValue,
    pub(crate) endpoints: Endpoints,
}

impl std::fmt::Debug for TinyFish {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TinyFish")
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
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
