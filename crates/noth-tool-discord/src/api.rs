//! Discord REST の GET だけを包む。テストでは差し替える
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

const API_BASE: &str = "https://discord.com/api/v10";
const TIMEOUT: Duration = Duration::from_secs(15);
/// 異常に大きい応答でメモリを使い切らないための上限
const BODY_MAX_BYTES: usize = 4 * 1024 * 1024;
const RETRY_WAIT_MAX_SECS: f64 = 5.0;

/// status 0 は接続・タイムアウトなど HTTP 応答が得られなかった場合
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApiError {
    pub(crate) status: u16,
    pub(crate) body: String,
    /// 429 のときだけ。Discord が示す待ち秒数
    pub(crate) retry_after: Option<f64>,
}

impl ApiError {
    pub(crate) fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            retry_after: None,
        }
    }
}

#[async_trait]
pub(crate) trait DiscordApi: Send + Sync {
    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError>;
}

pub(crate) struct RestApi {
    client: reqwest::Client,
    base: String,
    token: String,
}

impl RestApi {
    pub(crate) fn new(token: String) -> Result<Self, noth_core::Error> {
        Self::with_base(token, API_BASE.into())
    }

    pub(crate) fn with_base(token: String, base: String) -> Result<Self, noth_core::Error> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!(
                "DiscordBot (noth, ",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .build()
            .map_err(|e| noth_core::Error::Transport(e.to_string()))?;
        Ok(Self {
            client,
            base,
            token,
        })
    }

    async fn get_once(&self, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
        let transport = |e: reqwest::Error| {
            ApiError::new(
                0,
                if e.is_timeout() {
                    "タイムアウトしました".to_string()
                } else {
                    // reqwest のエラーには URL が入るが、トークンは header にしか載せていない
                    e.to_string()
                },
            )
        };
        let mut res = self
            .client
            .get(format!("{}{path}", self.base))
            .header("Authorization", format!("Bot {}", self.token))
            .query(query)
            .send()
            .await
            .map_err(transport)?;
        let status = res.status().as_u16();
        let header_retry = res
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok());
        let too_large = || ApiError::new(status, "応答が大きすぎます");
        if res
            .content_length()
            .is_some_and(|n| n > BODY_MAX_BYTES as u64)
        {
            return Err(too_large());
        }
        // Content-Length の無い応答でも、読み込みの途中で上限を超えたら打ち切る
        let mut bytes = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(transport)? {
            if bytes.len() + chunk.len() > BODY_MAX_BYTES {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        if !(200..300).contains(&status) {
            let mut e = ApiError::new(status, error_message(&bytes));
            if status == 429 {
                e.retry_after = serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .and_then(|v| v.get("retry_after").and_then(Value::as_f64))
                    .or(header_retry);
            }
            return Err(e);
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| ApiError::new(status, format!("応答を解釈できません: {e}")))
    }
}

#[async_trait]
impl DiscordApi for RestApi {
    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
        match self.get_once(path, query).await {
            // Worker を長く止めないよう、短い待ちのときだけ 1 回やり直す
            Err(ApiError {
                status: 429,
                retry_after: Some(secs),
                ..
            }) if secs <= RETRY_WAIT_MAX_SECS => {
                tokio::time::sleep(Duration::from_secs_f64(secs.max(0.0))).await;
                self.get_once(path, query).await
            }
            other => other,
        }
    }
}

fn error_message(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let message = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| text.into_owned());
    message.chars().take(200).collect()
}

const VIEW_HISTORY: &str = "Bot がこのチャンネルを見られません(View Channel または Read Message History の権限が足りません)";

/// 権限はサーバーごとに違い事前に調べられないので、呼び出し時の 403 を action 別の案内にする
pub(crate) fn describe_error(action: &str, e: &ApiError) -> String {
    let hint = match e.status {
        0 => return format!("Discord に接続できません: {}", e.body),
        401 => "Bot トークンが無効です。.env の DISCORD_BOT_TOKEN を確認してください".to_string(),
        403 => match action {
            "fetch_messages" | "list_pins" => VIEW_HISTORY.to_string(),
            "channel_info" => {
                "Bot がこのチャンネルを見られません(View Channel の権限が足りません)".into()
            }
            "member_info" => "Bot がこのメンバーを取得できません(権限が足りません)".into(),
            _ => "Bot の権限が足りません".into(),
        },
        404 => "見つかりません。ID が誤っているか、Bot から見えないチャンネル・メンバーです".into(),
        429 => match e.retry_after {
            Some(secs) => format!(
                "Discord のレート制限に達しました。{} 秒ほど待ってから再試行してください",
                secs.ceil() as u64
            ),
            None => "Discord のレート制限に達しました。少し待ってから再試行してください".into(),
        },
        _ => String::new(),
    };
    if hint.is_empty() {
        format!("Discord API {} ({action}): {}", e.status, e.body)
    } else {
        format!("Discord API {} ({action}): {hint} [{}]", e.status, e.body)
    }
}
