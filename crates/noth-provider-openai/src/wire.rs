use noth_core::traits::{Completion, LayeredPrompt};
use noth_core::types::{
    Billing, CompletionParams, CostRecord, ExchangeItem, Purpose, Role, ToolCall, Turn, Usage,
};
use noth_core::{Error, Result};
use serde_json::{Value, json};

use crate::pricing::{self, Overrides, TokenCounts};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Chat,
    Responses,
}

impl Format {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "chat" => Ok(Self::Chat),
            "responses" => Ok(Self::Responses),
            other => Err(Error::Config(format!(
                "provider.openai: format は \"chat\" か \"responses\" で指定してください(`{other}` が指定されています)"
            ))),
        }
    }
}

/// モデルごとに受け付ける値が違うので、ここでは綴りだけ検証する。非対応の値は API が 400 で返す
pub fn parse_effort(s: &str) -> Result<&'static str> {
    match s {
        "none" => Ok("none"),
        "minimal" => Ok("minimal"),
        "low" => Ok("low"),
        "medium" => Ok("medium"),
        "high" => Ok("high"),
        "xhigh" => Ok("xhigh"),
        "max" => Ok("max"),
        other => Err(Error::Config(format!(
            "provider.openai: effort は none / minimal / low / medium / high / xhigh / max のどれかで指定してください(`{other}` が指定されています)"
        ))),
    }
}

/// OpenAI のプレフィックスキャッシュは自動で、境界の指定手段がない。そのため `cache_to_end` は見ず、
/// Layer 0→1→2→3→4 の順を崩さないことだけがキャッシュヒットの条件になる
fn system_text(prompt: &LayeredPrompt) -> String {
    let mut s = format!("{}\n\n{}", prompt.harness_rules, prompt.profile);
    for layer in [&prompt.memory, &prompt.episodic_digest] {
        if !layer.is_empty() {
            s.push_str("\n\n");
            s.push_str(layer);
        }
    }
    s
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// 連続する同 role のターンは 1 件にまとめる。空文字のターンは API が拒否することがあるので捨てる
fn merged_turns(prompt: &LayeredPrompt) -> Vec<(Role, String)> {
    let mut out: Vec<(Role, String)> = Vec::new();
    let turns = prompt.active_log.iter().chain(&prompt.ephemeral_tail);
    for Turn { role, text, .. } in turns {
        if text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some((r, t)) if r == role => {
                t.push_str("\n\n");
                t.push_str(text);
            }
            _ => out.push((*role, text.clone())),
        }
    }
    out
}

fn arguments_string(call: &ToolCall) -> String {
    call.input.to_string()
}

pub fn build_chat_request(
    model: &str,
    prompt: &LayeredPrompt,
    params: &CompletionParams,
    effort: Option<&str>,
    compat: bool,
) -> Result<Value> {
    let mut messages = vec![json!({"role": "system", "content": system_text(prompt)})];
    for (role, text) in merged_turns(prompt) {
        messages.push(json!({"role": role_str(role), "content": text}));
    }
    for item in &prompt.tool_exchange {
        match item {
            ExchangeItem::Assistant { text, calls, .. } => {
                let text = text.as_deref().filter(|t| !t.is_empty());
                if text.is_none() && calls.is_empty() {
                    continue;
                }
                let mut m = json!({"role": "assistant", "content": text});
                if !calls.is_empty() {
                    m["tool_calls"] = calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": {"name": c.name, "arguments": arguments_string(c)},
                            })
                        })
                        .collect();
                }
                messages.push(m);
            }
            ExchangeItem::ToolResults(results) => {
                for r in results {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": r.call_id,
                        "content": r.output,
                    }));
                }
            }
        }
    }
    if messages.len() == 1 {
        return Err(Error::Other("openai: messages is empty".into()));
    }

    let mut body = json!({"model": model, "messages": messages});
    // 公式 API は max_tokens を非推奨とし o 系で拒否するが、互換サーバーは max_completion_tokens を
    // 知らないことが多い。base_url / preset 指定時は互換サーバーとみなして max_tokens を使う
    let key = if compat {
        "max_tokens"
    } else {
        "max_completion_tokens"
    };
    body[key] = params.max_tokens.into();
    if let Some(effort) = effort {
        body["reasoning_effort"] = effort.into();
    }
    // tool_choice は送らない。モデルが呼ぶかどうかを選ぶ
    if !prompt.tools.is_empty() {
        body["tools"] = prompt
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    },
                })
            })
            .collect();
    }
    Ok(body)
}

