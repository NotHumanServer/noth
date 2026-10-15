use noth_core::traits::{Completion, LayeredPrompt};
use noth_core::types::{
    Billing, CompletionParams, CostRecord, ExchangeItem, Purpose, Role, ToolCall, Turn, Usage,
};
use noth_core::{Error, Result};
use serde_json::{Value, json};

use crate::pricing::{self, Overrides, TokenCounts};

/// 先頭が assistant になるとき API が弾くため補う
const OMITTED_NOTICE: &str = "[以前の会話は省略されています]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTtl {
    FiveMinutes,
    OneHour,
}

impl CacheTtl {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "5m" => Ok(Self::FiveMinutes),
            "1h" => Ok(Self::OneHour),
            other => Err(Error::Config(format!(
                "provider.anthropic: cache_ttl は \"5m\" か \"1h\" で指定してください(`{other}` が指定されています)"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }

    fn cache_control(self) -> Value {
        json!({"type": "ephemeral", "ttl": self.as_str()})
    }
}

/// 省略時はモデルの既定値(Sonnet 5.5 は high)になる。未知の値を API に投げて実行時に 400 で気づくより、起動時に弾く
pub fn parse_effort(s: &str) -> Result<&'static str> {
    match s {
        "low" => Ok("low"),
        "medium" => Ok("medium"),
        "high" => Ok("high"),
        "xhigh" => Ok("xhigh"),
        "max" => Ok("max"),
        other => Err(Error::Config(format!(
            "provider.anthropic: effort は low / medium / high / xhigh / max のどれかで指定してください(`{other}` が指定されています)"
        ))),
    }
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

struct Message {
    role: Role,
    blocks: Vec<Value>,
    /// 他の block と混ぜないメッセージ。raw の assistant content(thinking の署名を守る)と tool_result が該当
    raw: bool,
}

fn tool_use_block(call: &ToolCall) -> Value {
    json!({"type": "tool_use", "id": call.id, "name": call.name, "input": call.input})
}

/// 空の tool_result は API が弾く(特に is_error: true)
const EMPTY_OUTPUT_NOTICE: &str = "(出力なし)";

fn is_blank_text(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
        && block
            .get("text")
            .and_then(Value::as_str)
            .is_none_or(|t| t.trim().is_empty())
}

fn push_exchange(messages: &mut Vec<Message>, item: &ExchangeItem) {
    match item {
        ExchangeItem::Assistant { text, calls, raw } => {
            // 空白だけの text ブロックは再送すると 400 になる。thinking など他のブロックはそのまま返す
            let raw_blocks: Vec<Value> = match raw {
                Some(Value::Array(blocks)) => blocks
                    .iter()
                    .filter(|b| !is_blank_text(b))
                    .cloned()
                    .collect(),
                Some(other) => vec![other.clone()],
                None => Vec::new(),
            };
            let from_raw = !raw_blocks.is_empty();
            let blocks = if from_raw {
                raw_blocks
            } else {
                text.iter()
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| text_block(t))
                    .chain(calls.iter().map(tool_use_block))
                    .collect()
            };
            if blocks.is_empty() {
                return;
            }
            match messages.last_mut() {
                Some(m) if m.role == Role::Assistant && !m.raw && !from_raw => {
                    m.blocks.extend(blocks);
                }
                _ => messages.push(Message {
                    role: Role::Assistant,
                    blocks,
                    raw: from_raw,
                }),
            }
        }
        ExchangeItem::ToolResults(results) => {
            let blocks: Vec<Value> = results
                .iter()
                .map(|r| {
                    let output = if r.output.is_empty() {
                        EMPTY_OUTPUT_NOTICE
                    } else {
                        &r.output
                    };
                    json!({
                        "type": "tool_result",
                        "tool_use_id": r.call_id,
                        "content": output,
                        "is_error": r.is_error,
                    })
                })
                .collect();
            if blocks.is_empty() {
                return;
            }
            // tool_result は直前の assistant の tool_use に続く user メッセージの先頭になければならないため、他の user 内容とは混ぜない
            messages.push(Message {
                role: Role::User,
                blocks,
                raw: true,
            });
        }
    }
}

/// thinking ブロックには cache_control を付けられないため、末尾から遡って最初の非 thinking ブロックを探す
fn last_cacheable(messages: &[Message]) -> Option<(usize, usize)> {
    messages.iter().enumerate().rev().find_map(|(mi, m)| {
        m.blocks
            .iter()
            .rposition(|b| {
                !matches!(
                    b.get("type").and_then(Value::as_str),
                    Some("thinking" | "redacted_thinking")
                )
            })
            .map(|bi| (mi, bi))
    })
}

/// 連続する同 role のターンは1メッセージの複数 block に畳む。追加した block の位置を返す
fn push_turn(messages: &mut Vec<Message>, turn: &Turn) -> Option<(usize, usize)> {
    // 空 text の block は API が拒否する
    if turn.text.is_empty() {
        return None;
    }
    match messages.last_mut() {
        Some(m) if m.role == turn.role && !m.raw => m.blocks.push(text_block(&turn.text)),
        _ => messages.push(Message {
            role: turn.role,
            blocks: vec![text_block(&turn.text)],
            raw: false,
        }),
    }
    let mi = messages.len() - 1;
    Some((mi, messages[mi].blocks.len() - 1))
}

pub fn build_request(
    model: &str,
    prompt: &LayeredPrompt,
    params: &CompletionParams,
    ttl: CacheTtl,
) -> Result<Value> {
    let mut system = vec![text_block(&format!(
        "{}\n\n{}",
        prompt.harness_rules, prompt.profile
    ))];
    system[0]["cache_control"] = ttl.cache_control();
    if !prompt.episodic_digest.is_empty() {
        let mut block = text_block(&prompt.episodic_digest);
        block["cache_control"] = ttl.cache_control();
        system.push(block);
    }

    let mut messages: Vec<Message> = Vec::new();
    let mut layer3_end = None;
    for turn in &prompt.active_log {
        layer3_end = push_turn(&mut messages, turn).or(layer3_end);
    }
    for turn in &prompt.ephemeral_tail {
        push_turn(&mut messages, turn);
    }
    for item in &prompt.tool_exchange {
        push_exchange(&mut messages, item);
    }

    match messages.last() {
        None => return Err(Error::Other("anthropic: messages is empty".into())),
        Some(m) if m.role != Role::User => {
            return Err(Error::Other(
                "anthropic: 最後のメッセージが user ではありません".into(),
            ));
        }
        Some(_) => {}
    }
    // Worker は tool 往復のたびに末尾が伸びるので、境界を末尾へ動かして直前までの往復をキャッシュに乗せる
    let breakpoint = if prompt.cache_to_end {
        last_cacheable(&messages)
    } else {
        layer3_end
    };
    if let Some((mi, bi)) = breakpoint {
        messages[mi].blocks[bi]["cache_control"] = ttl.cache_control();
    }
    if messages[0].role == Role::Assistant {
        messages.insert(
            0,
            Message {
                role: Role::User,
                blocks: vec![text_block(OMITTED_NOTICE)],
                raw: false,
            },
        );
    }

    let messages: Vec<Value> = messages
        .into_iter()
        .map(|m| json!({"role": role_str(m.role), "content": m.blocks}))
        .collect();
    let mut body = json!({
        "model": model,
        "max_tokens": params.max_tokens,
        "system": system,
        "messages": messages,
    });
    // キャッシュのプレフィックスは tools → system → messages の順なので、system の境界が tools も覆う
    if !prompt.tools.is_empty() {
        body["tools"] = prompt
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.input_schema}))
            .collect();
    }
    Ok(body)
}

