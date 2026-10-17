use std::collections::HashSet;

use noth_core::{Error, Result};

use crate::manifest;

#[derive(Clone, PartialEq)]
pub(crate) struct Settings {
    pub(crate) token: String,
    pub(crate) guild_id: u64,
    pub(crate) allowed: HashSet<u64>,
    /// manifest の順。空にはならない
    pub(crate) actions: Vec<&'static str>,
    /// channel.discord の api_base。未設定なら本物の Discord を使う
    pub(crate) api_base: Option<String>,
}

// token がログやパニック出力へ漏れないようにする
impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("token", &"<redacted>")
            .field("guild_id", &self.guild_id)
            .field("actions", &self.actions)
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

impl Settings {
    /// `channel` は `[channel.discord]`、`tool` は `[tool.discord]`
    pub(crate) fn parse(channel: &toml::Value, tool: &toml::Value) -> Result<Self> {
        let token = channel
            .get("token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| Error::Config("channel.discord.token が必要です".into()))?
            .to_string();
        let guild_id = parse_id(
            channel
                .get("guild_id")
                .ok_or_else(|| Error::Config("channel.discord.guild_id が必要です".into()))?,
            "channel.discord.guild_id",
        )?;
        let allowed = match channel.get("allowed_users") {
            None => HashSet::new(),
            Some(toml::Value::Array(items)) => items
                .iter()
                .map(|v| parse_id(v, "channel.discord.allowed_users"))
                .collect::<Result<_>>()?,
            Some(_) => {
                return Err(Error::Config(
                    "channel.discord.allowed_users は文字列の配列で指定してください".into(),
                ));
            }
        };
        let api_base = noth_core::config::parse_api_base(channel)?;
        Ok(Self {
            token,
            guild_id,
            allowed,
            actions: parse_actions(tool)?,
            api_base,
        })
    }
}

fn parse_actions(tool: &toml::Value) -> Result<Vec<&'static str>> {
    let requested: Vec<&str> = match tool.get("actions") {
        None => Vec::new(),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str())
            .collect::<Option<_>>()
            .ok_or_else(|| {
                Error::Config("tool.discord.actions は文字列の配列で指定してください".into())
            })?,
        Some(_) => {
            return Err(Error::Config(
                "tool.discord.actions は文字列の配列で指定してください".into(),
            ));
        }
    };
    if let Some(bad) = requested.iter().find(|a| !manifest::is_known(a)) {
        return Err(Error::Config(format!(
            "tool.discord.actions に未知の action があります: `{bad}`(使えるもの: {})",
            manifest::known_names().join(", ")
        )));
    }
    Ok(manifest::known_names()
        .into_iter()
        .filter(|n| requested.is_empty() || requested.contains(n))
        .collect())
}

fn parse_id(value: &toml::Value, key: &str) -> Result<u64> {
    let s = value
        .as_str()
        .ok_or_else(|| Error::Config(format!("{key} は数字の文字列で指定してください")))?;
    s.parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .ok_or_else(|| Error::Config(format!("{key} の ID が不正です: `{s}`")))
}