pub fn build_responses_request(
    model: &str,
    prompt: &LayeredPrompt,
    params: &CompletionParams,
    effort: Option<&str>,
) -> Result<Value> {
    let mut input: Vec<Value> = merged_turns(prompt)
        .into_iter()
        .map(|(role, text)| json!({"type": "message", "role": role_str(role), "content": text}))
        .collect();
    for item in &prompt.tool_exchange {
        match item {
            ExchangeItem::Assistant { text, calls, raw } => match raw {
                // reasoning item を欠くと tool 往復が 400 になるため、生の output を並べ替えず返す
                Some(Value::Array(items)) => input.extend(replayable_output(items)),
                Some(other) => input.push(other.clone()),
                None => {
                    if let Some(t) = text.as_deref().filter(|t| !t.is_empty()) {
                        input.push(json!({"type": "message", "role": "assistant", "content": t}));
                    }
                    for c in calls {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": c.id,
                            "name": c.name,
                            "arguments": arguments_string(c),
                        }));
                    }
                }
            },
            ExchangeItem::ToolResults(results) => {
                for r in results {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": r.call_id,
                        "output": r.output,
                    }));
                }
            }
        }
    }
    if input.is_empty() {
        return Err(Error::Other("openai: input is empty".into()));
    }

    // store: false で応答をサーバーに残さない。代わりに reasoning を毎回自分で持ち回る
    let mut body = json!({
        "model": model,
        "store": false,
        "instructions": system_text(prompt),
        "input": input,
        "max_output_tokens": params.max_tokens,
        // store: false では reasoning を暗号化して受け取らないと、次の tool 往復に持ち回れない
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(effort) = effort {
        body["reasoning"] = json!({"effort": effort});
    }
    if !prompt.tools.is_empty() {
        body["tools"] = prompt
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                })
            })
            .collect();
    }
    Ok(body)
}

/// store: false では サーバー側に item が残らず、id で引くと 404 になる(hermes-agent の
/// codex_responses_adapter.py `_replay_reasoning_items`)。また reasoning と同じ応答の message id は
/// その reasoning id に紐づくため、reasoning の id だけ外すと "without its required reasoning item" の
/// 400 になる(同 `_replay_message_items`, #97427)。function_call の id / call_id は残す
fn replayable_output(items: &[Value]) -> Vec<Value> {
    let has_reasoning = items
        .iter()
        .any(|i| i.get("type").and_then(Value::as_str) == Some("reasoning"));
    items
        .iter()
        .map(|item| {
            let ty = item.get("type").and_then(Value::as_str);
            let strip = ty == Some("reasoning") || (ty == Some("message") && has_reasoning);
            let mut item = item.clone();
            if let (true, Some(obj)) = (strip, item.as_object_mut()) {
                obj.remove("id");
            }
            item
        })
        .collect()
}

/// Codex バックエンドは message の content が文字列だと 400 "Unsupported content type" を返す
/// (hermes-agent の同 adapter, #51512)。型付きパーツにしか対応していない
fn typed_message_parts(input: &mut [Value]) {
    for item in input {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let part_type = match item.get("role").and_then(Value::as_str) {
            Some("assistant") => "output_text",
            _ => "input_text",
        };
        if let Some(text) = item.get("content").and_then(Value::as_str) {
            item["content"] = json!([{"type": part_type, "text": text}]);
        }
    }
}

/// 費用計算に必要な、リクエスト側の情報
pub struct CostContext<'a> {
    pub request_model: &'a str,
    pub billing: Billing,
    /// 内蔵の単価表を引くか。互換サーバーはモデル ID が同じでも価格が違うので引かない
    pub use_table: bool,
    pub overrides: &'a Overrides,
}

fn cost_record(resp: &Value, ctx: &CostContext, counts: TokenCounts) -> CostRecord {
    let model = resp
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .unwrap_or(ctx.request_model);
    // 定額プランは枠を消費するだけで、従量の金額は発生しない
    let amount_usd = match ctx.billing {
        Billing::Metered => {
            pricing::lookup(model, counts.prompt_tokens(), ctx.use_table, ctx.overrides)
                .map(|rates| pricing::amount_usd(&rates, &counts))
        }
        _ => None,
    };
    CostRecord {
        source: "provider-openai".into(),
        model: model.to_string(),
        // Core の計測が呼び出し元の値で上書きする
        purpose: Purpose::Main,
        run_id: None,
        input_tokens: counts.input,
        cache_read_tokens: counts.cache_read,
        cache_write_tokens: counts.cache_write,
        output_tokens: counts.output,
        unit: None,
        amount_usd,
        billing: ctx.billing,
    }
}

/// モデルは不正な JSON を返すことがある。落とさず `_raw` に入れ、tool 側のエラーとして モデルに返す
fn parse_arguments(args: &Value, name: &str) -> Value {
    match args {
        Value::String(s) if s.trim().is_empty() => json!({}),
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| {
            tracing::warn!(tool = name, "tool 引数が JSON として不正です");
            json!({"_raw": s})
        }),
        Value::Null => json!({}),
        // 互換サーバーには文字列ではなくオブジェクトで返すものがある
        other => other.clone(),
    }
}

