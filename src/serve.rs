//! JSON-RPC 2.0 MCP over HTTP (`memex serve`).
//!
//! `POST /mcp` is one JSON-RPC object (JSON or [TOON](https://github.com/toon-format/spec),
//! accessed: 2026-08-27). Notifications (no id) return 204. `GET /health`
//! returns 200 `ok`. Tracing stays on stderr. Loopback by default. No auth.
//!
//! Request encoding: `Content-Type: application/json` (default) or
//! `text/toon`. If that header is unset, a body that starts with `{` or `[`
//! is JSON, otherwise TOON. Response encoding: `Accept: text/toon`,
//! `?format=toon`, `--toon`, or the request encoding. Default JSON is for
//! humans. TOON is for language-model tools.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::Error;
use crate::mcp;
use crate::rpc::{JsonRpcResponse, RpcContext};
use crate::wire::{self, WireFormat};

/// Default listen address: IPv4 loopback, no auth.
pub const DEFAULT_BIND: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8741));

/// Axum router for MCP-over-HTTP. Tests call this without binding a port.
pub fn router(ctx: RpcContext) -> Router {
    Router::new()
        .route("/mcp", post(post_mcp))
        .route("/health", get(health))
        .with_state(ctx)
}

/// Listen on `bind` until the process exits. Starts the Tokio runtime.
pub fn serve(ctx: RpcContext, bind: SocketAddr) -> Result<(), Error> {
    tokio::runtime::Runtime::new()?.block_on(async move {
        let listener = tokio::net::TcpListener::bind(bind).await?;
        tracing::info!(%bind, "MCP HTTP listening");
        axum::serve(listener, router(ctx)).await?;
        Ok(())
    })
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Default, Deserialize)]
struct McpQuery {
    #[serde(default)]
    format: Option<String>,
}

async fn post_mcp(
    State(ctx): State<RpcContext>,
    headers: HeaderMap,
    Query(query): Query<McpQuery>,
    body: Bytes,
) -> Response {
    let text = match std::str::from_utf8(&body) {
        Ok(text) => text,
        Err(err) => {
            return rpc_body(
                JsonRpcResponse::parse_error(err.to_string()),
                WireFormat::Json,
            );
        }
    };
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok());
    let header_format = content_type.and_then(WireFormat::from_media_type);
    let parsed = wire::parse_rpc_value(text);
    let request_format = header_format.unwrap_or(match &parsed {
        Ok((_, format)) => *format,
        Err(_) => WireFormat::Json,
    });
    let format = WireFormat::response(
        ctx.prefer_toon,
        query.format.as_deref(),
        accept,
        request_format,
    );
    let value = match parsed {
        Ok((value, _)) => value,
        Err(err) => return rpc_body(JsonRpcResponse::parse_error(err), format),
    };
    match mcp::handle_value(&ctx, value) {
        Some(resp) => rpc_body(resp, format),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

fn rpc_body(resp: JsonRpcResponse, format: WireFormat) -> Response {
    match wire::encode_rpc(&resp, format) {
        Ok(body) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, format.content_type())],
            body,
        )
            .into_response(),
        Err(err) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, WireFormat::JSON_MEDIA)],
            serde_json::to_string(&JsonRpcResponse::parse_error(err)).unwrap_or_else(|_| {
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"encode failed"}}"#
                    .to_owned()
            }),
        )
            .into_response(),
    }
}
