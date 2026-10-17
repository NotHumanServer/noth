//! 実際の layers の組み立てと wire の変換を通して、連続するリクエストの先頭がバイト単位で一致するかを見る。
//! OpenAI の暗黙のキャッシュは、直近の user メッセージの末尾(と developer 群の末尾)にしか区切りを置かない。
//! そのため判定基準は「リクエストの最後の user メッセージの末尾までが、次のリクエストの先頭に残ること」になる
use noth_core::judge::DeliveryStyle;
use noth_core::layers;
use noth_core::traits::LayeredPrompt;
use noth_core::types::{
    CompletionParams, ExchangeItem, Role, ToolCall, ToolResult, ToolSpec, Turn, TurnKind,
};
use serde_json::{Value, json};

use crate::wire::build_codex_request;

fn user(kind: TurnKind, text: &str, chunk: i64) -> Turn {
    Turn {
        id: None,
        role: Role::User,
        kind,
        text: text.into(),
        chunk_id: Some(chunk),
    }
}

fn tools() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "web_search".into(),
        description: "検索".into(),
        input_schema: json!({"type": "object"}),
    }]
}

fn assemble(l3: &[Turn], tail: Vec<Turn>) -> LayeredPrompt {
    let mut prompt = layers::assemble(
        "rules",
        "profile",
        "",
        "digest",
        l3.to_vec(),
        tail,
        &tools(),
    );
    prompt.cache_session = Some("42".into());
    prompt
}

fn body_of(prompt: &LayeredPrompt) -> Value {
    let params = CompletionParams {
        max_tokens: 100,
        ..Default::default()
    };
    build_codex_request("m", prompt, &params, None).unwrap()
}

fn input_of(prompt: &LayeredPrompt) -> Vec<Value> {
    body_of(prompt)["input"].as_array().unwrap().clone()
}

fn assert_starts_with(label: &str, whole: &[Value], prefix: &[Value]) {
    for (i, want) in prefix.iter().enumerate() {
        let got = whole.get(i);
        assert!(
            got == Some(want),
            "{label}: input[{i}] が一致しません\n  期待: {want}\n  実際: {}",
            got.map_or("(要素なし)".to_string(), Value::to_string),
        );
    }
}

fn trigger(chunk: i64) -> Turn {
    layers::trigger_turn(chunk, &[], "general", 100)
}

/// actor.rs の respond と同じ順。バッファを昇格し、Layer 4 を付けて送り、返事の後で trigger と返事を昇格する。
/// `[判定]` は昇格しない
fn main_turn(l3: &mut Vec<Turn>, buffer: Option<&str>, chunk: i64, verdict: bool) -> Vec<Value> {
    if let Some(b) = buffer {
        l3.push(user(TurnKind::Buffer, b, chunk));
    }
    let mut tail = vec![trigger(chunk)];
    if verdict {
        tail.push(layers::verdict_turn(chunk));
    }
    let request = input_of(&assemble(l3, tail));
    l3.push(trigger(chunk));
    l3.push(layers::response_turn(chunk, &format!("返事 {chunk}")));
    request
}

fn start() -> Vec<Turn> {
    vec![
        user(TurnKind::Buffer, "[発言群]\nold", 1),
        layers::response_turn(1, "はい"),
    ]
}

#[test]
fn main_keeps_the_previous_request_as_prefix_without_verdict() {
    let mut l3 = start();
    let mut prev: Option<Vec<Value>> = None;
    for (i, buffer) in [
        None,
        Some("[発言群]\nbuffered a"),
        None,
        Some("[発言群]\nbuffered b"),
    ]
    .into_iter()
    .enumerate()
    {
        let chunk = i as i64 + 2;
        let request = main_turn(&mut l3, buffer, chunk, false);
        if let Some(p) = &prev {
            assert_starts_with(&format!("main ターン {chunk}"), &request, p);
        }
        prev = Some(request);
    }
}

#[test]
fn main_after_a_verdict_falls_back_to_the_write_before_it() {
    let mut l3 = start();
    let before = main_turn(&mut l3, Some("[発言群]\nbuffered"), 2, false);
    // [判定] は昇格しないので、このリクエストの書き込み位置(末尾)は次に残らない
    let with_verdict = main_turn(&mut l3, None, 3, true);
    let next = main_turn(&mut l3, None, 4, false);
    assert_starts_with("[判定] の次の main は 1 つ前の書き込みまで", &next, &before);
    // 内容が残るだけの構造の確認。trigger の末尾は書き込み位置ではない(書き込みは最後の user メッセージの末尾)ので、
    // ここまでキャッシュを引けるわけではない
    assert_starts_with(
        "trigger までの内容は残る(キャッシュを引ける位置ではない)",
        &next,
        &with_verdict[..with_verdict.len() - 1],
    );
    // 結論と昇格をはさんでも、直前の main の末尾は残る
    l3.push(layers::conclusion_turn(4, "run1", Ok("結論")));
    let after_conclusion = main_turn(&mut l3, Some("[発言群]\nafter"), 5, false);
    assert_starts_with("結論をはさんだ main", &after_conclusion, &next);
}

fn fork(l3: &mut Vec<Turn>, verdict: bool) -> (Vec<Value>, LayeredPrompt) {
    let mut tail = vec![trigger(3)];
    if verdict {
        tail.push(layers::verdict_turn(3));
    }
    let main_request = input_of(&assemble(l3, tail));
    // record_task が trigger と Ack + Goal を Layer 3 に昇格してから fork する
    l3.push(trigger(3));
    l3.push(layers::task_response_turn(3, "調べますね", "調べる"));
    let (head, tail) = layers::fork_split(l3.clone(), verdict.then(|| layers::verdict_turn(3)));
    let mut worker = assemble(&head, tail);
    worker
        .ephemeral_tail
        .push(user(TurnKind::Instruction, "Worker の指示", 0));
    worker.cache_to_end = true;
    (main_request, worker)
}

