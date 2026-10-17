use noth_core::{Error, Result};

#[derive(Clone, PartialEq)]
pub(crate) struct DiscordConfig {
    pub(crate) token: String,
    pub(crate) guild_id: u64,
    pub(crate) allowed_users: Vec<u64>,
    /// 管理コマンド(`/noth ...`)を使える唯一のユーザー。未設定なら誰も使えない
    pub(crate) master: Option<u64>,
    /// 除外するチャンネルまたはカテゴリの ID。`/noth ignore` で足したものはここに含まれない
    pub(crate) ignored_channels: Vec<u64>,
    /// Discord API のベース URL。未設定なら本物の Discord を使う。末尾の `/` は取り除く
    pub(crate) api_base: Option<String>,
}

// token がログやパニック出力へ漏れないようにする
impl std::fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("token", &"<redacted>")
            .field("guild_id", &self.guild_id)
            .field("allowed_users", &self.allowed_users)
            .field("master", &self.master)
            .field("ignored_channels", &self.ignored_channels)
            .field("api_base", &self.api_base)
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
        let allowed_users = parse_id_list(section, "allowed_users")?;
        let ignored_channels = parse_id_list(section, "ignored_channels")?;
        let master = section
            .get("master")
            .map(|v| parse_id(v, "channel.discord.master"))
            .transpose()?;
        let api_base = noth_core::config::parse_api_base(section)?;
        Ok(Self {
            token,
            guild_id,
            allowed_users,
            master,
            ignored_channels,
            api_base,
        })
    }
}

fn parse_id_list(section: &toml::Value, key: &str) -> Result<Vec<u64>> {
    let full = format!("channel.discord.{key}");
    match section.get(key) {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(items)) => items.iter().map(|v| parse_id(v, &full)).collect(),
        Some(_) => Err(Error::Config(format!(
            "{full} は文字列の配列で指定してください"
        ))),
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
            master: None,
            ignored_channels: vec![],
            api_base: None,
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
                allowed_users: vec![1, 2],
                master: None,
                ignored_channels: vec![],
                api_base: None,
            }
        );
    }

    #[test]
    fn ignored_channels_are_parsed_and_validated() {
        let c = DiscordConfig::parse(&section(
            r#"token = "t"
guild_id = "1"
ignored_channels = ["11", "22"]"#,
        ))
        .unwrap();
        assert_eq!(c.ignored_channels, vec![11, 22]);
        for bad in [
            r#"ignored_channels = "11""#,
            r#"ignored_channels = [11]"#,
            r#"ignored_channels = ["x"]"#,
            r#"ignored_channels = ["0"]"#,
        ] {
            let text = format!("token = \"t\"\nguild_id = \"1\"\n{bad}");
            assert!(
                matches!(DiscordConfig::parse(&section(&text)), Err(Error::Config(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn master_is_optional_and_parsed() {
        let with = DiscordConfig::parse(&section(
            r#"token = "t"
guild_id = "1"
master = "42""#,
        ))
        .unwrap();
        assert_eq!(with.master, Some(42));
        let without = DiscordConfig::parse(&section("token = \"t\"\nguild_id = \"1\"")).unwrap();
        assert_eq!(without.master, None);
    }

    #[test]
    fn api_base_is_optional_and_normalized() {
        let with = DiscordConfig::parse(&section(
            r#"token = "t"
guild_id = "1"
api_base = "http://127.0.0.1:3900/""#,
        ))
        .unwrap();
        assert_eq!(with.api_base, Some("http://127.0.0.1:3900".into()));
        let without = DiscordConfig::parse(&section("token = \"t\"\nguild_id = \"1\"")).unwrap();
        assert_eq!(without.api_base, None);
    }

    #[test]
    fn api_base_is_validated_by_core() {
        let bad = section("token = \"t\"\nguild_id = \"1\"\napi_base = \"https://discord.com\"");
        assert!(matches!(DiscordConfig::parse(&bad), Err(Error::Config(_))));
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
            r#"token = "t"
guild_id = "1"
master = 5"#,
            r#"token = "t"
guild_id = "1"
master = "x""#,
            r#"token = "t"
guild_id = "1"
master = "0""#,
        ] {
            assert!(
                matches!(DiscordConfig::parse(&section(bad)), Err(Error::Config(_))),
                "{bad}"
            );
        }
    }
}
