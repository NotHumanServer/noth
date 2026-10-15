use noth_core::traits::{Completion, LayeredPrompt};
use noth_core::types::{
    Billing, CompletionParams, CostRecord, ExchangeItem, Purpose, Role, ToolCall, ToolResult, Turn,
    Usage,
};
use noth_core::{Error, Result};
use serde_json::{Value, json};

use crate::pricing::{self, Overrides, TokenCounts};

/// 先頭が model になるとき API が弾くため補う
const OMITTED_NOTICE: &str = "[以前の会話は省略されています]";

/// API が id を返さなかった functionCall に振る接頭辞。API が返す id と衝突しない形にして、接頭辞だけで生成物と判別する
const GENERATED_ID_PREFIX: &str = "noth_gen_";

/// 応答をまたいで id を一意にするための連番。同じ Worker 実行内の複数ラウンドで functionResponse の対応が曖昧にならないようにする
static GENERATED_ID_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 省略時はモデルの既定値になる。未知の値は実行時に 400 で気づくより起動時に弾く
pub fn parse_thinking_level(s: &str) -> Result<&'static str> {
    match s {
        "minimal" => Ok("MINIMAL"),
        "low" => Ok("LOW"),
        "medium" => Ok("MEDIUM"),
        "high" => Ok("HIGH"),
        other => Err(Error::Config(format!(
            "provider.google: thinking_level は minimal / low / medium / high のどれかで指定してください(`{other}` が指定されています)"
        ))),
    }
}

fn text_part(text: &str) -> Value {
    json!({"text": text})
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "model",
    }
}

struct Content {
    role: &'static str,
    parts: Vec<Value>,
    /// API が返した model の parts。thought signature を守るため、他の part と混ぜない
    raw: bool,
}

fn push_parts(contents: &mut Vec<Content>, role: &'static str, parts: Vec<Value>, raw: bool) {
    if parts.is_empty() {
        return;
    }
    match contents.last_mut() {
        Some(c) if c.role == role && !c.raw && !raw => c.parts.extend(parts),
        _ => contents.push(Content { role, parts, raw }),
    }
}

fn push_turn(contents: &mut Vec<Content>, turn: &Turn) {
    // 空 text の part は API が拒否する
    if turn.text.is_empty() {
        return;
    }
    push_parts(
        contents,
        role_str(turn.role),
        vec![text_part(&turn.text)],
        false,
    );
}

fn function_call_part(call: &ToolCall) -> Value {
    let mut fc = json!({"name": call.name, "args": call.input});
    if !is_generated_id(&call.id) {
        fc["id"] = json!(call.id);
    }
    json!({"functionCall": fc})
}

/// 生成した id は API に送った覚えがないので、送り返すと対応する functionCall が見つからず弾かれる
fn is_generated_id(id: &str) -> bool {
    id.starts_with(GENERATED_ID_PREFIX)
}

fn function_response_part(r: &ToolResult) -> Value {
    let mut response = json!({"output": r.output});
    if r.is_error {
        response["error"] = json!(true);
    }
    let mut fr = json!({"name": r.name, "response": response});
    if !is_generated_id(&r.call_id) {
        fr["id"] = json!(r.call_id);
    }
    json!({"functionResponse": fr})
}

fn push_exchange(contents: &mut Vec<Content>, item: &ExchangeItem) {
    match item {
        ExchangeItem::Assistant { text, calls, raw } => {
            let parts = match raw {
                Some(Value::Array(parts)) => parts.clone(),
                Some(other) => vec![other.clone()],
                None => text
                    .iter()
                    .filter(|t| !t.is_empty())
                    .map(|t| text_part(t))
                    .chain(calls.iter().map(function_call_part))
                    .collect(),
            };
            push_parts(contents, "model", parts, raw.is_some());
        }
        ExchangeItem::ToolResults(results) => {
            let parts = results.iter().map(function_response_part).collect();
            push_parts(contents, "user", parts, false);
        }
    }
}

