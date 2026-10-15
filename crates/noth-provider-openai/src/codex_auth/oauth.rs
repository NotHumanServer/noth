use std::time::Duration;

use noth_core::{Error, Result};
use serde_json::Value;

use super::jwt::{now_secs, rfc3339_utc};
use super::store::StoredTokens;
use super::{CLIENT_ID, RELOGIN_HINT, TOKEN_URL};

pub(super) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// reqwest の `form` feature を足さずに済ませる
pub(super) fn form_body(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub(super) async fn post_form(
    client: &reqwest::Client,
    pairs: &[(&str, &str)],
) -> std::result::Result<(u16, Value), Error> {
    let resp = client
        .post(TOKEN_URL)
        // ファイルロックの奪取条件(LOCK_STALE)の前提になる上限
        .timeout(Duration::from_secs(30))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body(pairs))
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or_default()))
}

pub(super) fn tokens_from_response(body: &Value, fallback_refresh: &str) -> Result<StoredTokens> {
    let access = body["access_token"].as_str().filter(|s| !s.is_empty());
    let Some(access) = access else {
        return Err(Error::Other(
            "Codex のトークン応答に access_token がありません".into(),
        ));
    };
    // refresh_token は使い切りでローテーションされることがあるため、返ってきたら必ず差し替える
    let refresh = body["refresh_token"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(fallback_refresh);
    // 空のまま保存すると以降の更新がすべて失敗するので、取得できた時点で弾く
    if refresh.trim().is_empty() {
        return Err(Error::Other(
            "Codex のトークン応答に refresh_token がありません".into(),
        ));
    }
    Ok(StoredTokens {
        access_token: access.to_string(),
        refresh_token: refresh.trim().to_string(),
        last_refresh: rfc3339_utc(now_secs()),
    })
}

pub(super) async fn refresh(
    client: &reqwest::Client,
    current: &StoredTokens,
) -> Result<StoredTokens> {
    let (status, body) = post_form(
        client,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &current.refresh_token),
            ("client_id", CLIENT_ID),
        ],
    )
    .await?;
    match status {
        200 => tokens_from_response(&body, &current.refresh_token),
        // 429 と 5xx は refresh_token がまだ有効なので、再ログインを促さない
        429 | 500..=599 => Err(Error::Api {
            status,
            body: "Codex のトークン更新が一時的に受け付けられませんでした。時間をおいて再試行してください".into(),
        }),
        _ => Err(Error::Other(format!(
            "Codex のトークン更新が拒否されました(HTTP {status})。{RELOGIN_HINT}"
        ))),
    }
}

pub(super) fn login_err(msg: impl std::fmt::Display) -> Error {
    Error::Other(format!("Codex のログインに失敗しました: {msg}"))
}

pub(super) async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: &Value,
) -> std::result::Result<(u16, Value), Error> {
    let resp = client
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or_default()))
}
