use std::sync::Arc;
use std::time::Duration;

use noth_core::traits::Router;
use noth_core::{Error, Result};

use crate::SystemOne;

const JEV_URL: &str = "https://api.typesafe.ai/v1/systemone";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Preset {
    Jev,
    Clef,
    Custom,
}

impl Preset {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Preset::Jev => "jev",
            Preset::Clef => "clef",
            Preset::Custom => "custom",
        }
    }
}

pub(crate) fn from_section(s: &toml::Value) -> Result<Arc<dyn Router>> {
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