pub fn build_request(
    prompt: &LayeredPrompt,
    params: &CompletionParams,
    thinking_level: Option<&str>,
) -> Result<Value> {
    // Gemini は暗黙キャッシュで境界指定が無いため、層を system に連結するだけにする
    let system = [
        &prompt.harness_rules,
        &prompt.profile,
        &prompt.episodic_digest,
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .cloned()
    .collect::<Vec<_>>()
    .join("\n\n");

    let mut contents: Vec<Content> = Vec::new();
    for turn in prompt.active_log.iter().chain(&prompt.ephemeral_tail) {
        push_turn(&mut contents, turn);
    }
    for item in &prompt.tool_exchange {
        push_exchange(&mut contents, item);
    }
    if contents.is_empty() {
        return Err(Error::Other("google: contents is empty".into()));
    }
    if contents.last().is_some_and(|c| c.role != "user") {
        return Err(Error::Other(
            "google: 最後の content が user ではありません".into(),
        ));
    }
    if contents[0].role == "model" {
        contents.insert(
            0,
            Content {
                role: "user",
                parts: vec![text_part(OMITTED_NOTICE)],
                raw: false,
            },
        );
    }

    let mut generation_config = json!({"maxOutputTokens": params.max_tokens});
    if let Some(level) = thinking_level {
        generation_config["thinkingConfig"] = json!({"thinkingLevel": level});
    }
    let mut body = json!({
        "contents": contents
            .into_iter()
            .map(|c| json!({"role": c.role, "parts": c.parts}))
            .collect::<Vec<_>>(),
        "generationConfig": generation_config,
    });
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts": [text_part(&system)]});
    }
    if !prompt.tools.is_empty() {
        let decls: Vec<Value> = prompt
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "parametersJsonSchema": t.input_schema}))
            .collect();
        body["tools"] = json!([{"functionDeclarations": decls}]);
    }
    Ok(body)
}

/// 費用計算に必要な、リクエスト側の情報
pub struct CostContext<'a> {
    pub request_model: &'a str,
    /// 単価表を引ける接続先か。Vertex と opencode-go は単価が未確認
    pub priced: bool,
    pub overrides: &'a Overrides,
    pub purpose: Purpose,
    pub run_id: Option<String>,
}

pub fn parse_response(resp: &Value, cost_ctx: &CostContext) -> Result<Completion> {
    let usage = parse_usage(resp);
    let candidate = resp.pointer("/candidates/0");
    let reason = candidate
        .and_then(|c| c.get("finishReason"))
        .and_then(Value::as_str);
    if let Some(r @ ("MISSING_THOUGHT_SIGNATURE" | "MALFORMED_FUNCTION_CALL")) = reason {
        return Err(Error::Other(format!(
            "google: 応答が異常終了しました ({r})"
        )));
    }
    let parts = candidate
        .and_then(|c| c.pointer("/content/parts"))
        .and_then(Value::as_array);

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for p in parts.into_iter().flatten() {
        if p.get("thought").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(t) = p.get("text").and_then(Value::as_str) {
            if !t.trim().is_empty() {
                text.push_str(t);
            }
        } else if let Some(fc) = p.get("functionCall") {
            let name = fc.get("name").and_then(Value::as_str).ok_or_else(|| {
                Error::Other("google: functionCall に `name` がありません".into())
            })?;
            let id = fc
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map_or_else(
                    || {
                        let n = GENERATED_ID_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        format!("{GENERATED_ID_PREFIX}{n}")
                    },
                    str::to_string,
                );
            tool_calls.push(ToolCall {
                id,
                name: name.to_string(),
                input: fc.get("args").cloned().unwrap_or_else(|| json!({})),
            });
        }
    }

    if text.is_empty() && tool_calls.is_empty() {
        let block = resp
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str);
        tracing::warn!(
            finish_reason = reason,
            block_reason = block,
            "google の応答に本文がありません"
        );
    } else if let Some(r) = reason {
        if r == "STOP" {
            tracing::debug!(finish_reason = r, "google completion");
        } else {
            tracing::warn!(finish_reason = r, "google の応答が正常終了ではありません");
        }
    }

    let raw_assistant = if tool_calls.is_empty() {
        None
    } else {
        parts.map(|p| Value::Array(p.clone()))
    };
    Ok(Completion {
        text: (!text.is_empty()).then_some(text),
        tool_calls,
        usage,
        raw_assistant,
        cost: Some(cost_record(resp, cost_ctx)),
    })
}

