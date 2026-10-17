use serde::Deserialize;
use serde_json::Value;

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
    pub(crate) fn from_headers(h: &reqwest::header::HeaderMap) -> Option<Self> {
        let get = |name: &str| h.get(name)?.to_str().ok()?.trim().parse::<i64>().ok();
        Some(Self {
            limit: u64::try_from(get("x-ratelimit-limit")?).ok()?,
            remaining: u64::try_from(get("x-ratelimit-remaining")?).ok()?,
            reset_at: get("x-ratelimit-reset"),
        })
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