fn u64_at(v: &Value, pointer: &str) -> u64 {
    v.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

pub fn parse_chat_response(resp: &Value, cost_ctx: &CostContext) -> Result<Completion> {
    let choice = resp
        .pointer("/choices/0")
        .ok_or_else(|| Error::Other("openai: 応答に choices がありません".into()))?;
    let message = choice
        .get("message")
        .ok_or_else(|| Error::Other("openai: 応答に message がありません".into()))?;
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string);
    let tool_calls: Vec<ToolCall> = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| {
            let name = c.pointer("/function/name").and_then(Value::as_str);
            let name = name.unwrap_or_default().to_string();
            let input = parse_arguments(
                c.pointer("/function/arguments").unwrap_or(&Value::Null),
                &name,
            );
            ToolCall {
                id: str_at(c, "id").to_string(),
                name,
                input,
            }
        })
        .collect();

    let finish = choice.get("finish_reason").and_then(Value::as_str);
    if matches!(finish, Some("length" | "content_filter"))
        || message.get("refusal").is_some_and(|r| !r.is_null())
    {
        tracing::warn!(finish_reason = ?finish, "openai の応答が正常終了ではありません");
    }
    if finish == Some("length") && text.is_none() && tool_calls.is_empty() {
        return Err(Error::Other(
            "openai: 出力が上限に達し、本文が得られませんでした".into(),
        ));
    }

    let prompt = u64_at(resp, "/usage/prompt_tokens");
    let cached = u64_at(resp, "/usage/prompt_tokens_details/cached_tokens");
    let written = u64_at(resp, "/usage/prompt_tokens_details/cache_write_tokens");
    let output = u64_at(resp, "/usage/completion_tokens");
    let counts = TokenCounts::from_reported(prompt, cached, written, output);
    Ok(Completion {
        text,
        tool_calls,
        usage: Usage {
            // prompt_tokens はキャッシュ分を含む
            input_tokens: prompt,
            cached_tokens: cached,
            output_tokens: output,
        },
        raw_assistant: None,
        cost: Some(cost_record(resp, cost_ctx, counts)),
    })
}

pub fn parse_responses_response(resp: &Value, cost_ctx: &CostContext) -> Result<Completion> {
    let output = resp
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Other("openai: 応答に output 配列がありません".into()))?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    // Codex 系は途中経過の message(phase: commentary)と最終回答(final_answer)を別の item で返すことがある。
    // 全部つなげると同じ文面が二重になり、Ack と Goal が Layer 3 と Worker にも二重に渡る
    let is_message = |item: &Value| item.get("type").and_then(Value::as_str) == Some("message");
    fn phase(item: &Value) -> Option<&str> {
        item.get("phase").and_then(Value::as_str)
    }
    let has_final = output
        .iter()
        .any(|i| is_message(i) && phase(i) == Some("final_answer"));
    let mut seen_messages: Vec<String> = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if has_final && phase(item) != Some("final_answer") {
                    continue;
                }
                let mut item_text = String::new();
                let parts = item.get("content").and_then(Value::as_array);
                for part in parts.into_iter().flatten() {
                    // output_text が正だが、互換サーバー向けに text 型も受ける
                    if matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("output_text" | "text")
                    ) {
                        item_text.push_str(str_at(part, "text"));
                    }
                }
                if !item_text.trim().is_empty() {
                    if seen_messages.contains(&item_text) {
                        continue;
                    }
                    seen_messages.push(item_text.clone());
                }
                text.push_str(&item_text);
            }
            Some("function_call") => {
                let name = str_at(item, "name").to_string();
                let id = match str_at(item, "call_id") {
                    "" => str_at(item, "id"),
                    id => id,
                };
                tool_calls.push(ToolCall {
                    id: id.to_string(),
                    input: parse_arguments(item.get("arguments").unwrap_or(&Value::Null), &name),
                    name,
                });
            }
            _ => {}
        }
    }

    if resp.get("status").and_then(Value::as_str) == Some("incomplete") {
        let reason = resp
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            .unwrap_or("不明");
        tracing::warn!(reason, "openai の応答が途中で打ち切られました");
        if text.trim().is_empty() {
            return Err(Error::Other(format!(
                "openai: 応答が途中で打ち切られました(理由: {reason})"
            )));
        }
    }

    let text = (!text.trim().is_empty()).then_some(text);
    let raw_assistant = (!tool_calls.is_empty()).then(|| Value::Array(output.clone()));
    let prompt = u64_at(resp, "/usage/input_tokens");
    let cached = u64_at(resp, "/usage/input_tokens_details/cached_tokens");
    let written = u64_at(resp, "/usage/input_tokens_details/cache_write_tokens");
    let output = u64_at(resp, "/usage/output_tokens");
    let counts = TokenCounts::from_reported(prompt, cached, written, output);
    Ok(Completion {
        text,
        tool_calls,
        usage: Usage {
            input_tokens: prompt,
            cached_tokens: cached,
            output_tokens: output,
        },
        raw_assistant,
        cost: Some(cost_record(resp, cost_ctx, counts)),
    })
}

/// Codex バックエンドは SSE 必須で、max_output_tokens も受け付けない。
/// store: false の tool 往復で reasoning を replay するには encrypted_content を明示的に要求する必要がある
pub fn build_codex_request(
    model: &str,
    prompt: &LayeredPrompt,
    params: &CompletionParams,
    effort: Option<&str>,
) -> Result<Value> {
    let mut body = build_responses_request(model, prompt, params, effort)?;
    let key = codex_cache_key(&body);
    let obj = body.as_object_mut().expect("リクエストは常にオブジェクト");
    obj.remove("max_output_tokens");
    obj.insert("stream".into(), true.into());
    if let Some(input) = obj.get_mut("input").and_then(Value::as_array_mut) {
        typed_message_parts(input);
    }
    obj.insert("prompt_cache_key".into(), key.into());
    // 要約を求めると、長く考える間も reasoning_summary のイベントが流れて idle timeout に掛からない
    // (hermes-agent の codex transport も Codex バックエンドへ summary: "auto" を送っている)
    let reasoning = obj.entry("reasoning").or_insert_with(|| json!({}));
    reasoning["summary"] = "auto".into();
    Ok(body)
}

