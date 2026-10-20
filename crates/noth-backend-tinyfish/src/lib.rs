//! TinyFish の HTTP クライアント。tool-web-* の3 module が共有する(modules.md §4)
use noth_core::{Error, Result};

mod client;
mod errors;
pub mod meter;
mod models;
#[cfg(feature = "testing")]
pub mod testing;
mod wallet;

pub use client::{Endpoints, TinyFish};
pub use models::{
    AgentRun, FetchError, FetchOptions, FetchResponse, FetchResult, Metered, RateLimit, RunError,
    RunStatus, SearchOptions, SearchResponse, SearchResult,
};
pub use wallet::{Wallet, WalletMeter, WalletRates};

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

#[cfg(test)]
mod tests;
