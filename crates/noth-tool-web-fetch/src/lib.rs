use std::sync::Arc;

use async_trait::async_trait;
use noth_backend_tinyfish::{FetchOptions, FetchResponse, TinyFish, require_backend};
use noth_core::registry::{ModuleCtx, Registry};
use noth_core::traits::Tool;
use noth_core::types::{ToolContext, ToolOutput, ToolSpec};
use noth_core::{Error, Result};
use serde_json::{Value, json};

const MAX_URLS: usize = 10;
/// 全 URL の本文に割り当てる文字数の合計。1ページが出力を占有して残りが切れるのを防ぐため URL 数で等分する
const TOTAL_TEXT_BUDGET: usize = 20_000;

pub fn register(reg: &mut Registry) {
    reg.register_tool("tool-web-fetch", factory);
}

fn factory(ctx: &ModuleCtx) -> Result<Arc<dyn Tool>> {
    require_backend(&ctx.section, "web-fetch")?;
    Ok(Arc::new(WebFetch {
        backend: TinyFish::from_config(ctx.config)?,
    }))
}

pub struct WebFetch {
    backend: TinyFish,
}

impl WebFetch {
    pub fn new(backend: TinyFish) -> Self {
        Self { backend }
    }
}

fn parse_input(input: &Value) -> std::result::Result<(Vec<String>, FetchOptions), String> {
    let urls: Vec<String> = input
        .get("urls")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().map(|u| u.as_str().map(str::to_string)).collect())
        .ok_or("urls は URL 文字列の配列で指定してください")?;
    if urls.is_empty() || urls.len() > MAX_URLS {
        return Err(format!("urls は 1〜{MAX_URLS} 件で指定してください"));
    }
    let purpose = input
        .get("purpose")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    Ok((urls, FetchOptions { purpose }))
}

fn truncate_chars(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((i, _)) => format!("{}\n...(以下省略)", &text[..i]),
        None => text.to_string(),
    }
}

/// 本文が1件も取れなかったときだけ is_error。失敗行は本文に埋もれないよう先頭に置く
fn format_response(resp: &FetchResponse) -> (String, bool) {
    let mut parts = Vec::new();
    for e in &resp.errors {
        let status = e.status.map(|s| format!(" (HTTP {s})")).unwrap_or_default();
        parts.push(format!("## {}\n[取得失敗] {}{status}", e.url, e.error));
    }
    let share = TOTAL_TEXT_BUDGET / resp.results.len().max(1);
    let mut any_text = false;
    for r in &resp.results {
        let mut s = format!("## {}\n", r.url);
        if let Some(t) = r.title.as_deref().filter(|t| !t.is_empty()) {
            s.push_str(&format!("タイトル: {t}\n"));
        }
        match r.text.as_deref().filter(|t| !t.is_empty()) {
            Some(text) => {
                any_text = true;
                s.push_str(&format!("\n{}", truncate_chars(text, share)));
            }
            None => s.push_str("(本文を取得できませんでした)"),
        }
        parts.push(s);
    }
    if parts.is_empty() {
        return ("取得結果が空でした。".into(), true);
    }
    (parts.join("\n\n"), !any_text)
}

#[async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description: "指定した URL のページ本文を Markdown で取得する(最大 10 件)。\
                web_search で見つけたページの中身を読むときに使う。URL ごとに成否が分かれ、失敗した URL には理由が付く。"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "urls": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": MAX_URLS,
                        "description": "取得する URL"
                    },
                    "purpose": {"type": "string", "description": "取得の目的。本文の抽出精度を上げるために渡す"}
                },
                "required": ["urls"]
            }),
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let (urls, opts) = match parse_input(&input) {
            Ok(v) => v,
            Err(text) => {
                return Ok(ToolOutput {
                    text,
                    is_error: true,
                });
            }
        };
        match self.backend.fetch(&urls, &opts).await {
            Ok(resp) => {
                let (text, is_error) = format_response(&resp);
                Ok(ToolOutput { text, is_error })
            }
            Err(Error::Api { body, .. }) => Ok(ToolOutput {
                text: body,
                is_error: true,
            }),
            Err(e) => Err(e),
        }
    }

    /// Fetch は枠を消費するので呼ばず、設定の検証(構築時に済み)だけで済ませる
    async fn health_check(&self) -> Result<String> {
        Ok("設定を確認しました(API は呼んでいません)".into())
    }
}

#[cfg(test)]
mod tests;