/// instructions と tools が同じ間は同じ値になる。キャッシュのプレフィックスを決めるのがこの2つだから。
/// 暗号用途ではないので FNV-1a で足りる
pub fn codex_cache_key(body: &Value) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    let parts = [
        body["instructions"].to_string(),
        body.get("tools").map(Value::to_string).unwrap_or_default(),
    ];
    for part in &parts {
        for b in part.bytes().chain(std::iter::once(0)) {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("noth-{hash:016x}")
}

/// ストリーム中のエラーを原因で分ける。上限到達は待っても直らず、サーバー側の一時的な失敗は再試行で直る
fn sse_error(event: &Value) -> Error {
    let nested = event
        .get("response")
        .and_then(|r| r.get("error"))
        .or_else(|| event.get("error"));
    let message = [nested.and_then(|e| e.get("message")), event.get("message")]
        .into_iter()
        .flatten()
        .find_map(Value::as_str)
        .unwrap_or("詳細不明");
    let code = [nested.and_then(|e| e.get("code")), event.get("code")]
        .into_iter()
        .flatten()
        .find_map(Value::as_str)
        .unwrap_or_default();
    let one_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let text =
        format!("openai: Codex のストリームがエラーを返しました: {one_line}(コード: {code})");
    let hint = format!("{code} {one_line}").to_lowercase();
    if ["quota", "usage_limit", "billing", "credit"]
        .iter()
        .any(|w| hint.contains(w))
    {
        Error::Api {
            status: 429,
            body: text,
        }
    } else if ["server_error", "overloaded", "rate_limit", "unavailable"]
        .iter()
        .any(|w| hint.contains(w))
    {
        Error::Transport(text)
    } else {
        Error::Other(text)
    }
}

/// SSE 全文から Completion を組み立てる。terminal の output は null や空のことがあるため、
/// 本文と tool call は `output_item.done` を正とし、無いときだけ terminal の output に頼る
pub fn assemble_sse(text: &str, cost_ctx: &CostContext) -> Result<Completion> {
    let mut items: Vec<Value> = Vec::new();
    let mut terminal: Option<Value> = None;
    let mut data_lines: Vec<&str> = Vec::new();
    // 空行がイベントの区切り。末尾に空行が無い応答も拾うため、最後に番兵の空行を足す
    for line in text.lines().chain(std::iter::once("")) {
        if let Some(d) = line.strip_prefix("data:") {
            data_lines.push(d.trim_start());
            continue;
        }
        if !line.is_empty() || data_lines.is_empty() {
            continue;
        }
        let data = data_lines.join("\n");
        data_lines.clear();
        if data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(&data) else {
            tracing::debug!("SSE の data が JSON ではないため読み飛ばします");
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    items.push(item.clone());
                }
            }
            Some("response.completed" | "response.incomplete") => {
                terminal = event.get("response").cloned();
            }
            Some("response.failed" | "error") => {
                return Err(sse_error(&event));
            }
            _ => {}
        }
    }
    // 途中で切れたストリームの item は tool call の引数が欠けている恐れがあり、使えない
    let Some(terminal) = terminal else {
        return Err(Error::Transport(
            "openai: Codex のストリームが完了イベントなしで終わりました".into(),
        ));
    };
    if let (true, Some(output)) = (
        items.is_empty(),
        terminal.get("output").and_then(Value::as_array),
    ) {
        items = output.clone();
    }
    let mut synthetic = json!({"output": items});
    for key in ["status", "incomplete_details", "usage", "model"] {
        if let Some(v) = terminal.get(key) {
            synthetic[key] = v.clone();
        }
    }
    parse_responses_response(&synthetic, cost_ctx)
}