#[test]
fn worker_first_step_starts_with_the_whole_fork_time_request() {
    for verdict in [false, true] {
        let mut l3 = start();
        l3.push(user(TurnKind::Buffer, "[発言群]\nbuffered", 2));
        let (main_request, worker) = fork(&mut l3, verdict);
        assert_starts_with(
            &format!("Worker の最初のステップ(判定 {verdict})"),
            &input_of(&worker),
            &main_request,
        );
    }
}

fn exchange(n: usize) -> Vec<ExchangeItem> {
    let id = format!("call_{n}");
    vec![
        ExchangeItem::Assistant {
            text: None,
            calls: vec![ToolCall {
                id: id.clone(),
                name: "web_search".into(),
                input: json!({"q": n}),
            }],
            raw: None,
        },
        ExchangeItem::ToolResults(vec![ToolResult {
            call_id: id,
            name: "web_search".into(),
            output: format!("結果 {n}"),
            is_error: false,
        }]),
    ]
}

#[test]
fn worker_steps_keep_the_previous_step_as_prefix() {
    let mut l3 = start();
    let (_, mut worker) = fork(&mut l3, true);
    let mut prev = input_of(&worker);
    for n in 1..=3 {
        worker.tool_exchange.extend(exchange(n));
        let next = input_of(&worker);
        assert_starts_with(&format!("Worker のステップ {}", n + 1), &next, &prev);
        prev = next;
    }
}

#[test]
fn join_shares_the_fork_time_request_up_to_the_trigger() {
    for verdict in [false, true] {
        let mut l3 = start();
        let (main_request, _) = fork(&mut l3, verdict);
        l3.push(layers::conclusion_turn(3, "run1", Ok("結論")));
        let join = input_of(&assemble(
            &l3,
            vec![layers::delivery_instruction_turn(
                3,
                DeliveryStyle::NormalReport,
                &layers::ReportTarget {
                    channel_name: "general",
                    channel_id: 1,
                    request: None,
                },
                false,
            )],
        ));
        // [判定] は Join の Layer 3 に無いので、判定ありのときは trigger の末尾までしか重ならない
        let shared = if verdict {
            &main_request[..main_request.len() - 1]
        } else {
            &main_request[..]
        };
        assert_starts_with(&format!("Join(判定 {verdict})"), &join, shared);
    }
}

#[test]
fn worker_uses_the_same_cache_key_as_main() {
    let params = CompletionParams {
        max_tokens: 100,
        ..Default::default()
    };
    let mut base = assemble(&[user(TurnKind::Buffer, "[発言群]\nold", 1)], Vec::new());
    base.cache_session = Some("42".into());
    let mut worker = base.clone();
    worker
        .ephemeral_tail
        .push(user(TurnKind::Instruction, "Worker の指示", 0));
    worker.cache_to_end = true;
    let key = |p: &LayeredPrompt| {
        build_codex_request("m", p, &params, None).unwrap()["prompt_cache_key"].clone()
    };
    assert_eq!(key(&base), key(&worker));
}

#[test]
fn instructions_tools_and_key_are_identical_across_main_and_worker() {
    let mut l3 = start();
    let main_a = body_of(&assemble(&l3, vec![trigger(3), layers::verdict_turn(3)]));
    let main_b = body_of(&assemble(&l3, vec![trigger(4)]));
    let (_, worker) = fork(&mut l3, true);
    let worker = body_of(&worker);
    for key in [
        "instructions",
        "tools",
        "prompt_cache_key",
        "model",
        "include",
    ] {
        assert_eq!(main_a[key], main_b[key], "main→main の {key}");
        assert_eq!(main_a[key], worker[key], "main→Worker の {key}");
    }
}

#[test]
fn worker_steps_with_reasoning_items_keep_the_previous_step_as_prefix() {
    let mut l3 = start();
    let (_, mut worker) = fork(&mut l3, false);
    let mut prev = input_of(&worker);
    for n in 1..=2 {
        let id = format!("call_{n}");
        let raw = json!([
            {"type": "reasoning", "id": format!("rs_{n}"), "summary": [], "encrypted_content": format!("enc{n}")},
            {"type": "message", "id": format!("msg_{n}"), "role": "assistant", "content": [{"type": "output_text", "text": "調べます"}]},
            {"type": "function_call", "id": format!("fc_{n}"), "call_id": id, "name": "web_search", "arguments": "{}"},
        ]);
        worker.tool_exchange.push(ExchangeItem::Assistant {
            text: Some("調べます".into()),
            calls: vec![ToolCall {
                id: id.clone(),
                name: "web_search".into(),
                input: json!({}),
            }],
            raw: Some(raw),
        });
        worker
            .tool_exchange
            .push(ExchangeItem::ToolResults(vec![ToolResult {
                call_id: id,
                name: "web_search".into(),
                output: format!("結果 {n}"),
                is_error: false,
            }]));
        let next = input_of(&worker);
        assert_starts_with(
            &format!("reasoning を含む Worker のステップ {}", n + 1),
            &next,
            &prev,
        );
        // reasoning の id は store: false で引けないので外れ、暗号化された中身は残る
        assert!(
            next.iter()
                .any(|i| i["type"] == "reasoning" && i.get("id").is_none())
        );
        prev = next;
    }
}
