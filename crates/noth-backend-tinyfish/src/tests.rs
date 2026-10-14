use super::*;
use crate::testing::{MockServer, Reply, load_config};

fn config_with(backend: &str) -> Config {
    load_config(
        &format!(
            "[modules]\nenabled = [\"channel-discord\", \"router-mention\", \"provider-anthropic\", \"tool-web-search\"]\n\
             [channel.discord]\ntoken = \"t\"\n[provider.anthropic]\napi_key = \"k\"\n\
             [tool.web-search]\nbackend = \"tinyfish\"\n[backend.tinyfish]\n{backend}\n"
        ),
        "",
    )
}

#[test]
fn from_config_requires_api_key() {
    let err = TinyFish::from_config(&config_with(""))
        .unwrap_err()
        .to_string();
    assert!(err.contains("[backend.tinyfish] api_key"), "{err}");
}

#[test]
fn from_config_rejects_bad_timeout() {
    let c = config_with("api_key = \"k\"\ntimeout_secs = 0");
    assert!(TinyFish::from_config(&c).is_err());
}

#[test]
fn from_config_reads_overrides() {
    let c = config_with("api_key = \"k\"\nsearch_url = \"http://x/\"\ntimeout_secs = 3");
    let t = TinyFish::from_config(&c).unwrap();
    assert_eq!(t.endpoints.search, "http://x");
    assert_eq!(t.endpoints.fetch, FETCH_URL);
    assert!(!format!("{t:?}").contains("api_key"));
}

#[test]
fn require_backend_checks_name() {
    let ok = toml::Value::Table("backend = \"tinyfish\"".parse().unwrap());
    assert!(require_backend(&ok, "web-search").is_ok());
    assert!(require_backend(&toml::Value::Table(Default::default()), "web-search").is_ok());
    let bad = toml::Value::Table("backend = \"serper\"".parse().unwrap());
    let err = require_backend(&bad, "web-search").unwrap_err().to_string();
    assert!(
        err.contains("serper") && err.contains("[tool.web-search]"),
        "{err}"
    );
}

#[test]
fn retry_policy() {
    let s = Duration::from_secs;
    assert_eq!(retry_delay(429, None, 0), Some(Duration::from_millis(500)));
    assert_eq!(retry_delay(503, None, 1), Some(Duration::from_secs(1)));
    assert_eq!(retry_delay(503, None, 2), None);
    assert_eq!(retry_delay(400, None, 0), None);
    assert_eq!(retry_delay(500, None, 0), None);
    assert_eq!(retry_delay(429, Some(s(3)), 0), Some(s(3)));
    assert_eq!(retry_delay(429, Some(s(11)), 0), None);
}

#[tokio::test]
async fn search_sends_params_and_parses() {
    let server = MockServer::start(|_, _| {
        Reply::json(
            200,
            r#"{"query":"q","results":[{"position":1,"site_name":"example.com","snippet":"s","title":"T","url":"https://example.com","date":"2026-01-01"}],"total_results":1,"page":0}"#,
        )
    })
    .await;
    let opts = SearchOptions {
        domain_type: Some("news".into()),
        recency_minutes: Some(60),
        include_domains: vec!["a.com".into(), "b.com".into()],
        page: Some(2),
        purpose: None,
    };
    let resp = server.backend().search("rust 言語", &opts).await.unwrap();
    assert_eq!(resp.results[0].title, "T");
    assert_eq!(resp.results[0].date.as_deref(), Some("2026-01-01"));

    let req = &server.recorded()[0];
    assert_eq!(req.method, "GET");
    assert_eq!(req.header("x-api-key"), Some("test-key"));
    let url = Url::parse(&format!("http://h{}", req.target)).unwrap();
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(q["query"], "rust 言語");
    assert_eq!(q["domain_type"], "news");
    assert_eq!(q["recency_minutes"], "60");
    assert_eq!(q["include_domains"], "a.com,b.com");
    assert_eq!(q["page"], "2");
    assert!(!q.contains_key("purpose"));
}

#[tokio::test]
async fn fetch_sends_json_body() {
    let server = MockServer::start(|_, _| {
        Reply::json(
            200,
            r#"{"results":[{"url":"https://a","title":"A","text":null}],"errors":[{"url":"https://b","error":"timeout"}]}"#,
        )
    })
    .await;
    let urls = vec!["https://a".to_string(), "https://b".to_string()];
    let opts = FetchOptions {
        purpose: Some("調査".into()),
    };
    let resp = server.backend().fetch(&urls, &opts).await.unwrap();
    assert_eq!(resp.results[0].text, None);
    assert_eq!(resp.errors[0].error, "timeout");

    let req = &server.recorded()[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.header("x-api-key"), Some("test-key"));
    let body: Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(body["urls"], json!(["https://a", "https://b"]));
    assert_eq!(body["format"], "markdown");
    assert_eq!(body["purpose"], "調査");
}

