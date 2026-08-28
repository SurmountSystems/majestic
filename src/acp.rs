//! ACP-shaped NDJSON JSON-RPC (`memex acp`).
//!
//! Same local functions as MCP, exposed as JSON-RPC methods. Default is JSON,
//! one object per line. `--toon` uses [TOON](https://github.com/toon-format/spec)
//! (accessed: 2026-08-27) for language-model tools. Not the crates.io
//! `agent-client-protocol` SDK.

use std::io;

use serde_json::Value;

use crate::Error;
use crate::rpc::{
    JsonRpcRequest, JsonRpcResponse, LOCAL_FUNCTION_METHODS, RpcContext, call_local,
    product_rpc_error, respond_ok, serve_ndjson,
};
use crate::wire;

/// Run the ACP-shaped server on stdin/stdout until stdin closes.
pub fn serve_stdio(ctx: &RpcContext) -> Result<(), Error> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve_ndjson(ctx, stdin.lock(), stdout.lock(), handle_value)
}

/// Handle one JSON or TOON document.
pub fn handle_line(ctx: &RpcContext, line: &str) -> Option<JsonRpcResponse> {
    match wire::parse_rpc_value(line) {
        Ok((value, _)) => handle_value(ctx, value),
        Err(err) => Some(JsonRpcResponse::parse_error(err)),
    }
}

/// Handle a parsed JSON value as one ACP request.
pub fn handle_value(ctx: &RpcContext, value: Value) -> Option<JsonRpcResponse> {
    match serde_json::from_value::<JsonRpcRequest>(value) {
        Ok(req) => handle_request(ctx, &req),
        Err(err) => Some(JsonRpcResponse::parse_error(err.to_string())),
    }
}

/// Dispatch one ACP JSON-RPC method (local function name, or `initialize`).
pub fn handle_request(ctx: &RpcContext, request: &JsonRpcRequest) -> Option<JsonRpcResponse> {
    tracing::debug!(method = %request.method, "received an ACP request");
    if request.method == "initialize" {
        return respond_ok(request, initialize_result());
    }
    match call_local(ctx, &request.method, &request.params) {
        Ok(value) => respond_ok(request, value),
        Err(err) => {
            request.id.as_ref()?;
            product_rpc_error(request, &err)
        }
    }
}

/// In-process ACP call. Same as [`call_local`].
pub fn call(ctx: &RpcContext, method: &str, params: &Value) -> Result<Value, Error> {
    call_local(ctx, method, params)
}

/// Method names this surface dispatches (not including `initialize`).
pub fn dispatched_methods() -> &'static [&'static str] {
    LOCAL_FUNCTION_METHODS
}

/// Parse ACP method names from the **ACP method** column of `docs/local-functions.md`.
pub fn methods_listed_in_doc(doc: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut acp_col: Option<usize> = None;
    for line in doc.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') {
            if acp_col.is_some() {
                break;
            }
            continue;
        }
        let cols: Vec<&str> = trimmed
            .split('|')
            .map(str::trim)
            .filter(|col| !col.is_empty())
            .collect();
        if acp_col.is_none() {
            if let Some(index) = cols.iter().position(|col| *col == "ACP method") {
                acp_col = Some(index);
            }
            continue;
        }
        if trimmed.contains("---") {
            continue;
        }
        let Some(index) = acp_col else {
            continue;
        };
        let Some(raw) = cols.get(index) else {
            continue;
        };
        if *raw == "n/a" {
            continue;
        }
        for method in backtick_idents(raw) {
            if seen.insert(method.clone()) {
                out.push(method);
            }
        }
    }
    out
}

fn initialize_result() -> Value {
    serde_json::json!({
        "protocolVersion": "acp-local/1",
        "serverInfo": {
            "name": "memex",
            "version": env!("CARGO_PKG_VERSION")
        },
        "methods": LOCAL_FUNCTION_METHODS
    })
}

fn backtick_idents(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else {
            break;
        };
        let token = &rest[..end];
        rest = &rest[end + 1..];
        let ident = !token.is_empty()
            && token
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        if ident {
            out.push(token.to_owned());
        }
    }
    out
}
