use std::path::{Path, PathBuf};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use majestic::rpc::RpcContext;
use majestic::serve::{self, DEFAULT_BIND};
use majestic::{WireFormat, encode_rpc, parse_rpc_value};
use serde_json::{Value, json};
use tower::ServiceExt;

fn operator_memex() -> &'static Path {
    Path::new("/home/hunter/memex")
}

fn fake_home() -> PathBuf {
    let home = PathBuf::from("/tmp/majestic-http-unused-home");
    assert!(
        !home.starts_with(operator_memex()),
        "tests must not use the operator memex directory"
    );
    home
}

#[test]
fn http_mcp_default_bind_is_loopback() {
    assert!(
        DEFAULT_BIND.ip().is_loopback(),
        "default bind must be loopback, got {DEFAULT_BIND}"
    );
    assert_eq!(DEFAULT_BIND.port(), 8741);
    assert_eq!(DEFAULT_BIND.to_string(), "127.0.0.1:8741");
}

#[tokio::test]
async fn http_mcp_post_tools_list_includes_search() {
    let ctx = RpcContext::new(fake_home());
    let app = serve::router(ctx);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "tools/list"
                    })
                    .to_string(),
                ))
                .expect("tools/list request"),
        )
        .await
        .expect("router");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    let body: Value = serde_json::from_slice(&bytes).expect("JSON-RPC body");
    assert!(
        body.get("error").is_none(),
        "tools/list must be JSON-RPC success, got {body}"
    );
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    assert!(
        names.contains(&"search"),
        "tools/list must include search, got {names:?}"
    );
}

#[tokio::test]
async fn http_mcp_get_health_ok() {
    let app = serve::router(RpcContext::new(fake_home()));
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/health")
                .body(Body::empty())
                .expect("health request"),
        )
        .await
        .expect("router");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("health body");
    assert_eq!(bytes.as_ref(), b"ok");
}

#[tokio::test]
async fn http_mcp_notification_returns_204() {
    let app = serve::router(RpcContext::new(fake_home()));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/initialized"
                    })
                    .to_string(),
                ))
                .expect("notification request"),
        )
        .await
        .expect("router");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("empty body");
    assert!(bytes.is_empty(), "204 must have an empty body");
}

#[tokio::test]
async fn http_mcp_post_accepts_toon_and_responds_toon() {
    let ctx = RpcContext::new(fake_home());
    let app = serve::router(ctx);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    });
    let toon = encode_rpc(&req, WireFormat::Toon).expect("encode TOON tools/list");
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "text/toon")
                .header("accept", "text/toon")
                .body(Body::from(toon))
                .expect("TOON tools/list request"),
        )
        .await
        .expect("router");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.starts_with("text/toon"),
        "TOON request with Accept text/toon must return text/toon, got {content_type}"
    );
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    let text = std::str::from_utf8(&bytes).expect("utf-8 TOON body");
    let (body, format) = parse_rpc_value(text).expect("parse TOON response");
    assert_eq!(format, WireFormat::Toon);
    assert!(
        body.get("error").is_none(),
        "tools/list must be JSON-RPC success, got {body}"
    );
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    assert!(
        names.contains(&"search"),
        "TOON tools/list must include search, got {names:?}"
    );
}
