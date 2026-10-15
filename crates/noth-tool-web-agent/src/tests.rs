use super::*;
use noth_backend_tinyfish::testing::{MockServer, Recorded, Reply, load_config};
use noth_core::types::{Billing, MeterEvent, TaskCategory};

fn ctx() -> ToolContext {
    ToolContext {
        run_id: "r".into(),
        task_category: TaskCategory::WebResearch,
        work_dir: std::env::temp_dir(),
        meter: None,
    }
}

fn tool(server: &MockServer) -> WebAgent {
    WebAgent::new(server.backend(), Duration::ZERO, 300)
}

fn run_json(status: &str, extra: &str) -> String {
    format!(r#"{{"run_id":"r1","status":"{status}"{extra}}}"#)
}

#[tokio::test]
async fn polls_until_completed() {
    let server = MockServer::start(|req, n| {
        if req.method == "POST" {
            return Reply::json(200, r#"{"run_id":"r1","error":null}"#);
        }
        match n {
            1 => Reply::json(200, &run_json("PENDING", "")),
            2 => Reply::json(200, &run_json("RUNNING", r#","num_of_steps":1"#)),
            _ => Reply::json(
                200,
                &run_json(
                    "COMPLETED",
                    r#","num_of_steps":4,"result":{"price":"100円"}"#,
                ),
            ),
        }
    })
    .await;
    let out = tool(&server)
        .call(
            json!({"url":"https://x","goal":"価格を調べる","max_steps":30}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("status: COMPLETED"));
    assert!(out.text.contains("num_of_steps: 4"));
    assert!(out.text.contains("\"price\": \"100円\""));

    let reqs = server.recorded();
    assert_eq!(reqs.len(), 4);
    assert_eq!(reqs[0].target, "/v1/automation/run-async");
    assert_eq!(reqs[0].header("x-api-key"), Some("test-key"));
    let body: Value = serde_json::from_str(&reqs[0].body).unwrap();
    assert_eq!(body["agent_config"]["max_steps"], 30);
    assert_eq!(body["agent_config"]["max_duration_seconds"], 300);
    assert!(reqs[1..].iter().all(|r| r.target == "/v1/runs/r1"));
}

#[tokio::test]
async fn failure_result_is_error_with_reason() {
    let server = MockServer::start(|req, _| {
        if req.method == "POST" {
            Reply::json(200, r#"{"run_id":"r1"}"#)
        } else {
            Reply::json(
                200,
                &run_json(
                    "COMPLETED",
                    r#","num_of_steps":2,"result":{"status":"failure","reason":"ログインが必要"}"#,
                ),
            )
        }
    })
    .await;
    let out = tool(&server)
        .call(json!({"url":"https://x","goal":"g"}), &ctx())
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text.contains("ログインが必要"), "{}", out.text);
}

#[tokio::test]
async fn failed_run_reports_error_message() {
    let server = MockServer::start(|req, _| {
        if req.method == "POST" {
            Reply::json(200, r#"{"run_id":"r1"}"#)
        } else {
            Reply::json(
                200,
                &run_json(
                    "FAILED",
                    r#","error":{"code":"TIMEOUT","message":"時間切れ"}"#,
                ),
            )
        }
    })
    .await;
    let out = tool(&server)
        .call(json!({"url":"https://x","goal":"g"}), &ctx())
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text.contains("TIMEOUT: 時間切れ"), "{}", out.text);
}

#[tokio::test]
async fn dropped_call_cancels_run() {
    let server = MockServer::start(|req, _| {
        if req.method == "POST" && req.target.ends_with("run-async") {
            Reply::json(200, r#"{"run_id":"r1"}"#)
        } else if req.target.ends_with("/cancel") {
            Reply::json(200, r#"{"run_id":"r1","status":"CANCELLED"}"#)
        } else {
            Reply::json(200, &run_json("RUNNING", ""))
        }
    })
    .await;
    let t = WebAgent::new(server.backend(), Duration::from_millis(10), 300);
    let timed_out = tokio::time::timeout(
        Duration::from_millis(100),
        t.call(json!({"url":"https://x","goal":"g"}), &ctx()),
    )
    .await;
    assert!(timed_out.is_err());
    for _ in 0..100 {
        if server
            .recorded()
            .iter()
            .any(|r| r.target == "/v1/runs/r1/cancel")
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("cancel が送られなかった");
}

#[tokio::test]
async fn completed_call_does_not_cancel() {
    let server = MockServer::start(|req, _| {
        if req.method == "POST" {
            Reply::json(200, r#"{"run_id":"r1"}"#)
        } else {
            Reply::json(200, &run_json("COMPLETED", r#","result":{}"#))
        }
    })
    .await;
    tool(&server)
        .call(json!({"url":"https://x","goal":"g"}), &ctx())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        server
            .recorded()
            .iter()
            .all(|r| !r.target.ends_with("/cancel"))
    );
}

#[tokio::test]
async fn validates_input() {
    let server = MockServer::start(|_, _| Reply::json(200, "{}")).await;
    let t = tool(&server);
    for input in [
        json!({"goal":"g"}),
        json!({"url":"https://x"}),
        json!({"url":"https://x","goal":"g","max_steps":151}),
        json!({"url":"https://x","goal":"g","max_steps":0}),
    ] {
        assert!(t.call(input, &ctx()).await.unwrap().is_error);
    }
    assert!(server.recorded().is_empty());
}

#[tokio::test]
async fn spec_warns_about_cost() {
    let server = MockServer::start(|_, _| Reply::json(200, "{}")).await;
    let spec = tool(&server).spec();
    assert_eq!(spec.name, "web_agent");
    assert!(spec.description.contains("有料"));
    assert_eq!(spec.input_schema["required"], json!(["url", "goal"]));
}

#[test]
fn factory_reads_settings() {
    for (section, ok) in [
        ("", true),
        ("poll_interval_secs = 2\nmax_duration_secs = 60", true),
        ("poll_interval_secs = 0", false),
        ("backend = \"x\"", false),
    ] {
        let cfg = load_config(
            &format!(
                "[modules]\nenabled = [\"channel-discord\", \"router-mention\", \"provider-anthropic\", \"tool-web-agent\"]\n\
                 [channel.discord]\ntoken = \"t\"\n[provider.anthropic]\napi_key = \"k\"\n\
                 [tool.web-agent]\n{section}\n[backend.tinyfish]\napi_key = \"k\"\n"
            ),
            "",
        );
        let ctx = ModuleCtx {
            name: "tool-web-agent",
            section: cfg.module_section("tool-web-agent"),
            config: &cfg,
        };
        assert_eq!(factory(&ctx).is_ok(), ok, "{section}");
    }
}

fn has_cancel(server: &MockServer) -> bool {
    server
        .recorded()
        .iter()
        .any(|r| r.target == "/v1/runs/r1/cancel")
}

fn mock_with_status(status: &'static str) -> impl Fn(&Recorded, usize) -> Reply {
    move |req, _| {
        if req.target.ends_with("run-async") {
            Reply::json(200, r#"{"run_id":"r1"}"#)
        } else if req.target.ends_with("/cancel") {
            Reply::json(200, r#"{"run_id":"r1","status":"CANCELLED"}"#)
        } else {
            Reply::json(200, &run_json(status, ""))
        }
    }
}

#[tokio::test]
async fn unknown_status_cancels_and_errors() {
    let server = MockServer::start(mock_with_status("WEIRD")).await;
    let out = tool(&server)
        .call(json!({"url":"https://x","goal":"g"}), &ctx())
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text.contains("WEIRD"), "{}", out.text);
    assert!(has_cancel(&server));
}

#[tokio::test]
async fn local_deadline_cancels_and_errors() {
    let server = MockServer::start(mock_with_status("RUNNING")).await;
    let t = WebAgent::new(server.backend(), Duration::from_millis(10), 0)
        .with_grace(Duration::from_millis(50));
    let out = t
        .call(json!({"url":"https://x","goal":"g"}), &ctx())
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text.contains("中断"), "{}", out.text);
    assert!(has_cancel(&server));
}

fn ctx_with(run_id: &str) -> ToolContext {
    ToolContext {
        run_id: run_id.into(),
        ..ctx()
    }
}

fn completed_with_steps(n: u32) -> impl Fn(&Recorded, usize) -> Reply {
    move |req, _| {
        if req.method == "POST" {
            Reply::json(200, r#"{"run_id":"r1"}"#)
        } else {
            Reply::json(
                200,
                &run_json(
                    "COMPLETED",
                    &format!(r#","num_of_steps":{n},"result":{{}}"#),
                ),
            )
        }
    }
}

fn sent_max_steps(server: &MockServer) -> Vec<Value> {
    server
        .recorded()
        .iter()
        .filter(|r| r.target.ends_with("run-async"))
        .map(|r| {
            serde_json::from_str::<Value>(&r.body).unwrap()["agent_config"]["max_steps"].clone()
        })
        .collect()
}

#[tokio::test]
async fn max_steps_is_min_of_input_and_setting() {
    let server = MockServer::start(completed_with_steps(1)).await;
    let t = tool(&server).with_step_limits(20, 1000);
    for input in [
        json!({"url":"u","goal":"g"}),
        json!({"url":"u","goal":"g","max_steps":50}),
        json!({"url":"u","goal":"g","max_steps":5}),
    ] {
        t.call(input, &ctx()).await.unwrap();
    }
    assert_eq!(
        sent_max_steps(&server),
        vec![json!(20), json!(20), json!(5)]
    );
}

#[tokio::test]
async fn per_run_cap_blocks_without_api_call() {
    let server = MockServer::start(completed_with_steps(10)).await;
    let t = tool(&server).with_step_limits(30, 25);
    let input = json!({"url":"u","goal":"g"});
    assert!(
        !t.call(input.clone(), &ctx_with("a"))
            .await
            .unwrap()
            .is_error
    );
    assert!(
        !t.call(input.clone(), &ctx_with("a"))
            .await
            .unwrap()
            .is_error
    );
    // 累計 20 なので残りは 5 に絞られる
    assert_eq!(sent_max_steps(&server)[2 - 1], json!(25 - 10));
    assert!(
        !t.call(input.clone(), &ctx_with("a"))
            .await
            .unwrap()
            .is_error
    );
    let before = server.recorded().len();
    let out = t.call(input.clone(), &ctx_with("a")).await.unwrap();
    assert!(out.is_error);
    assert!(out.text.contains("上限"));
    assert_eq!(server.recorded().len(), before);
    // 別 run_id は影響を受けない
    assert!(!t.call(input, &ctx_with("b")).await.unwrap().is_error);
}

#[tokio::test]
async fn transient_poll_error_does_not_cancel() {
    let server = MockServer::start(|req, n| {
        if req.method == "POST" {
            Reply::json(200, r#"{"run_id":"r1"}"#)
        } else if n < 3 {
            Reply::json(500, r#"{"error":{"code":"INTERNAL_ERROR","message":"m"}}"#)
        } else {
            Reply::json(200, &run_json("COMPLETED", r#","result":{}"#))
        }
    })
    .await;
    let out = tool(&server)
        .call(json!({"url":"u","goal":"g"}), &ctx())
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text);
    assert!(!has_cancel(&server));
}

#[tokio::test]
async fn persistent_poll_error_cancels_after_retries() {
    let server = MockServer::start(|req, _| {
        if req.target.ends_with("run-async") {
            Reply::json(200, r#"{"run_id":"r1"}"#)
        } else if req.target.ends_with("/cancel") {
            Reply::json(200, r#"{"run_id":"r1","status":"CANCELLED"}"#)
        } else {
            Reply::json(500, r#"{"error":{"code":"INTERNAL_ERROR","message":"m"}}"#)
        }
    })
    .await;
    let out = tool(&server)
        .call(json!({"url":"u","goal":"g"}), &ctx())
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(has_cancel(&server));
    let polls = server
        .recorded()
        .iter()
        .filter(|r| r.target == "/v1/runs/r1")
        .count();
    assert_eq!(polls, 3);
}

#[test]
fn factory_validates_step_settings() {
    for (section, ok) in [
        ("max_steps = 10\nmax_steps_per_run = 20", true),
        ("max_steps = 151", false),
        ("max_steps_per_run = 0", false),
    ] {
        let cfg = load_config(
            &format!(
                "[modules]\nenabled = [\"channel-discord\", \"router-mention\", \"provider-anthropic\", \"tool-web-agent\"]\n\
                 [channel.discord]\ntoken = \"t\"\n[provider.anthropic]\napi_key = \"k\"\n\
                 [tool.web-agent]\n{section}\n[backend.tinyfish]\napi_key = \"k\"\n"
            ),
            "",
        );
        let ctx = ModuleCtx {
            name: "tool-web-agent",
            section: cfg.module_section("tool-web-agent"),
            config: &cfg,
        };
        assert_eq!(factory(&ctx).is_ok(), ok, "{section}");
    }
}

#[tokio::test]
async fn permanent_poll_error_cancels_immediately() {
    let server = MockServer::start(|req, _| {
        if req.target.ends_with("run-async") {
            Reply::json(200, r#"{"run_id":"r1"}"#)
        } else if req.target.ends_with("/cancel") {
            Reply::json(200, r#"{"run_id":"r1","status":"CANCELLED"}"#)
        } else {
            Reply::json(404, r#"{"error":{"code":"NOT_FOUND","message":"m"}}"#)
        }
    })
    .await;
    let out = tool(&server)
        .call(json!({"url":"u","goal":"g"}), &ctx())
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(has_cancel(&server));
    let polls = server
        .recorded()
        .iter()
        .filter(|r| r.target == "/v1/runs/r1")
        .count();
    assert_eq!(polls, 1);
}

fn metered_ctx() -> (
    ToolContext,
    tokio::sync::mpsc::UnboundedReceiver<MeterEvent>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (
        ToolContext {
            meter: Some(tx),
            run_id: "run-1".into(),
            ..ctx()
        },
        rx,
    )
}

fn wallet_mock(wallet_status: u16) -> impl Fn(&Recorded, usize) -> Reply {
    move |req, _| {
        if req.target == "/v1/wallet" {
            Reply::json(
                wallet_status,
                r#"{"available_balance":"10","currency":"USD","as_of":"x","rates":{"meters":[{"label":"TinyFish Agent","unit_amount":"0.016","per":"step"}]}}"#,
            )
        } else if req.method == "POST" {
            Reply::json(200, r#"{"run_id":"r1"}"#)
        } else {
            Reply::json(
                200,
                &run_json("COMPLETED", r#","num_of_steps":5,"result":{}"#),
            )
        }
    }
}

#[tokio::test]
async fn reports_metered_cost_from_wallet_rate_and_caches_it() {
    let server = MockServer::start(wallet_mock(200)).await;
    let t = tool(&server);
    let (ctx, mut rx) = metered_ctx();
    for _ in 0..2 {
        t.call(json!({"url":"u","goal":"g"}), &ctx).await.unwrap();
    }
    for _ in 0..2 {
        let Some(MeterEvent::Cost(c)) = rx.recv().await else {
            panic!("cost が来るはず");
        };
        assert_eq!(
            (c.source.as_str(), c.model.as_str()),
            ("tool-web-agent", "agent")
        );
        assert_eq!(c.unit, Some(("step".into(), 5)));
        assert_eq!(c.billing, Billing::Metered);
        assert_eq!(c.run_id.as_deref(), Some("run-1"));
        assert!((c.amount_usd.unwrap() - 0.08).abs() < 1e-9);
    }
    let wallets = server
        .recorded()
        .iter()
        .filter(|r| r.target == "/v1/wallet")
        .count();
    assert_eq!(wallets, 1);
}

#[tokio::test]
async fn unknown_rate_gives_none_amount() {
    let server = MockServer::start(wallet_mock(500)).await;
    let (ctx, mut rx) = metered_ctx();
    tool(&server)
        .call(json!({"url":"u","goal":"g"}), &ctx)
        .await
        .unwrap();
    let Some(MeterEvent::Cost(c)) = rx.recv().await else {
        panic!("cost が来るはず");
    };
    assert_eq!(c.amount_usd, None);
    assert_eq!(c.unit, Some(("step".into(), 5)));
}

#[tokio::test]
async fn account_status_reads_wallet() {
    let server = MockServer::start(wallet_mock(200)).await;
    let line = tool(&server).account_status().await.unwrap().unwrap();
    assert!(line.starts_with("TinyFish 残高 $10"), "{line}");
}
