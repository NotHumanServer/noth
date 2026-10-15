use noth_core::{Error, Result};

#[derive(Clone, PartialEq)]
pub(crate) struct DiscordConfig {
    pub(crate) token: String,
    pub(crate) guild_id: u64,
    pub(crate) allowed_users: Vec<u64>,
}

// token がログやパニック出力へ漏れないようにする
impl std::fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("token", &"<redacted>")
            .field("guild_id", &self.guild_id)
            .field("allowed_users", &self.allowed_users)
            .finish()
    }
}

impl DiscordConfig {
    pub(crate) fn parse(section: &toml::Value) -> Result<Self> {
        let token = section
            .get("token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| Error::Config("channel.discord.token が必要です".into()))?
            .to_string();
        let guild_id = parse_id(
            section
                .get("guild_id")
                .ok_or_else(|| Error::Config("channel.discord.guild_id が必要です".into()))?,
            "channel.discord.guild_id",
        )?;
        let allowed_users = match section.get("allowed_users") {
            None => Vec::new(),
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
        Ok(Self {
            token,
            guild_id,
            allowed_users,
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_token() {
        let c = DiscordConfig {
            token: "secret".into(),
            guild_id: 1,
            allowed_users: vec![],
        };
        assert!(!format!("{c:?}").contains("secret"));
    }

    fn section(s: &str) -> toml::Value {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn config_ok() {
        let c = DiscordConfig::parse(&section(
            r#"token = "t"
guild_id = "123"
allowed_users = ["1", "2"]"#,
        ))
        .unwrap();
        assert_eq!(
            c,
            DiscordConfig {
                token: "t".into(),
                guild_id: 123,
                allowed_users: vec![1, 2]
            }
        );
    }

    #[test]
    fn config_errors() {
        for bad in [
            r#"guild_id = "1""#,
            r#"token = "" 
guild_id = "1""#,
            r#"token = "t""#,
            r#"token = "t"
guild_id = 1"#,
            r#"token = "t"
guild_id = "x""#,
            r#"token = "t"
guild_id = "1"
allowed_users = ["1", 2]"#,
            r#"token = "t"
guild_id = "1"
allowed_users = "1""#,
        ] {
            assert!(
                matches!(DiscordConfig::parse(&section(bad)), Err(Error::Config(_))),
                "{bad}"
            );
        }
    }
}