#[cfg(test)]
mod tests {
    use noth_core::types::{ToolResult, ToolSpec, TurnKind};

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
            active_log: vec![turn(Role::User, "a"), turn(Role::User, "b")],
            ephemeral_tail: vec![turn(Role::Assistant, "c"), turn(Role::User, "d")],
            tools: vec![ToolSpec {
                name: "web_search".into(),
                description: "検索".into(),
                input_schema: json!({"type": "object"}),
            }],
            ..Default::default()
        }
    }

    fn params() -> CompletionParams {
        CompletionParams {
            max_tokens: 100,
            ..Default::default()
        }
    }

    const NO_OVERRIDES: Overrides = Overrides {
        input: None,
        cached_input: None,
        cache_write: None,
        output: None,
    };

    fn cx() -> CostContext<'static> {
        CostContext {
            request_model: "gpt-5.4",
            billing: Billing::Metered,
            use_table: true,
            overrides: &NO_OVERRIDES,
        }
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: "web_search".into(),
            input: json!({"q": "x"}),
        }
    }

    fn results() -> ExchangeItem {
        ExchangeItem::ToolResults(vec![ToolResult {
            call_id: "call_1".into(),
            name: "web_search".into(),
            output: "結果".into(),
            is_error: false,
        }])
    }

    #[test]
    fn effort_validation() {
        for e in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            assert_eq!(parse_effort(e).unwrap(), e);
        }
        assert!(parse_effort("ultra").is_err());
        assert_eq!(Format::parse("chat").unwrap(), Format::Chat);
        assert_eq!(Format::parse("responses").unwrap(), Format::Responses);
        assert!(Format::parse("x").is_err());
    }

    #[test]
    fn chat_request_basics() {
        let body = build_chat_request("m", &prompt(), &params(), Some("high"), false).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(
            msgs[0],
            json!({"role": "system", "content": "rules\n\nprofile\n\ndigest"})
        );
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[1], json!({"role": "user", "content": "a\n\nb"}));
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[3]["content"], "d");
        assert_eq!(body["max_completion_tokens"], 100);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("tool_choice").is_none());
        assert_eq!(
            body["tools"][0],
            json!({"type": "function", "function": {"name": "web_search", "description": "検索", "parameters": {"type": "object"}}})
        );
    }

    #[test]
    fn memory_sits_between_profile_and_digest() {
        let mut p = prompt();
        p.memory = "memory".into();
        let body = build_chat_request("m", &p, &params(), None, true).unwrap();
        assert_eq!(
            body["messages"][0]["content"],
            "rules\n\nprofile\n\nmemory\n\ndigest"
        );
    }

    #[test]
    fn chat_request_compat_and_empty_digest() {
        let mut p = prompt();
        p.episodic_digest.clear();
        p.tools.clear();
        let body = build_chat_request("m", &p, &params(), None, true).unwrap();
        assert_eq!(body["messages"][0]["content"], "rules\n\nprofile");
        assert_eq!(body["max_tokens"], 100);
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn chat_tool_exchange() {
        let mut p = prompt();
        p.tool_exchange = vec![
            ExchangeItem::Assistant {
                text: None,
                calls: vec![call()],
                raw: None,
            },
            results(),
        ];
        let body = build_chat_request("m", &p, &params(), None, false).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        let a = &msgs[4];
        assert_eq!(a["role"], "assistant");
        assert!(a["content"].is_null());
        assert_eq!(a["tool_calls"][0]["id"], "call_1");
        assert_eq!(a["tool_calls"][0]["function"]["arguments"], "{\"q\":\"x\"}");
        assert_eq!(
            msgs[5],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "結果"})
        );
    }

    #[test]
    fn responses_request_basics() {
        let body = build_responses_request("m", &prompt(), &params(), Some("low")).unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["instructions"], "rules\n\nprofile\n\ndigest");
        assert_eq!(body["max_output_tokens"], 100);
        assert_eq!(body["reasoning"], json!({"effort": "low"}));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(
            input[0],
            json!({"type": "message", "role": "user", "content": "a\n\nb"})
        );
        assert_eq!(
            body["tools"][0],
            json!({"type": "function", "name": "web_search", "description": "検索", "parameters": {"type": "object"}})
        );
        assert!(body.get("tool_choice").is_none());
        let body = build_responses_request("m", &prompt(), &params(), None).unwrap();
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn responses_exchange_without_raw() {
        let mut p = prompt();
        p.tool_exchange = vec![
            ExchangeItem::Assistant {
                text: Some("調べます".into()),
                calls: vec![call()],
                raw: None,
            },
            results(),
        ];
        let body = build_responses_request("m", &p, &params(), None).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[3]["content"], "調べます");
        assert_eq!(
            input[4],
            json!({"type": "function_call", "call_id": "call_1", "name": "web_search", "arguments": "{\"q\":\"x\"}"})
        );
        assert_eq!(
            input[5],
            json!({"type": "function_call_output", "call_id": "call_1", "output": "結果"})
        );
    }

    #[test]
    fn responses_raw_output_is_replayed_verbatim() {
        let raw = json!([
            {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "abc"},
            {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "web_search", "arguments": "{}"},
        ]);
        let mut p = prompt();
        p.tool_exchange = vec![
            ExchangeItem::Assistant {
                text: Some("無視される".into()),
                calls: vec![call()],
                raw: Some(raw.clone()),
            },
            results(),
        ];
        let body = build_responses_request("m", &p, &params(), None).unwrap();
        let input = body["input"].as_array().unwrap();
        // reasoning の id だけは store: false で引けないため外れる。他は変えない
        let mut reasoning = raw[0].clone();
        reasoning.as_object_mut().unwrap().remove("id");
        assert_eq!(input[3], reasoning);
        assert_eq!(input[4], raw[1]);
        assert_eq!(input[5]["type"], "function_call_output");
        assert_eq!(input.len(), 6);
    }

    #[test]
    fn empty_prompt_is_error() {
        let p = LayeredPrompt::default();
        assert!(build_chat_request("m", &p, &params(), None, false).is_err());
        assert!(build_responses_request("m", &p, &params(), None).is_err());
    }

    #[test]
    fn parse_chat_text_and_usage() {
        let resp = json!({
            "choices": [{"message": {"role": "assistant", "content": "やあ"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 50, "completion_tokens": 7, "prompt_tokens_details": {"cached_tokens": 40}},
        });
        let c = parse_chat_response(&resp, &cx()).unwrap();
        assert_eq!(c.text.as_deref(), Some("やあ"));
        assert!(c.tool_calls.is_empty());
        assert!(c.raw_assistant.is_none());
        assert_eq!(
            c.usage,
            Usage {
                input_tokens: 50,
                cached_tokens: 40,
                output_tokens: 7
            }
        );
    }

    #[test]
    fn parse_chat_tool_calls_and_invalid_arguments() {
        let resp = json!({
            "choices": [{"message": {"content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "a", "arguments": "{\"x\":1}"}},
                {"id": "c2", "type": "function", "function": {"name": "b", "arguments": "{broken"}},
                {"id": "c3", "type": "function", "function": {"name": "c", "arguments": ""}},
            ]}, "finish_reason": "tool_calls"}],
        });
        let c = parse_chat_response(&resp, &cx()).unwrap();
        assert!(c.text.is_none());
        assert_eq!(c.tool_calls.len(), 3);
        assert_eq!(c.tool_calls[0].input, json!({"x": 1}));
        assert_eq!(c.tool_calls[1].input, json!({"_raw": "{broken"}));
        assert_eq!(c.tool_calls[2].input, json!({}));
        assert_eq!(c.usage, Usage::default());
    }

    #[test]
    fn parse_chat_errors() {
        assert!(parse_chat_response(&json!({}), &cx()).is_err());
        let resp = json!({"choices": [{"message": {"content": null}, "finish_reason": "length"}]});
        assert!(parse_chat_response(&resp, &cx()).is_err());
    }

    #[test]
    fn parse_responses_text_calls_usage() {
        let resp = json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_1", "summary": []},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "調べる"}, {"type": "text", "text": "ね"}]},
                {"type": "function_call", "id": "fc_1", "call_id": "call_9", "name": "web_search", "arguments": "{\"q\":\"y\"}"},
            ],
            "usage": {"input_tokens": 30, "input_tokens_details": {"cached_tokens": 20}, "output_tokens": 5},
        });
        let c = parse_responses_response(&resp, &cx()).unwrap();
        assert_eq!(c.text.as_deref(), Some("調べるね"));
        assert_eq!(c.tool_calls[0].id, "call_9");
        assert_eq!(c.tool_calls[0].input, json!({"q": "y"}));
        assert_eq!(c.raw_assistant.unwrap(), resp["output"]);
        assert_eq!(
            c.usage,
            Usage {
                input_tokens: 30,
                cached_tokens: 20,
                output_tokens: 5
            }
        );
    }

    #[test]
    fn parse_responses_no_raw_without_calls() {
        let resp = json!({"output": [{"type": "message", "content": [{"type": "output_text", "text": "hi"}]}]});
        let c = parse_responses_response(&resp, &cx()).unwrap();
        assert!(c.raw_assistant.is_none());
        assert!(parse_responses_response(&json!({}), &cx()).is_err());
    }

    #[test]
    fn parse_responses_incomplete() {
        let empty = json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": [{"type": "reasoning", "id": "rs"}]});
        let err = parse_responses_response(&empty, &cx())
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_output_tokens"), "{err}");
        let partial = json!({"status": "incomplete", "output": [{"type": "message", "content": [{"type": "output_text", "text": "途中"}]}]});
        assert_eq!(
            parse_responses_response(&partial, &cx())
                .unwrap()
                .text
                .as_deref(),
            Some("途中")
        );
    }

    const SSE: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{}}\n\n\
