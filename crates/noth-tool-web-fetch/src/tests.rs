use super::*;
use noth_backend_tinyfish::testing::{MockServer, Reply, load_config};
use noth_core::types::{Billing, MeterEvent, TaskCategory};

fn ctx() -> ToolContext {
    ToolContext {
        run_id: "r".into(),
        task_category: TaskCategory::WebResearch,
        work_dir: std::env::temp_dir(),
        meter: None,
        history: None,
        audience: Default::default(),
    }
}

#[tokio::test]
async fn formats_results_and_errors() {
    let server = MockServer::start(|_, _| {
        Reply::json(
            200,
            r#"{"results":[{"url":"https://a","title":"A","text":"本文A"},{"url":"https://c","text":null}],
                "errors":[{"url":"https://b","error":"target_http_error","status":404}]}"#,
        )
    })
    .await;
    let out = WebFetch::new(server.backend())
        .call(
            json!({"urls":["https://a","https://b","https://c"],"purpose":"確認"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert_eq!(
        out.text,
        "## https://b\n[取得失敗] target_http_error (HTTP 404)\n\n\
         ## https://a\nタイトル: A\n\n本文A\n\n## https://c\n(本文を取得できませんでした)"
    );
    let body: Value = serde_json::from_str(&server.recorded()[0].body).unwrap();
    assert_eq!(body["format"], "markdown");
    assert_eq!(body["purpose"], "確認");
}

#[tokio::test]
async fn all_failed_is_error() {
    let server = MockServer::start(|_, _| {
        Reply::json(
            200,
            r#"{"results":[],"errors":[{"url":"https://b","error":"timeout"}]}"#,
        )
    })
    .await;
    let out = WebFetch::new(server.backend())
        .call(json!({"urls":["https://b"]}), &ctx())
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text.contains("[取得失敗] timeout"));
}

#[tokio::test]
async fn validates_urls() {
    let server = MockServer::start(|_, _| Reply::json(200, "{}")).await;
    let tool = WebFetch::new(server.backend());
    let many: Vec<String> = (0..11).map(|i| format!("https://x/{i}")).collect();
    for input in [
        json!({}),
        json!({"urls":[]}),
        json!({"urls":many}),
        json!({"urls":[1]}),
    ] {
        assert!(tool.call(input, &ctx()).await.unwrap().is_error);
    }
    assert!(server.recorded().is_empty());
}

#[tokio::test]
async fn api_error_becomes_error_output() {
    let server = MockServer::start(|_, _| {
        Reply::json(
            402,
            r#"{"error":{"code":"FREE_ALLOWANCE_EXHAUSTED","message":"枠切れ"}}"#,
        )
    })
    .await;
    let out = WebFetch::new(server.backend())
        .call(json!({"urls":["https://a"]}), &ctx())
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text.contains("FREE_ALLOWANCE_EXHAUSTED"));
}

#[tokio::test]
async fn health_check_does_not_call_api() {
    let server = MockServer::start(|_, _| Reply::json(200, "{}")).await;
    WebFetch::new(server.backend())
        .health_check()
        .await
        .unwrap();
    assert!(server.recorded().is_empty());
}

#[test]
fn factory_checks_backend_name() {
    for (section, ok) in [("", true), ("backend = \"x\"", false)] {
        let cfg = load_config(
            &format!(
                "[modules]\nenabled = [\"channel-discord\", \"router-mention\", \"provider-anthropic\", \"tool-web-fetch\"]\n\
                 [channel.discord]\ntoken = \"t\"\n[provider.anthropic]\napi_key = \"k\"\n\
                 [tool.web-fetch]\n{section}\n[backend.tinyfish]\napi_key = \"k\"\n"
            ),
            "",
        );
        let ctx = ModuleCtx {
            name: "tool-web-fetch",
            section: cfg.module_section("tool-web-fetch"),
            config: &cfg,
        };
        assert_eq!(factory(&ctx).is_ok(), ok, "{section}");
    }
}

#[tokio::test]
async fn text_budget_is_split_per_url() {
    let long = "あ".repeat(30_000);
    let body = json!({"results":[
        {"url":"https://a","text":long},
        {"url":"https://b","text":"short"}
    ]})
    .to_string();
    let server = MockServer::start(move |_, _| Reply::json(200, &body)).await;
    let out = WebFetch::new(server.backend())
        .call(json!({"urls":["https://a","https://b"]}), &ctx())
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.text.contains("short"));
    assert!(out.text.contains("(以下省略)"));
    assert!(out.text.chars().count() < TOTAL_TEXT_BUDGET + 200);
}

#[tokio::test]
async fn all_empty_text_is_error() {
    let server = MockServer::start(|_, _| {
        Reply::json(
            200,
            r#"{"results":[{"url":"https://a","text":null},{"url":"https://b","text":""}]}"#,
        )
    })
    .await;
    let out = WebFetch::new(server.backend())
        .call(json!({"urls":["https://a","https://b"]}), &ctx())
        .await
        .unwrap();
    assert!(out.is_error);
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

#[tokio::test]
async fn reports_url_count_cost() {
    let server = MockServer::start(|_, _| {
        Reply::json(200, r#"{"results":[{"url":"https://a","text":"t"}]}"#)
            .with_header("x-ratelimit-limit", "150")
            .with_header("x-ratelimit-remaining", "148")
            .with_header("x-ratelimit-reset", "1760500000")
    })
    .await;
    let (ctx, mut rx) = metered_ctx();
    WebFetch::new(server.backend())
        .call(json!({"urls":["https://a","https://b"]}), &ctx)
        .await
        .unwrap();
    let Some(MeterEvent::RateLimit(rl)) = rx.recv().await else {
        panic!("rate limit が先に来るはず");
    };
    assert_eq!(
        (rl.source.as_str(), rl.bucket.as_str(), rl.limit),
        ("tool-web-fetch", "fetch", 150)
    );
    let Some(MeterEvent::Cost(c)) = rx.recv().await else {
        panic!("cost が来るはず");
    };
    assert_eq!(c.unit, Some(("url".into(), 2)));
    assert_eq!(c.billing, Billing::Free);
    assert_eq!(c.amount_usd, Some(0.0));
}

#[tokio::test]
async fn account_status_is_none() {
    let server = MockServer::start(|_, _| Reply::json(200, "{}")).await;
    assert_eq!(
        WebFetch::new(server.backend())
            .account_status()
            .await
            .unwrap(),
        None
    );
    assert!(server.recorded().is_empty());
}