#[tokio::test]
async fn agent_endpoints() {
    let server = MockServer::start(|req, _| {
        if req.target == "/v1/automation/run-async" {
            Reply::json(200, r#"{"run_id":"r1","error":null}"#)
        } else if req.target == "/v1/runs/r1/cancel" {
            Reply::json(200, r#"{"run_id":"r1","status":"CANCELLED"}"#)
        } else {
            Reply::json(
                200,
                r#"{"run_id":"r1","status":"COMPLETED","num_of_steps":3,"result":{"a":1},"error":null}"#,
            )
        }
    })
    .await;
    let tf = server.backend();
    let id = tf
        .agent_run_async("https://x", "goal", Some(20), 300)
        .await
        .unwrap();
    assert_eq!(id, "r1");
    let run = tf.agent_run(&id).await.unwrap();
    assert_eq!(run.run_status(), RunStatus::Completed);
    assert_eq!(run.num_of_steps, Some(3));
    tf.agent_cancel(&id).await.unwrap();

    let reqs = server.recorded();
    let body: Value = serde_json::from_str(&reqs[0].body).unwrap();
    assert_eq!(body["url"], "https://x");
    assert_eq!(body["goal"], "goal");
    assert_eq!(body["agent_config"]["max_steps"], 20);
    assert_eq!(body["agent_config"]["max_duration_seconds"], 300);
    assert_eq!(reqs[1].method, "GET");
    assert_eq!(reqs[2].method, "POST");
}

#[tokio::test]
async fn error_body_becomes_one_line_without_status() {
    let server = MockServer::start(|_, _| {
        Reply::json(
            400,
            r#"{"error":{"code":"INVALID_INPUT","message":"query\nが空です"},"request_id":"x"}"#,
        )
    })
    .await;
    let err = server
        .backend()
        .search("", &SearchOptions::default())
        .await
        .unwrap_err();
    match err {
        Error::Api { status, body } => {
            assert_eq!(status, 400);
            assert_eq!(body, "TinyFish エラー INVALID_INPUT: query が空です");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(server.recorded().len(), 1, "400 は再試行しない");
}

#[test]
fn long_error_is_truncated() {
    let body = format!(
        r#"{{"error":{{"code":"X","message":"{}"}}}}"#,
        "あ".repeat(500)
    );
    let msg = error_message(&body);
    assert!(msg.chars().count() <= ERROR_BODY_LIMIT + 3, "{msg}");
    assert!(msg.ends_with("..."));
}

#[tokio::test]
async fn retries_429_then_succeeds() {
    let server = MockServer::start(|_, n| {
        if n < 2 {
            Reply::json(
                429,
                r#"{"error":{"code":"RATE_LIMIT_EXCEEDED","message":"m"}}"#,
            )
            .with_header("Retry-After", "0")
        } else {
            Reply::json(200, r#"{"results":[]}"#)
        }
    })
    .await;
    let resp = server
        .backend()
        .search("q", &SearchOptions::default())
        .await
        .unwrap();
    assert!(resp.results.is_empty());
    assert_eq!(server.recorded().len(), 3);
}

#[tokio::test]
async fn gives_up_after_two_retries() {
    let server = MockServer::start(|_, _| {
        Reply::json(503, r#"{"error":{"code":"SERVICE_BUSY","message":"m"}}"#)
            .with_header("Retry-After", "0")
    })
    .await;
    let err = server
        .backend()
        .search("q", &SearchOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Api { status: 503, .. }));
    assert_eq!(server.recorded().len(), 3);
}

#[tokio::test]
async fn health_check_uses_one_search() {
    let server = MockServer::start(|_, _| Reply::json(200, r#"{"results":[]}"#)).await;
    server.backend().health_check().await.unwrap();
    let reqs = server.recorded();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].target.contains("query=noth"));
}

#[tokio::test]
async fn run_async_is_not_retried() {
    let server = MockServer::start(|_, _| {
        Reply::json(503, r#"{"error":{"code":"SERVICE_BUSY","message":"m"}}"#)
            .with_header("Retry-After", "0")
    })
    .await;
    let err = server
        .backend()
        .agent_run_async("https://x", "g", None, 60)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Api { status: 503, .. }));
    assert_eq!(server.recorded().len(), 1);
}

#[tokio::test]
async fn daily_limit_without_retry_after_is_not_retried() {
    let server = MockServer::start(|_, _| {
        Reply::json(
            429,
            r#"{"error":{"code":"DAILY_LIMIT_EXCEEDED","message":"m"}}"#,
        )
    })
    .await;
    let err = server
        .backend()
        .search("q", &SearchOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Api { status: 429, .. }));
    assert_eq!(server.recorded().len(), 1);
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let server = MockServer::start(|_, _| {
        Reply::json(302, "{}").with_header("Location", "http://127.0.0.1:1/")
    })
    .await;
    let err = server
        .backend()
        .search("q", &SearchOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Api { status: 302, .. }));
}

#[test]
fn api_key_header_is_sensitive() {
    let t = TinyFish::new(
        "secret".into(),
        Duration::from_secs(1),
        Endpoints::default(),
    )
    .unwrap();
    assert!(t.api_key.is_sensitive());
    assert!(TinyFish::new("a\nb".into(), Duration::from_secs(1), Endpoints::default()).is_err());
}