event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"zz\"}}\n\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"こんにちは\"}]}}\n\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_7\",\"name\":\"web_search\",\"arguments\":\"{\\\"q\\\":\\\"z\\\"}\"}}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":null,\"usage\":{\"input_tokens\":12,\"input_tokens_details\":{\"cached_tokens\":8},\"output_tokens\":3}}}\n\n\
data: [DONE]\n";

    #[test]
    fn codex_request_is_streaming_without_max_tokens() {
        let body = build_codex_request("m", &prompt(), &params(), Some("low")).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["instructions"], "rules\n\nprofile\n\ndigest");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(
            body["reasoning"],
            json!({"effort": "low", "summary": "auto"})
        );
        assert!(body.get("tool_choice").is_none());
        let key = body["prompt_cache_key"].as_str().unwrap();
        assert_eq!(key, codex_cache_key(&body));
        let mut other = prompt();
        other.profile = "changed".into();
        let body2 = build_codex_request("m", &other, &params(), Some("low")).unwrap();
        assert_ne!(key, body2["prompt_cache_key"].as_str().unwrap());
    }

    #[test]
    fn assemble_sse_text_call_usage() {
        let c = assemble_sse(SSE, &cx()).unwrap();
        assert_eq!(c.text.as_deref(), Some("こんにちは"));
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].id, "call_7");
        assert_eq!(c.tool_calls[0].input, json!({"q": "z"}));
        assert_eq!(
            c.usage,
            Usage {
                input_tokens: 12,
                cached_tokens: 8,
                output_tokens: 3
            }
        );
        let raw = c.raw_assistant.unwrap();
        assert_eq!(raw.as_array().unwrap().len(), 3);
        assert_eq!(raw[0]["encrypted_content"], "zz");
    }

    /// delta・text.done・output_item.done・completed の全部が同じ本文を運ぶ実際の流れ
    fn sse_transcript(items: &[(&str, &str)]) -> String {
        let mut out = String::from(
            "data: {\"type\":\"response.created\",\"response\":{}}\n\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"A\"}\n\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"ck\"}\n\n\
data: {\"type\":\"response.output_text.done\",\"text\":\"Ack\"}\n\n",
        );
        let mut output = Vec::new();
        for (phase, text) in items {
            let item = json!({"type": "message", "phase": phase, "content": [{"type": "output_text", "text": text}]});
            out.push_str(&format!(
                "data: {}\n\n",
                json!({"type": "response.output_item.done", "item": item})
            ));
            output.push(item);
        }
        out.push_str(&format!(
            "data: {}\n\n",
            json!({"type": "response.completed", "response": {"status": "completed", "output": output, "usage": {"input_tokens": 1, "output_tokens": 1}}})
        ));
        out
    }

    #[test]
    fn assemble_sse_ignores_reasoning_summary_events_and_text() {
        let sse = "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"考え中\"}\n\n\
data: {\"type\":\"response.reasoning_summary_text.done\",\"text\":\"考え中\"}\n\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"考え中\"}]}}\n\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"答え\"}]}}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
        assert_eq!(
            assemble_sse(sse, &cx()).unwrap().text.as_deref(),
            Some("答え")
        );
    }

    #[test]
    fn assemble_sse_single_message_is_not_doubled() {
        let sse = sse_transcript(&[("final_answer", "Ack\n[Goal]\ng")]);
        let c = assemble_sse(&sse, &cx()).unwrap();
        assert_eq!(c.text.as_deref(), Some("Ack\n[Goal]\ng"));
    }

    #[test]
    fn assemble_sse_ignores_commentary_when_final_answer_exists() {
        let sse = sse_transcript(&[
            ("commentary", "Ack\n[Goal]\ng"),
            ("final_answer", "Ack\n[Goal]\ng"),
        ]);
        let c = assemble_sse(&sse, &cx()).unwrap();
        assert_eq!(c.text.as_deref(), Some("Ack\n[Goal]\ng"));
    }

    #[test]
    fn assemble_sse_drops_identical_repeated_messages() {
        let sse = sse_transcript(&[("", "Ack\n[Goal]\ng"), ("", "Ack\n[Goal]\ng")]);
        let c = assemble_sse(&sse, &cx()).unwrap();
        assert_eq!(c.text.as_deref(), Some("Ack\n[Goal]\ng"));
    }

    #[test]
    fn assemble_sse_keeps_distinct_messages() {
        let sse = sse_transcript(&[("", "a"), ("", "b")]);
        let c = assemble_sse(&sse, &cx()).unwrap();
        assert_eq!(c.text.as_deref(), Some("ab"));
    }

    #[test]
    fn assemble_sse_falls_back_to_terminal_output() {
        let sse = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}]}}";
        assert_eq!(
            assemble_sse(sse, &cx()).unwrap().text.as_deref(),
            Some("hi")
        );
    }

    #[test]
    fn assemble_sse_errors() {
        let failed = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"boom\"}}}\n\n";
        assert!(
            assemble_sse(failed, &cx())
                .unwrap_err()
                .to_string()
                .contains("boom")
        );
        let err = "data: {\"type\":\"error\",\"message\":\"bad\"}\n\n";
        assert!(
            assemble_sse(err, &cx())
                .unwrap_err()
                .to_string()
                .contains("bad")
        );
        assert!(assemble_sse("", &cx()).is_err());
    }

    #[test]
    fn codex_messages_use_typed_parts() {
        let mut p = prompt();
        p.tool_exchange = vec![ExchangeItem::Assistant {
            text: Some("調べます".into()),
            calls: vec![call()],
            raw: None,
        }];
        let body = build_codex_request("m", &p, &params(), None).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(
            input[0]["content"],
            json!([{"type": "input_text", "text": "a\n\nb"}])
        );
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert!(input.iter().all(|i| !i["content"].is_string()));
        let plain = build_responses_request("m", &p, &params(), None).unwrap();
        assert!(plain["input"][0]["content"].is_string());
    }

    #[test]
    fn responses_requests_include_encrypted_reasoning() {
        let body = build_responses_request("m", &prompt(), &params(), None).unwrap();
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        let body = build_codex_request("m", &prompt(), &params(), None).unwrap();
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn replay_strips_reasoning_and_linked_message_ids() {
        let raw = json!([
            {"type": "reasoning", "id": "rs_1", "encrypted_content": "abc"},
            {"type": "message", "id": "msg_1", "role": "assistant", "content": []},
            {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "n", "arguments": "{}"},
        ]);
        let out = replayable_output(raw.as_array().unwrap());
        assert!(out[0].get("id").is_none());
        assert_eq!(out[0]["encrypted_content"], "abc");
        assert!(out[1].get("id").is_none());
        assert_eq!(out[2]["id"], "fc_1");
        assert_eq!(out[2]["call_id"], "call_1");
        let no_reasoning = json!([{"type": "message", "id": "msg_2", "content": []}]);
        assert_eq!(
            replayable_output(no_reasoning.as_array().unwrap())[0]["id"],
            "msg_2"
        );
    }

    #[test]
    fn assemble_sse_requires_terminal_event() {
        let sse = "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"x\"}]}}\n\n";
        assert!(assemble_sse(sse, &cx()).is_err());
    }

    #[test]
    fn sse_error_message_is_one_line() {
        let e = "data: {\"type\":\"error\",\"message\":\"a\\nb   c\"}\n\n";
        let msg = assemble_sse(e, &cx()).unwrap_err().to_string();
        assert!(!msg.contains('\n') && msg.contains("a b c"), "{msg}");
    }

    #[test]
    fn sse_errors_are_classified_by_code() {
        let ev = |code: &str, msg: &str| {
            format!(
                "data: {{\"type\":\"response.failed\",\"response\":{{\"error\":{{\"code\":\"{code}\",\"message\":\"{msg}\"}}}}}}\n\n"
            )
        };
        let kind = |code: &str, msg: &str| assemble_sse(&ev(code, msg), &cx()).unwrap_err().kind();
        use noth_core::error::ErrorKind;
        assert_eq!(kind("usage_limit_reached", "limit"), ErrorKind::Quota);
        assert_eq!(kind("insufficient_quota", "x"), ErrorKind::Quota);
        assert_eq!(kind("server_error", "oops"), ErrorKind::Unavailable);
        assert_eq!(kind("rate_limit_exceeded", "slow"), ErrorKind::Unavailable);
        assert_eq!(kind("invalid_prompt", "bad"), ErrorKind::Other);
        let err = assemble_sse(&ev("server_error", "oops"), &cx()).unwrap_err();
        assert!(matches!(err, Error::Transport(_)));
    }

    fn cost_cx<'a>(billing: Billing, use_table: bool, o: &'a Overrides) -> CostContext<'a> {
        CostContext {
            request_model: "gpt-5.4",
            billing,
            use_table,
            overrides: o,
        }
    }

    fn chat_resp(model: &str) -> Value {
        json!({
            "model": model,
            "choices": [{"message": {"content": "x"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 200000, "completion_tokens": 1000000, "prompt_tokens_details": {"cached_tokens": 100000}},
        })
    }

    #[test]
    fn chat_cost_splits_cached_and_uses_response_model() {
        let c = parse_chat_response(&chat_resp("gpt-5.4-2026-03-05"), &cx())
            .unwrap()
            .cost
            .unwrap();
        assert_eq!(c.source, "provider-openai");
        assert_eq!(c.model, "gpt-5.4-2026-03-05");
        assert_eq!((c.input_tokens, c.cache_read_tokens), (100_000, 100_000));
        assert_eq!(c.billing, Billing::Metered);
        // 0.1M*2.5 + 0.1M*0.25 + 1M*15
        assert!((c.amount_usd.unwrap() - (0.25 + 0.025 + 15.0)).abs() < 1e-9);
    }

    #[test]
    fn responses_cost_long_context_and_cache_write() {
        let resp = json!({
            "model": "gpt-6-sol",
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "x"}]}],
            "usage": {"input_tokens": 300000, "output_tokens": 1000000,
                "input_tokens_details": {"cached_tokens": 100000, "cache_write_tokens": 50000}},
        });
        let c = parse_responses_response(&resp, &cx())
            .unwrap()
            .cost
            .unwrap();
        assert_eq!(c.cache_write_tokens, 50_000);
        assert_eq!(c.input_tokens, 150_000);
        // 272K 超なので long 単価: 0.15M*4 + 0.1M*0.4 + 0.05M*5 + 1M*15
        assert!((c.amount_usd.unwrap() - (0.6 + 0.04 + 0.25 + 15.0)).abs() < 1e-9);
    }

    #[test]
    fn compatible_server_has_no_amount_unless_overridden() {
        let none = Overrides::default();
        let c = parse_chat_response(
            &chat_resp("gpt-5.4"),
            &cost_cx(Billing::Metered, false, &none),
        )
        .unwrap()
        .cost
        .unwrap();
        assert_eq!(c.amount_usd, None);
        assert_eq!(c.input_tokens, 100_000);
        let o = Overrides {
            input: Some(1.0),
            output: Some(2.0),
            ..Default::default()
        };
        let c = parse_chat_response(&chat_resp("kimi"), &cost_cx(Billing::Metered, false, &o))
            .unwrap()
            .cost
            .unwrap();
        // 0.1M*1 + 0.1M*1(cached は input に揃う) + 1M*2
        assert!((c.amount_usd.unwrap() - 2.2).abs() < 1e-9);
    }

    #[test]
    fn unknown_model_has_no_amount() {
        let c = parse_chat_response(&chat_resp("mystery-1"), &cx())
            .unwrap()
            .cost
            .unwrap();
        assert_eq!(c.amount_usd, None);
        assert_eq!(c.output_tokens, 1_000_000);
    }

    #[test]
    fn codex_is_subscription_without_amount() {
        let none = Overrides::default();
        let cx = cost_cx(Billing::Subscription, false, &none);
        let c = assemble_sse(SSE, &cx).unwrap().cost.unwrap();
        assert_eq!(c.billing, Billing::Subscription);
        assert_eq!(c.amount_usd, None);
        assert_eq!(
            (c.input_tokens, c.cache_read_tokens, c.output_tokens),
            (4, 8, 3)
        );
    }

    #[test]
    fn overrides_apply_to_table_models() {
        let o = Overrides {
            output: Some(100.0),
            ..Default::default()
        };
        let c = parse_chat_response(&chat_resp("gpt-5.4"), &cost_cx(Billing::Metered, true, &o))
            .unwrap()
            .cost
            .unwrap();
        assert!((c.amount_usd.unwrap() - (0.25 + 0.025 + 100.0)).abs() < 1e-9);
    }
}