/// 費用計算に必要な、リクエスト側の情報
pub struct CostContext<'a> {
    pub request_model: &'a str,
    pub ttl: CacheTtl,
    pub overrides: &'a Overrides,
}

pub fn parse_response(resp: &Value, cost_ctx: &CostContext) -> Result<Completion> {
    let blocks = resp
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Other("anthropic: response has no content array".into()))?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                let t = b.get("text").and_then(Value::as_str).unwrap_or_default();
                if !t.trim().is_empty() {
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                let field = |k: &str| {
                    b.get(k).ok_or_else(|| {
                        Error::Other(format!("anthropic: tool_use ブロックに `{k}` がありません"))
                    })
                };
                tool_calls.push(ToolCall {
                    id: field("id")?.as_str().unwrap_or_default().to_string(),
                    name: field("name")?.as_str().unwrap_or_default().to_string(),
                    input: field("input")?.clone(),
                });
            }
            _ => {}
        }
    }
    if let Some(reason) = resp.get("stop_reason").and_then(Value::as_str) {
        if matches!(reason, "max_tokens" | "refusal") {
            tracing::warn!(
                stop_reason = reason,
                "anthropic の応答が正常終了ではありません"
            );
        } else {
            tracing::debug!(stop_reason = reason, "anthropic completion");
        }
    }

    let u = |k: &str| {
        resp.pointer(&format!("/usage/{k}"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let cache_read = u("cache_read_input_tokens");
    // input_tokens はキャッシュ分を含まないため、プロンプト全体の上限判定用に合算する
    let usage = Usage {
        input_tokens: u("input_tokens") + cache_read + u("cache_creation_input_tokens"),
        cached_tokens: cache_read,
        output_tokens: u("output_tokens"),
    };
    let cost = Some(cost_record(resp, cost_ctx, &u));
    let raw_assistant = (!tool_calls.is_empty()).then(|| Value::Array(blocks.clone()));
    Ok(Completion {
        text: (!text.is_empty()).then_some(text),
        tool_calls,
        usage,
        raw_assistant,
        cost,
    })
}

fn cost_record(resp: &Value, ctx: &CostContext, u: &impl Fn(&str) -> u64) -> CostRecord {
    let model = resp
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(ctx.request_model);
    let write_total = u("cache_creation_input_tokens");
    // 内訳があれば実際の TTL ごとの料金で数え、無ければリクエストの cache_ttl で全量を数える
    let breakdown = |k: &str| {
        resp.pointer(&format!("/usage/cache_creation/{k}"))
            .and_then(Value::as_u64)
    };
    let (write_5m, write_1h) = match (
        breakdown("ephemeral_5m_input_tokens"),
        breakdown("ephemeral_1h_input_tokens"),
    ) {
        (None, None) => match ctx.ttl {
            CacheTtl::FiveMinutes => (write_total, 0),
            CacheTtl::OneHour => (0, write_total),
        },
        (a, b) => (a.unwrap_or(0), b.unwrap_or(0)),
    };
    let counts = TokenCounts {
        input: u("input_tokens"),
        cache_read: u("cache_read_input_tokens"),
        cache_write_5m: write_5m,
        cache_write_1h: write_1h,
        output: u("output_tokens"),
    };
    let amount_usd = pricing::lookup(model, counts.prompt_tokens(), ctx.overrides)
        .map(|rates| pricing::amount_usd(&rates, &counts));
    CostRecord {
        source: "provider-anthropic".into(),
        model: model.to_string(),
        // Core の計測が呼び出し元の値で上書きする
        purpose: Purpose::Main,
        run_id: None,
        input_tokens: counts.input,
        cache_read_tokens: counts.cache_read,
        cache_write_tokens: write_5m + write_1h,
        output_tokens: counts.output,
        unit: None,
        amount_usd,
        billing: Billing::Metered,
    }
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
            active_log: vec![turn(Role::User, "a"), turn(Role::Assistant, "b")],
            ephemeral_tail: vec![turn(Role::User, "c")],
            tools: vec![],
            ..Default::default()
        }
    }

    fn params() -> CompletionParams {
        CompletionParams {
            max_tokens: 100,
            ..Default::default()
        }
    }

    fn count_cache_controls(v: &Value) -> usize {
        match v {
            Value::Object(m) => {
                usize::from(m.contains_key("cache_control"))
                    + m.values().map(count_cache_controls).sum::<usize>()
            }
            Value::Array(a) => a.iter().map(count_cache_controls).sum(),
            _ => 0,
        }
    }

    #[test]
    fn system_blocks_and_breakpoints() {
        let body = build_request("m", &prompt(), &params(), CacheTtl::OneHour).unwrap();
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys.len(), 2);
        assert_eq!(sys[0]["text"], "rules\n\nprofile");
        assert_eq!(
            sys[0]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        assert_eq!(sys[1]["text"], "digest");
        assert_eq!(sys[1]["cache_control"]["ttl"], "1h");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["content"][0]["cache_control"]["type"], "ephemeral");
        assert!(msgs[2]["content"][0].get("cache_control").is_none());
        assert_eq!(count_cache_controls(&body), 3);
        assert_eq!(body["max_tokens"], 100);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn empty_digest_gives_one_system_block() {
        let mut p = prompt();
        p.episodic_digest.clear();
        let body = build_request("m", &p, &params(), CacheTtl::FiveMinutes).unwrap();
        assert_eq!(body["system"].as_array().unwrap().len(), 1);
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "5m");
        assert_eq!(count_cache_controls(&body), 2);
    }

    #[test]
    fn consecutive_users_are_merged_and_breakpoint_is_last_layer3_block() {
        let mut p = prompt();
        p.active_log = vec![turn(Role::User, "a"), turn(Role::User, "b")];
        p.ephemeral_tail = vec![turn(Role::User, "c")];
        let body = build_request("m", &p, &params(), CacheTtl::FiveMinutes).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert!(content[0].get("cache_control").is_none());
        assert!(content[1].get("cache_control").is_some());
        assert!(content[2].get("cache_control").is_none());
    }

    #[test]
    fn empty_layer3_has_no_message_breakpoint() {
        let mut p = prompt();
        p.active_log.clear();
        let body = build_request("m", &p, &params(), CacheTtl::FiveMinutes).unwrap();
        assert_eq!(count_cache_controls(&body["messages"]), 0);
        assert_eq!(count_cache_controls(&body), 2);
    }

    #[test]
    fn leading_assistant_gets_notice() {
        let mut p = prompt();
        p.active_log = vec![turn(Role::Assistant, "b")];
        let body = build_request("m", &p, &params(), CacheTtl::FiveMinutes).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["text"], OMITTED_NOTICE);
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["role"], "user");
    }

    #[test]
    fn last_not_user_is_error() {
        let mut p = prompt();
        p.ephemeral_tail.clear();
        assert!(build_request("m", &p, &params(), CacheTtl::FiveMinutes).is_err());
        p.active_log.clear();
        assert!(build_request("m", &p, &params(), CacheTtl::FiveMinutes).is_err());
    }

    #[test]
    fn tools_are_passed_through() {
        let mut p = prompt();
        p.tools = vec![ToolSpec {
            name: "t".into(),
            description: "d".into(),
            input_schema: json!({"type": "object"}),
        }];
        let body = build_request("m", &p, &params(), CacheTtl::FiveMinutes).unwrap();
        assert_eq!(body["tools"][0]["name"], "t");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    fn cx() -> CostContext<'static> {
        static NONE: Overrides = Overrides {
            input: None,
            cache_write_5m: None,
            cache_write_1h: None,
            cache_read: None,
            output: None,
        };
        CostContext {
            request_model: "claude-opus-5-5",
            ttl: CacheTtl::FiveMinutes,
            overrides: &NONE,
        }
    }

    fn spec() -> ToolSpec {
        ToolSpec {
            name: "shell".into(),
            description: "d".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn exchange() -> Vec<ExchangeItem> {
        vec![
            ExchangeItem::Assistant {
                text: Some("調べます".into()),
                calls: vec![ToolCall {
                    id: "toolu_1".into(),
                    name: "shell".into(),
                    input: json!({"command": "ls"}),
                }],
                raw: Some(json!([
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "text", "text": "調べます"},
                    {"type": "tool_use", "id": "toolu_1", "name": "shell", "input": {"command": "ls"}}
                ])),
            },
            ExchangeItem::ToolResults(vec![ToolResult {
                call_id: "toolu_1".into(),
                name: "shell".into(),
                output: "a.txt".into(),
                is_error: false,
            }]),
        ]
    }

    #[test]
    fn tool_exchange_round_trip_keeps_raw_content() {
        let mut p = prompt();
        p.tools = vec![spec()];
        p.tool_exchange = exchange();
        let body = build_request("m", &p, &params(), CacheTtl::FiveMinutes).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[3]["role"], "assistant");
        assert_eq!(msgs[3]["content"][0]["type"], "thinking");
        assert_eq!(msgs[3]["content"][0]["signature"], "sig");
        assert_eq!(msgs[3]["content"][2]["id"], "toolu_1");
        assert_eq!(msgs[4]["role"], "user");
        let r = &msgs[4]["content"][0];
        assert_eq!(r["type"], "tool_result");
        assert_eq!(r["tool_use_id"], "toolu_1");
        assert_eq!(r["content"], "a.txt");
        assert_eq!(r["is_error"], false);
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn exchange_without_raw_builds_blocks() {
        let mut p = prompt();
        p.tool_exchange = vec![
            ExchangeItem::Assistant {
                text: Some("x".into()),
                calls: vec![ToolCall {
                    id: "t".into(),
                    name: "shell".into(),
                    input: json!({}),
                }],
                raw: None,
            },
            ExchangeItem::ToolResults(vec![]),
            ExchangeItem::ToolResults(vec![ToolResult {
                call_id: "t".into(),
                name: "shell".into(),
                output: "e".into(),
                is_error: true,
            }]),
        ];
        let body = build_request("m", &p, &params(), CacheTtl::FiveMinutes).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[3]["content"][0]["text"], "x");
        assert_eq!(msgs[3]["content"][1]["type"], "tool_use");
        assert_eq!(msgs[4]["content"][0]["is_error"], true);
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "shell".into(),
            input: json!({}),
        }
    }

    #[test]
    fn raw_drops_blank_text_but_keeps_thinking() {
        let mut p = prompt();
        p.tool_exchange = vec![
            ExchangeItem::Assistant {
                text: None,
                calls: vec![call("t")],
                raw: Some(json!([
                    {"type": "thinking", "thinking": "x", "signature": "s"},
                    {"type": "text", "text": " \n"},
                    {"type": "tool_use", "id": "t", "name": "shell", "input": {}}
                ])),
            },
            ExchangeItem::ToolResults(vec![ToolResult {
                call_id: "t".into(),
                name: "shell".into(),
                output: "ok".into(),
                is_error: false,
            }]),
        ];
        let body = build_request("m", &p, &params(), CacheTtl::FiveMinutes).unwrap();
        let content = body["messages"][3]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[1]["type"], "tool_use");
    }

    #[test]
    fn empty_tool_output_gets_placeholder() {
        let mut p = prompt();
        p.tool_exchange = vec![
            ExchangeItem::Assistant {
                text: None,
                calls: vec![call("t")],
                raw: None,
            },
            ExchangeItem::ToolResults(vec![ToolResult {
                call_id: "t".into(),
                name: "shell".into(),
                output: String::new(),
                is_error: true,
            }]),
        ];
        let body = build_request("m", &p, &params(), CacheTtl::FiveMinutes).unwrap();
        assert_eq!(body["messages"][4]["content"][0]["content"], "(出力なし)");
        assert_eq!(body["messages"][4]["content"][0]["is_error"], true);
    }

    #[test]
    fn empty_raw_falls_back_to_calls_and_results_stay_separate() {
        let mut p = prompt();
        p.tool_exchange = vec![
            ExchangeItem::Assistant {
                text: None,
                calls: vec![call("t")],
                raw: Some(json!([])),
            },
            ExchangeItem::ToolResults(vec![ToolResult {
                call_id: "t".into(),
                name: "shell".into(),
                output: "ok".into(),
                is_error: false,
            }]),
        ];
        let body = build_request("m", &p, &params(), CacheTtl::FiveMinutes).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[3]["role"], "assistant");
        assert_eq!(msgs[3]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[4]["role"], "user");
        assert_eq!(msgs[4]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[4]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn cache_to_end_puts_breakpoint_on_last_block() {
        let mut p = prompt();
        p.tools = vec![spec()];
        p.tool_exchange = exchange();
        p.cache_to_end = true;
        let body = build_request("m", &p, &params(), CacheTtl::FiveMinutes).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert!(msgs[1]["content"][0].get("cache_control").is_none());
        assert!(msgs[4]["content"][0].get("cache_control").is_some());
        assert_eq!(count_cache_controls(&body["messages"]), 1);
        assert!(count_cache_controls(&body) <= 4);
    }

    #[test]
    fn cache_to_end_skips_thinking_blocks() {
        let msgs = vec![Message {
            role: Role::Assistant,
            blocks: vec![
                json!({"type": "text", "text": "a"}),
                json!({"type": "thinking", "thinking": "t", "signature": "s"}),
            ],
            raw: true,
        }];
        assert_eq!(last_cacheable(&msgs), Some((0, 0)));
    }

    #[test]
    fn main_prompt_keeps_layer3_breakpoint_with_tools() {
        let mut p = prompt();
        p.tools = vec![spec()];
        let body = build_request("m", &p, &params(), CacheTtl::FiveMinutes).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert!(msgs[1]["content"][0].get("cache_control").is_some());
        assert!(msgs[2]["content"][0].get("cache_control").is_none());
        assert_eq!(body["tools"][0]["name"], "shell");
        assert_eq!(count_cache_controls(&body), 3);
    }

    #[test]
    fn raw_assistant_set_only_with_tool_use() {
        let resp = json!({
            "content": [
                {"type": "thinking", "thinking": "t", "signature": "s"},
                {"type": "tool_use", "id": "a", "name": "n", "input": {}}
            ],
            "usage": {}
        });
        let c = parse_response(&resp, &cx()).unwrap();
        assert_eq!(c.raw_assistant, Some(resp["content"].clone()));
        let plain = json!({"content": [{"type":"text","text":"x"}], "usage": {}});
        assert!(
            parse_response(&plain, &cx())
                .unwrap()
                .raw_assistant
                .is_none()
        );
    }

    #[test]
    fn parses_text_tool_use_and_usage() {
        let resp = json!({
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "text", "text": "world"},
                {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {"q": "x"}}
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 10,
                "cache_read_input_tokens": 200,
                "cache_creation_input_tokens": 30,
                "output_tokens": 5
            }
        });
        let c = parse_response(&resp, &cx()).unwrap();
        assert_eq!(c.text.as_deref(), Some("hello world"));
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].id, "toolu_1");
        assert_eq!(c.tool_calls[0].input, json!({"q": "x"}));
        assert_eq!(
            c.usage,
            Usage {
                input_tokens: 240,
                cached_tokens: 200,
                output_tokens: 5
            }
        );
    }

    #[test]
    fn cost_uses_response_model_and_ttl_split() {
        let resp = json!({
            "model": "claude-opus-5-5-20261001",
            "content": [],
            "usage": {
                "input_tokens": 1_000_000,
                "cache_read_input_tokens": 1_000_000,
                "cache_creation_input_tokens": 2_000_000,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 1_000_000,
                    "ephemeral_1h_input_tokens": 1_000_000
                },
                "output_tokens": 1_000_000
            }
        });
        let c = parse_response(&resp, &cx()).unwrap().cost.unwrap();
        assert_eq!(c.source, "provider-anthropic");
        assert_eq!(c.model, "claude-opus-5-5-20261001");
        assert_eq!(c.billing, Billing::Metered);
        assert_eq!(c.cache_write_tokens, 2_000_000);
        assert!((c.amount_usd.unwrap() - (4.0 + 0.2 + 5.0 + 8.0 + 20.0)).abs() < 1e-9);
    }

    #[test]
    fn cost_without_breakdown_follows_cache_ttl() {
        let resp = json!({
            "content": [],
            "usage": {"cache_creation_input_tokens": 1_000_000}
        });
        let mut cx1 = cx();
        let five = parse_response(&resp, &cx1).unwrap().cost.unwrap();
        assert_eq!(five.model, "claude-opus-5-5");
        assert!((five.amount_usd.unwrap() - 5.0).abs() < 1e-9);
        cx1.ttl = CacheTtl::OneHour;
        let hour = parse_response(&resp, &cx1).unwrap().cost.unwrap();
        assert!((hour.amount_usd.unwrap() - 8.0).abs() < 1e-9);
    }

    #[test]
    fn cost_unknown_model_has_no_amount() {
        let resp = json!({"model": "mystery", "content": [], "usage": {"input_tokens": 5}});
        let c = parse_response(&resp, &cx()).unwrap().cost.unwrap();
        assert!(c.amount_usd.is_none());
        assert_eq!(c.input_tokens, 5);
    }

    #[test]
    fn cost_respects_overrides() {
        let o = Overrides {
            output: Some(1.0),
            ..Default::default()
        };
        let cx2 = CostContext {
            overrides: &o,
            ..cx()
        };
        let resp = json!({"content": [], "usage": {"output_tokens": 1_000_000}});
        let c = parse_response(&resp, &cx2).unwrap().cost.unwrap();
        assert!((c.amount_usd.unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn no_text_gives_none() {
        let resp = json!({"content": [], "usage": {"input_tokens": 1, "output_tokens": 0}});
        let c = parse_response(&resp, &cx()).unwrap();
        assert!(c.text.is_none());
        assert_eq!(c.usage.cached_tokens, 0);
    }

    #[test]
    fn whitespace_only_text_is_skipped() {
        let resp = json!({"content": [{"type":"text","text":" \n"}], "usage": {}});
        assert!(parse_response(&resp, &cx()).unwrap().text.is_none());
    }

    #[test]
    fn cache_ttl_rejects_unknown() {
        assert!(CacheTtl::parse("5m").is_ok());
        assert!(CacheTtl::parse("1h").is_ok());
        assert!(matches!(CacheTtl::parse("10m"), Err(Error::Config(_))));
    }

    #[test]
    fn effort_values() {
        assert_eq!(parse_effort("low").unwrap(), "low");
        assert_eq!(parse_effort("max").unwrap(), "max");
        assert!(parse_effort("adaptive").is_err());
        assert!(parse_effort("LOW").is_err());
    }
}
