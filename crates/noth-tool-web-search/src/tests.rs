use super::*;
use noth_backend_tinyfish::testing::{MockServer, Reply, load_config};
use noth_core::types::TaskCategory;

fn ctx() -> ToolContext {
    ToolContext {
        run_id: "r".into(),
        task_category: TaskCategory::WebResearch,
        work_dir: std::env::temp_dir(),
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
