use std::sync::Arc;

use async_trait::async_trait;
use noth_backend_tinyfish::meter::{report_cost, report_rate_limit};
use noth_backend_tinyfish::{SearchOptions, SearchResponse, TinyFish, require_backend};
use noth_core::registry::{ModuleCtx, Registry};
use noth_core::traits::Tool;
use noth_core::types::{Billing, ToolContext, ToolOutput, ToolSpec};
use noth_core::{Error, Result};
use serde_json::{Value, json};

const SOURCE: &str = "tool-web-search";

pub fn register(reg: &mut Registry) {
    reg.register_tool("tool-web-search", factory);
}

fn factory(ctx: &ModuleCtx) -> Result<Arc<dyn Tool>> {
    require_backend(&ctx.section, "web-search")?;
    Ok(Arc::new(WebSearch {
        backend: TinyFish::from_config(ctx.config)?,
    }))
}

pub struct WebSearch {
    backend: TinyFish,
}

impl WebSearch {
    pub fn new(backend: TinyFish) -> Self {
        Self { backend }
    }
}

fn parse_options(input: &Value) -> std::result::Result<(String, SearchOptions), String> {
    let query = input
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or("query(検索語)は必須です")?
        .to_string();
    let domain_type = match input.get("domain_type") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_str() {
            Some(s @ ("web" | "news" | "research_paper")) => Some(s.to_string()),
            _ => return Err("domain_type は web / news / research_paper のいずれかです".into()),
        },
    };
    let uint = |key: &str| match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or(format!("{key} は 0 以上の整数で指定してください")),
    };
    let recency_minutes = uint("recency_minutes")?;
    if recency_minutes == Some(0) {
        return Err("recency_minutes は 1 以上で指定してください".into());
    }
    let include_domains = match input.get("include_domains") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => v
            .as_array()
            .and_then(|a| a.iter().map(|d| d.as_str().map(str::to_string)).collect())
            .ok_or("include_domains は文字列の配列で指定してください")?,
    };
    Ok((
        query,
        SearchOptions {
            domain_type,
            recency_minutes,
            include_domains,
            page: uint("page")?,
            purpose: None,
        },
    ))
}

fn format_results(query: &str, resp: &SearchResponse) -> String {
    if resp.results.is_empty() {
        return format!("「{query}」の検索結果はありませんでした。検索語や条件を変えてください。");
    }
    let mut out = String::new();
    for (i, r) in resp.results.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            out.push_str(&format!("   {}\n", r.snippet.replace('\n', " ")));
        }
        if let Some(d) = r.date.as_deref().filter(|d| !d.is_empty()) {
            out.push_str(&format!("   日付: {d}\n"));
        }
    }
    out.truncate(out.trim_end().len());
    out
}

#[async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".into(),
            description: "Web を検索し、タイトル・URL・スニペットの一覧を返す。最新の情報や事実の裏取りの入口に使う。\
                本文が必要なら、結果の URL を web_fetch で取得する。"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "検索語。site:example.com のような演算子も使える"},
                    "domain_type": {
                        "type": "string",
                        "enum": ["web", "news", "research_paper"],
                        "description": "検索対象。既定は web"
                    },
                    "recency_minutes": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "直近この分数以内の結果に絞る"
                    },
                    "include_domains": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "この domain の結果だけにする"
                    },
                    "page": {"type": "integer", "minimum": 0, "description": "0 始まりのページ番号"}
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let (query, opts) = match parse_options(&input) {
            Ok(v) => v,
            Err(text) => {
                return Ok(ToolOutput {
                    text,
                    is_error: true,
                });
            }
        };
        match self.backend.search_metered(&query, &opts).await {
            Ok(resp) => {
                report_rate_limit(ctx, SOURCE, "search", resp.rate_limit.as_ref());
                report_cost(
                    ctx,
                    SOURCE,
                    "search",
                    ("request", 1),
                    Some(0.0),
                    Billing::Free,
                );
                Ok(ToolOutput {
                    text: format_results(&query, &resp.body),
                    is_error: false,
                })
            }
            Err(Error::Api { body, .. }) => Ok(ToolOutput {
                text: body,
                is_error: true,
            }),
            Err(e) => Err(e),
        }
    }

    async fn health_check(&self) -> Result<String> {
        self.backend.health_check().await
    }

    async fn account_status(&self) -> Result<Option<String>> {
        self.backend.account_status().await.map(Some)
    }
}

#[cfg(test)]
mod tests;
