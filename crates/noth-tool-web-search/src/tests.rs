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
async fn formats_numbered_list_and_sends_params() {
    let server = MockServer::start(|_, _| {
        Reply::json(
            200,
            r#"{"results":[
              {"title":"A","url":"https://a.example","snippet":"sa","date":"2026-02-03"},
              {"title":"B","url":"https://b.example","snippet":""}]}"#,
        )
    })
    .await;
    let tool = WebSearch::new(server.backend());
    let out = tool
        .call(
            json!({"query":"rust","domain_type":"news","recency_minutes":30,"include_domains":["a.example"],"page":1}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert_eq!(
        out.text,
        "1. A\n   https://a.example\n   sa\n   日付: 2026-02-03\n2. B\n   https://b.example"
    );
    let target = &server.recorded()[0].target;
    for p in [
        "query=rust",
        "domain_type=news",
        "recency_minutes=30",
        "include_domains=a.example",
        "page=1",
    ] {
        assert!(target.contains(p), "{target}");
    }
}

#[tokio::test]
async fn empty_results_message() {
    let server = MockServer::start(|_, _| Reply::json(200, r#"{"results":[]}"#)).await;
    let out = WebSearch::new(server.backend())
        .call(json!({"query":"zzz"}), &ctx())
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(
        out.text.contains("検索結果はありませんでした"),
        "{}",
        out.text
    );
}

#[tokio::test]
async fn invalid_input_is_error_output_without_request() {
    let server = MockServer::start(|_, _| Reply::json(200, "{}")).await;
    let tool = WebSearch::new(server.backend());
    for input in [
        json!({}),
        json!({"query":"  "}),
        json!({"query":"a","domain_type":"x"}),
        json!({"query":"a","recency_minutes":0}),
        json!({"query":"a","include_domains":[1]}),
    ] {
        let out = tool.call(input, &ctx()).await.unwrap();
        assert!(out.is_error);
    }
    assert!(server.recorded().is_empty());
}

#[tokio::test]
async fn api_error_becomes_error_output() {
    let server = MockServer::start(|_, _| {
        Reply::json(
            401,
            r#"{"error":{"code":"INVALID_API_KEY","message":"bad"}}"#,
        )
    })
    .await;
    let out = WebSearch::new(server.backend())
        .call(json!({"query":"a"}), &ctx())
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text.contains("INVALID_API_KEY"));
}

#[test]
fn spec_name_and_schema() {
    let server_less = noth_backend_tinyfish::TinyFish::new(
        "k".into(),
        std::time::Duration::from_secs(1),
        Default::default(),
    )
    .unwrap();
    let spec = WebSearch::new(server_less).spec();
    assert_eq!(spec.name, "web_search");
    assert_eq!(spec.input_schema["required"], json!(["query"]));
}

fn config(tool_section: &str) -> noth_core::config::Config {
    load_config(
        &format!(
            "[modules]\nenabled = [\"channel-discord\", \"router-mention\", \"provider-anthropic\", \"tool-web-search\"]\n\
             [channel.discord]\ntoken = \"t\"\n[provider.anthropic]\napi_key = \"k\"\n\
             [tool.web-search]\n{tool_section}\n[backend.tinyfish]\napi_key = \"k\"\n"
        ),
        "",
    )
}

#[test]
fn factory_checks_backend_name() {
    for (section, ok) in [
        ("", true),
        ("backend = \"tinyfish\"", true),
        ("backend = \"x\"", false),
    ] {
        let cfg = config(section);
        let ctx = ModuleCtx {
            name: "tool-web-search",
            section: cfg.module_section("tool-web-search"),
            config: &cfg,
        };
        assert_eq!(factory(&ctx).is_ok(), ok, "{section}");
    }
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
async fn reports_rate_limit_and_free_cost() {
    let server = MockServer::start(|_, _| {
        Reply::json(200, r#"{"results":[]}"#)
            .with_header("x-ratelimit-limit", "30")
            .with_header("x-ratelimit-remaining", "7")
            .with_header("x-ratelimit-reset", "1760500000")
    })
    .await;
    let (ctx, mut rx) = metered_ctx();
    WebSearch::new(server.backend())
        .call(json!({"query":"a"}), &ctx)
        .await
        .unwrap();
    let Some(MeterEvent::RateLimit(rl)) = rx.recv().await else {
        panic!("rate limit が先に来るはず");
    };
    assert_eq!(
        (rl.source.as_str(), rl.bucket.as_str()),
        ("tool-web-search", "search")
    );
    assert_eq!(
        (rl.limit, rl.remaining, rl.reset_at),
        (30, 7, Some(1760500000))
    );
    let Some(MeterEvent::Cost(c)) = rx.recv().await else {
        panic!("cost が来るはず");
    };
    assert_eq!(c.model, "search");
    assert_eq!(c.run_id.as_deref(), Some("run-1"));
    assert_eq!(c.unit, Some(("request".into(), 1)));
    assert_eq!(c.amount_usd, Some(0.0));
    assert_eq!(c.billing, Billing::Free);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn account_status_reads_wallet() {
    let server = MockServer::start(|_, _| {
        Reply::json(
            200,
            r#"{"available_balance":"10","currency":"USD","as_of":"x"}"#,
        )
    })
    .await;
    let line = WebSearch::new(server.backend())
        .account_status()
        .await
        .unwrap();
    assert_eq!(line.as_deref(), Some("TinyFish 残高 $10(x 時点)"));
}