fn cost_record(resp: &Value, ctx: &CostContext) -> CostRecord {
    let model = resp
        .get("modelVersion")
        .and_then(Value::as_str)
        .unwrap_or(ctx.request_model);
    let u = parse_usage(resp);
    // promptTokenCount はキャッシュ分を含むので、キャッシュ単価で数える分を引く
    let counts = TokenCounts {
        input: u.input_tokens.saturating_sub(u.cached_tokens),
        cache_read: u.cached_tokens,
        output: u.output_tokens,
    };
    let amount_usd = pricing::lookup(model, ctx.priced, counts.prompt_tokens(), ctx.overrides)
        .map(|rates| pricing::amount_usd(&rates, &counts));
    CostRecord {
        source: "provider-google".into(),
        model: model.to_string(),
        purpose: ctx.purpose,
        run_id: ctx.run_id.clone(),
        input_tokens: counts.input,
        cache_read_tokens: counts.cache_read,
        // 暗黙キャッシュの書き込みは課金されない
        cache_write_tokens: 0,
        output_tokens: counts.output,
        unit: None,
        amount_usd,
        billing: Billing::Metered,
    }
}

fn parse_usage(resp: &Value) -> Usage {
    let u = |k: &str| {
        resp.pointer(&format!("/usageMetadata/{k}"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Usage {
        input_tokens: u("promptTokenCount") + u("toolUsePromptTokenCount"),
        cached_tokens: u("cachedContentTokenCount"),
        output_tokens: u("candidatesTokenCount") + u("thoughtsTokenCount"),
    }
}

/// エラー JSON の code は AI Studio が文字列、標準形式が整数。status は無いことがある
pub struct ApiError {
    pub label: Option<String>,
    pub message: String,
    pub details: Vec<Value>,
}

impl ApiError {
    fn detail(&self, type_suffix: &str) -> Option<&Value> {
        self.details.iter().find(|d| {
            d.get("@type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.ends_with(type_suffix))
        })
    }

    /// 上限到達は待っても回復しない。status や code の表記が形式ごとに違うため、
    /// QuotaFailure・文言・code の3経路で判定する
    pub fn is_quota_exhausted(&self) -> bool {
        self.detail("google.rpc.QuotaFailure").is_some()
            || self.message.to_lowercase().contains("quota")
            || self.label.as_deref() == Some("quota_exceeded")
    }

    /// RetryInfo.retryDelay("12s" や "1.5s")。Retry-After ヘッダが無いときの待ち時間に使う
    pub fn retry_delay(&self) -> Option<std::time::Duration> {
        let s = self
            .detail("google.rpc.RetryInfo")?
            .get("retryDelay")?
            .as_str()?;
        let secs: f64 = s.strip_suffix('s')?.parse().ok()?;
        (secs.is_finite() && secs >= 0.0).then(|| std::time::Duration::from_secs_f64(secs))
    }
}

pub fn parse_error(raw: &str) -> ApiError {
    let v = serde_json::from_str::<Value>(raw).ok();
    let err = v.as_ref().and_then(|v| v.get("error"));
    let label = err.and_then(|e| {
        e.get("status")
            .and_then(Value::as_str)
            .or_else(|| e.get("code").and_then(Value::as_str))
            .map(str::to_string)
    });
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(raw);
    let details = err
        .and_then(|e| e.get("details"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    ApiError {
        label,
        message: message.to_string(),
        details,
    }
}

/// 1 行・約 200 文字に整える。数値の status は Error::Api の表示と二重になるため入れない
pub fn format_error(status: u16, raw: &str) -> String {
    const MAX_CHARS: usize = 200;
    let e = parse_error(raw);
    let one_line = e.message.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = one_line
        .strip_prefix(&format!("{status}:"))
        .map_or(one_line.as_str(), str::trim_start);
    let body = match &e.label {
        Some(l) => format!("{l}: {trimmed}"),
        None => trimmed.to_string(),
    };
    match body.char_indices().nth(MAX_CHARS) {
        Some((i, _)) => format!("{}...", &body[..i]),
        None => body,
    }
}

#[cfg(test)]
mod tests {
    use noth_core::types::{ToolSpec, TurnKind};

    use super::*;

    fn turn(role: Role, text: &str) -> Turn {
        Turn {
            id: None,
            role,
            kind: TurnKind::Buffer,
            text: text.into(),
            chunk_id: None,
        }
    }

    fn prompt() -> LayeredPrompt {
        LayeredPrompt {
            harness_rules: "rules".into(),
            profile: "profile".into(),
            episodic_digest: "digest".into(),
            active_log: vec![turn(Role::User, "a"), turn(Role::Assistant, "b")],
            ephemeral_tail: vec![turn(Role::User, "c"), turn(Role::User, "d")],
            ..Default::default()
        }
    }

    fn cx() -> CostContext<'static> {
        CostContext {
            request_model: "gemini-2.5-flash",
            priced: true,
            overrides: &Overrides {
                input: None,
                cached_input: None,
                output: None,
            },
            purpose: Purpose::Main,
            run_id: None,
        }
    }

    fn params() -> CompletionParams {
        CompletionParams {
            max_tokens: 100,
            ..Default::default()
        }
    }

    #[test]
    fn request_maps_layers_and_merges_roles() {
        let body = build_request(&prompt(), &params(), Some("HIGH")).unwrap();
        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "rules\n\nprofile\n\ndigest"
        );
        let c = body["contents"].as_array().unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0]["role"], "user");
        assert_eq!(c[1]["role"], "model");
        assert_eq!(c[2]["role"], "user");
        assert_eq!(c[2]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 100);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn empty_digest_is_left_out_and_no_thinking_by_default() {
        let mut p = prompt();
        p.episodic_digest.clear();
        let body = build_request(&p, &params(), None).unwrap();
        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "rules\n\nprofile"
        );
        assert!(body["generationConfig"].get("thinkingConfig").is_none());
    }

    #[test]
    fn leading_model_turn_gets_notice() {
        let mut p = prompt();
        p.active_log = vec![turn(Role::Assistant, "b")];
        p.ephemeral_tail = vec![turn(Role::User, "c")];
        let body = build_request(&p, &params(), None).unwrap();
        let c = body["contents"].as_array().unwrap();
        assert_eq!(c[0]["role"], "user");
        assert_eq!(c[0]["parts"][0]["text"], OMITTED_NOTICE);
        assert_eq!(c[1]["role"], "model");
    }

    #[test]
    fn trailing_model_turn_is_error() {
        let mut p = prompt();
        p.ephemeral_tail.clear();
        assert!(build_request(&p, &params(), None).is_err());
    }

    #[test]
    fn api_issued_call_prefixed_ids_are_sent_back() {
        let part = function_call_part(&call("call_1"));
        assert_eq!(part["functionCall"]["id"], "call_1");
    }

    #[test]
    fn empty_prompt_is_error() {
        let p = LayeredPrompt::default();
        assert!(build_request(&p, &params(), None).is_err());
    }

    #[test]
    fn tools_become_function_declarations() {
        let mut p = prompt();
        p.tools = vec![ToolSpec {
            name: "web_search".into(),
            description: "d".into(),
            input_schema: json!({"type": "object"}),
        }];
        let body = build_request(&p, &params(), None).unwrap();
        let d = &body["tools"][0]["functionDeclarations"][0];
        assert_eq!(d["name"], "web_search");
        assert_eq!(d["parametersJsonSchema"], json!({"type": "object"}));
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "web_search".into(),
            input: json!({"q": "x"}),
        }
    }

    #[test]
    fn raw_assistant_is_replayed_verbatim() {
        let raw = json!([
            {"text": "考え中", "thought": true, "thoughtSignature": "sig0"},
            {"functionCall": {"id": "f1", "name": "web_search", "args": {"q": "x"}}, "thoughtSignature": "sig1"}
        ]);
        let mut p = prompt();
        p.tool_exchange = vec![
            ExchangeItem::Assistant {
                text: None,
                calls: vec![call("f1")],
                raw: Some(raw.clone()),
            },
            ExchangeItem::ToolResults(vec![ToolResult {
                call_id: "f1".into(),
                name: "web_search".into(),
                output: "found".into(),
                is_error: false,
            }]),
        ];
        let body = build_request(&p, &params(), None).unwrap();
        let c = body["contents"].as_array().unwrap();
        assert_eq!(c.len(), 5);
        assert_eq!(c[3]["role"], "model");
        assert_eq!(c[3]["parts"], raw);
        assert_eq!(c[4]["role"], "user");
        assert_eq!(
            c[4]["parts"][0]["functionResponse"],
            json!({"name": "web_search", "id": "f1", "response": {"output": "found"}})
        );
    }

    #[test]
    fn without_raw_function_calls_are_built_and_errors_flagged() {
        let mut p = prompt();
        p.tool_exchange = vec![
            ExchangeItem::Assistant {
                text: Some("検索します".into()),
                calls: vec![call("f1")],
                raw: None,
            },
            ExchangeItem::ToolResults(vec![ToolResult {
                call_id: "f1".into(),
                name: "web_search".into(),
                output: "boom".into(),
                is_error: true,
            }]),
        ];
        let body = build_request(&p, &params(), None).unwrap();
        let c = body["contents"].as_array().unwrap();
        // 末尾の user ターンの後ろに model が続くので merge されない
        assert_eq!(c[3]["parts"][0]["text"], "検索します");
        assert_eq!(c[3]["parts"][1]["functionCall"]["id"], "f1");
        assert_eq!(
            c[4]["parts"][0]["functionResponse"]["response"],
            json!({"output": "boom", "error": true})
        );
    }

    #[test]
    fn generated_ids_are_not_sent_back() {
        assert!(is_generated_id("noth_gen_0"));
        // API 自身が返す call_ 形式の id は生成物ではない
        assert!(!is_generated_id("call_1"));
        let part = function_response_part(&ToolResult {
            call_id: "noth_gen_1".into(),
            name: "n".into(),
            output: "o".into(),
            is_error: false,
        });
        assert!(part["functionResponse"].get("id").is_none());
    }

    #[test]
    fn response_skips_thoughts_and_collects_calls() {
        let resp = json!({
            "candidates": [{
                "finishReason": "STOP",
                "content": {"role": "model", "parts": [
                    {"text": "hidden", "thought": true},
                    {"text": "こんにちは"},
                    {"functionCall": {"name": "a", "args": {"k": 1}}, "thoughtSignature": "s"},
                    {"functionCall": {"id": "x9", "name": "b", "args": {}}}
                ]}
            }],
            "usageMetadata": {
                "promptTokenCount": 100, "toolUsePromptTokenCount": 5,
                "cachedContentTokenCount": 60,
                "candidatesTokenCount": 10, "thoughtsTokenCount": 7
            }
        });
        let c = parse_response(&resp, &cx()).unwrap();
        assert_eq!(c.text.as_deref(), Some("こんにちは"));
        assert_eq!(c.tool_calls.len(), 2);
        let generated = c.tool_calls[0].id.clone();
        assert!(is_generated_id(&generated));
        // 別の応答では同じ id を振らない
        let again = parse_response(&resp, &cx()).unwrap();
        assert_ne!(again.tool_calls[0].id, generated);
        assert_eq!(c.tool_calls[0].input, json!({"k": 1}));
        assert_eq!(c.tool_calls[1].id, "x9");
        assert_eq!(
            c.usage,
            Usage {
                input_tokens: 105,
                cached_tokens: 60,
                output_tokens: 17
            }
        );
        let raw = c.raw_assistant.unwrap();
        assert_eq!(raw.as_array().unwrap().len(), 4);
        assert_eq!(raw[2]["thoughtSignature"], "s");
    }

    #[test]
    fn plain_text_has_no_raw() {
        let resp = json!({"candidates": [{"content": {"parts": [{"text": "hi"}]}}]});
        let c = parse_response(&resp, &cx()).unwrap();
        assert_eq!(c.text.as_deref(), Some("hi"));
        assert!(c.raw_assistant.is_none());
    }

    #[test]
    fn blocked_response_is_empty_not_error() {
        let resp = json!({"candidates": [{"finishReason": "SAFETY"}]});
        let c = parse_response(&resp, &cx()).unwrap();
        assert!(c.text.is_none() && c.tool_calls.is_empty());
        let resp = json!({"promptFeedback": {"blockReason": "SAFETY"}});
        assert!(parse_response(&resp, &cx()).unwrap().text.is_none());
    }

    #[test]
    fn broken_finish_reasons_are_errors() {
        for r in ["MISSING_THOUGHT_SIGNATURE", "MALFORMED_FUNCTION_CALL"] {
            let resp = json!({"candidates": [{"finishReason": r}]});
            assert!(parse_response(&resp, &cx()).is_err(), "{r}");
        }
    }

    #[test]
    fn thinking_level_validation() {
        assert_eq!(parse_thinking_level("minimal").unwrap(), "MINIMAL");
        assert_eq!(parse_thinking_level("high").unwrap(), "HIGH");
        assert!(parse_thinking_level("HIGH").is_err());
        assert!(parse_thinking_level("max").is_err());
    }

    #[test]
    fn error_formats() {
        // AI Studio: code が文字列、status なし
        let e = r#"{"error":{"code":"quota_exceeded","message":"too\nmany"}}"#;
        assert_eq!(format_error(429, e), "quota_exceeded: too many");
        // 標準形式: code が整数、status あり
        let e = r#"{"error":{"code":429,"message":"slow down","status":"RESOURCE_EXHAUSTED"}}"#;
        assert_eq!(format_error(429, e), "RESOURCE_EXHAUSTED: slow down");
        // 整数 code だけなら label なし
        let e = r#"{"error":{"code":400,"message":"bad"}}"#;
        assert_eq!(format_error(400, e), "bad");
        assert_eq!(format_error(502, "502: Bad Gateway"), "Bad Gateway");
        assert_eq!(format_error(500, &"x".repeat(300)).chars().count(), 203);
    }

    fn quota_failure_error() -> &'static str {
        r#"{"error":{"code":429,"message":"limit","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.QuotaFailure"}]}}"#
    }

    #[test]
    fn quota_exhaustion_detection() {
        assert!(parse_error(quota_failure_error()).is_quota_exhausted());
        let by_message = r#"{"error":{"code":429,"message":"You exceeded your current Quota","status":"RESOURCE_EXHAUSTED"}}"#;
        assert!(parse_error(by_message).is_quota_exhausted());
        let by_code = r#"{"error":{"code":"quota_exceeded","message":"x"}}"#;
        assert!(parse_error(by_code).is_quota_exhausted());
        let rate = r#"{"error":{"code":429,"message":"slow down","status":"RESOURCE_EXHAUSTED"}}"#;
        assert!(!parse_error(rate).is_quota_exhausted());
        assert!(!parse_error("not json").is_quota_exhausted());
    }

    #[test]
    fn retry_delay_from_retry_info() {
        let with = |d: &str| {
            format!(
                r#"{{"error":{{"code":429,"message":"m","details":[{{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"{d}"}}]}}}}"#
            )
        };
        assert_eq!(
            parse_error(&with("12s")).retry_delay(),
            Some(std::time::Duration::from_secs(12))
        );
        assert_eq!(
            parse_error(&with("1.5s")).retry_delay(),
            Some(std::time::Duration::from_millis(1500))
        );
        assert_eq!(parse_error(&with("soon")).retry_delay(), None);
        assert_eq!(parse_error(quota_failure_error()).retry_delay(), None);
    }

    #[test]
    fn cost_splits_cached_and_includes_thoughts() {
        let resp = json!({
            "modelVersion": "gemini-2.5-flash",
            "candidates": [{"content": {"parts": [{"text": "hi"}]}}],
            "usageMetadata": {
                "promptTokenCount": 1000, "toolUsePromptTokenCount": 100,
                "cachedContentTokenCount": 600,
                "candidatesTokenCount": 10, "thoughtsTokenCount": 40
            }
        });
        let cx = CostContext {
            purpose: Purpose::Worker,
            run_id: Some("r1".into()),
            ..cx()
        };
        let c = parse_response(&resp, &cx).unwrap().cost.unwrap();
        assert_eq!(c.source, "provider-google");
        assert_eq!(c.model, "gemini-2.5-flash");
        assert_eq!(c.purpose, Purpose::Worker);
        assert_eq!(c.run_id.as_deref(), Some("r1"));
        assert_eq!(
            (
                c.input_tokens,
                c.cache_read_tokens,
                c.cache_write_tokens,
                c.output_tokens
            ),
            (500, 600, 0, 50)
        );
        let want = (500.0 * 0.3 + 600.0 * 0.03 + 50.0 * 2.5) / 1e6;
        assert!((c.amount_usd.unwrap() - want).abs() < 1e-12);
        assert_eq!(c.billing, Billing::Metered);
    }

    #[test]
    fn cost_unpriced_target_has_no_amount() {
        let resp = json!({"candidates": [{"content": {"parts": [{"text": "hi"}]}}],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 1}});
        let cx = CostContext {
            priced: false,
            ..cx()
        };
        let c = parse_response(&resp, &cx).unwrap().cost.unwrap();
        assert_eq!(c.model, "gemini-2.5-flash");
        assert_eq!(c.amount_usd, None);
        assert_eq!(c.input_tokens, 10);
    }
}
