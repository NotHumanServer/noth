//! スラッシュコマンドの登録と interaction への応答。
//! serenity の型は Components V2 の応答(`Message` に読み戻せない)を扱えないので、生の JSON を reqwest で送る
use std::time::Duration;

use noth_core::async_trait;
use serde_json::Value;

const API_BASE: &str = "https://discord.com/api/v10";
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApiError {
    /// 0 は接続・タイムアウトなど応答が得られなかった場合
    pub(crate) status: u16,
    pub(crate) body: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Discord API {}: {}", self.status, self.body)
    }
}

#[async_trait]
pub(crate) trait InteractionApi: Send + Sync {
    /// guild のコマンドを丸ごと置き換える(冪等)
    async fn sync_commands(&self, body: &Value) -> Result<(), ApiError>;
    /// 最初の応答(type 4 / 5 / 7)。受け付けは 3 秒以内
    async fn callback(
        &self,
        interaction_id: u64,
        token: &str,
        body: &Value,
    ) -> Result<(), ApiError>;
    /// 最初の応答を書き換える。defer した応答はここで中身を入れる
    async fn edit_original(&self, token: &str, body: &Value) -> Result<(), ApiError>;
    /// 最初の応答とは別の新しいメッセージを送る。defer 済みの応答を書き換えられないとき(V2 から戻せない)の逃げ道
    async fn followup(&self, token: &str, body: &Value) -> Result<(), ApiError>;
}

pub(crate) struct RestInteractionApi {
    client: reqwest::Client,
    base: String,
    bot_token: String,
    application_id: u64,
    guild_id: u64,
}

impl RestInteractionApi {
    pub(crate) fn new(bot_token: String, application_id: u64, guild_id: u64) -> Self {
        Self::with_base(bot_token, application_id, guild_id, API_BASE.into())
    }

    pub(crate) fn with_base(
        bot_token: String,
        application_id: u64,
        guild_id: u64,
        base: String,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!(
                "DiscordBot (noth, ",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .build()
            .unwrap_or_default();
        Self {
            client,
            base,
            bot_token,
            application_id,
            guild_id,
        }
    }

    async fn send(&self, req: reqwest::RequestBuilder, body: &Value) -> Result<(), ApiError> {
        let res = req.json(body).send().await.map_err(|e| ApiError {
            status: 0,
            body: if e.is_timeout() {
                "タイムアウトしました".into()
            } else {
                e.to_string()
            },
        })?;
        let status = res.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        let text = res.text().await.unwrap_or_default();
        Err(ApiError {
            status,
            body: text.chars().take(300).collect(),
        })
    }
}

#[async_trait]
impl InteractionApi for RestInteractionApi {
    async fn sync_commands(&self, body: &Value) -> Result<(), ApiError> {
        let url = format!(
            "{}/applications/{}/guilds/{}/commands",
            self.base, self.application_id, self.guild_id
        );
        let req = self
            .client
            .put(url)
            .header("Authorization", format!("Bot {}", self.bot_token));
        self.send(req, body).await
    }

    // interaction の token は URL に含まれ、Authorization は要らない
    async fn callback(
        &self,
        interaction_id: u64,
        token: &str,
        body: &Value,
    ) -> Result<(), ApiError> {
        let url = format!(
            "{}/interactions/{interaction_id}/{token}/callback",
            self.base
        );
        self.send(self.client.post(url), body).await
    }

    async fn edit_original(&self, token: &str, body: &Value) -> Result<(), ApiError> {
        let url = format!(
            "{}/webhooks/{}/{token}/messages/@original",
            self.base, self.application_id
        );
        self.send(self.client.patch(url), body).await
    }

    async fn followup(&self, token: &str, body: &Value) -> Result<(), ApiError> {
        let url = format!("{}/webhooks/{}/{token}", self.base, self.application_id);
        self.send(self.client.post(url), body).await
    }
}
