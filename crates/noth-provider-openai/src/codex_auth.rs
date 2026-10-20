//! ChatGPT サブスクリプションで Codex バックエンドを叩くための OAuth(device code)とトークンの保管・更新
mod jwt;
mod oauth;
mod store;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::time::Duration;

use noth_core::Result;
use serde_json::json;

use oauth::{login_err, post_form, post_json, tokens_from_response};

pub use jwt::{account_id, base64url_decode, jwt_claims, jwt_exp, mask_account_id};
pub use store::{StoredTokens, TokenStore, load, save};

const ISSUER: &str = "https://auth.openai.com";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const REFRESH_SKEW_SECS: u64 = 120;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MIN_POLL_INTERVAL_SECS: u64 = 3;
const RELOGIN_HINT: &str = "`noth auth codex` を実行して再ログインしてください";

/// OpenAI は第三者のハーネスに自己申告を求めている(hermes-agent の agent/codex_headers.py
/// `codex_cloudflare_headers` の注記)。偽装せず Noth として名乗る。
/// 組織の data residency が強制されたワークスペースは residency ヘッダが無いと 401 になる
pub fn request_headers(access_token: &str) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("originator", "noth".to_string()),
        ("User-Agent", format!("Noth/{}", env!("CARGO_PKG_VERSION"))),
    ];
    if let Some(id) = account_id(access_token) {
        headers.push(("ChatGPT-Account-ID", id));
    }
    let claims = jwt_claims(access_token).unwrap_or_default();
    let auth = &claims[AUTH_CLAIM];
    let residency = ["chatgpt_data_residency", "chatgpt_compute_residency"]
        .iter()
        .find_map(|k| auth[*k].as_str().map(str::trim).filter(|s| !s.is_empty()));
    if let Some(r) = residency {
        headers.push(("x-openai-internal-codex-residency", r.to_string()));
    }
    headers
}

/// exp が読めないトークンは不透明とみなし、毎回更新しに行かない
pub fn needs_refresh(exp: Option<u64>, now: u64) -> bool {
    exp.is_some_and(|exp| exp <= now + REFRESH_SKEW_SECS)
}

/// device code でログインし、トークンを `store_path` に保存する
pub async fn login(store_path: &Path, print: impl Fn(&str)) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(login_err)?;
    let (status, device) = post_json(
        &client,
        &format!("{ISSUER}/api/accounts/deviceauth/usercode"),
        &json!({"client_id": CLIENT_ID}),
    )
    .await?;
    if status != 200 {
        return Err(login_err(format!(
            "デバイスコードの発行が HTTP {status} でした"
        )));
    }
    let (Some(user_code), Some(device_auth_id)) = (
        device["user_code"].as_str().filter(|s| !s.is_empty()),
        device["device_auth_id"].as_str().filter(|s| !s.is_empty()),
    ) else {
        return Err(login_err("デバイスコードの応答が不完全です"));
    };
    // interval は文字列で返ることがある
    let interval = device["interval"]
        .as_u64()
        .or_else(|| device["interval"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(5)
        .max(MIN_POLL_INTERVAL_SECS);

    print(&format!(
        "1. ブラウザで {ISSUER}/codex/device を開く\n2. コード {user_code} を入力する\nサインインを待っています(最大 15 分)..."
    ));

    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    let code_resp = loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(login_err("15 分以内にサインインが完了しませんでした"));
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let polled = post_json(
            &client,
            &format!("{ISSUER}/api/accounts/deviceauth/token"),
            &json!({"device_auth_id": device_auth_id, "user_code": user_code}),
        )
        .await;
        match polled {
            Ok((200, body)) => break body,
            // 403 / 404 はまだサインインが終わっていないだけ
            Ok((403 | 404, _)) => {}
            Ok((status, _)) => {
                return Err(login_err(format!("ポーリングが HTTP {status} でした")));
            }
            // 一時的な通信断で最初からやり直させない。期限まで続ける
            Err(e) => tracing::warn!(%e, "Codex ログインのポーリングに失敗しました"),
        }
    };

    let (Some(code), Some(verifier)) = (
        code_resp["authorization_code"]
            .as_str()
            .filter(|s| !s.is_empty()),
        code_resp["code_verifier"]
            .as_str()
            .filter(|s| !s.is_empty()),
    ) else {
        return Err(login_err(
            "認可コードまたは code_verifier が応答に含まれていません",
        ));
    };
    let redirect_uri = format!("{ISSUER}/deviceauth/callback");
    let (status, body) = post_form(
        &client,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ],
    )
    .await?;
    if status != 200 {
        return Err(login_err(format!("トークン交換が HTTP {status} でした")));
    }
    let tokens = tokens_from_response(&body, "")?;
    save(store_path, &tokens)?;
    print(&format!("保存しました: {}", store_path.display()));
    Ok(())
}
