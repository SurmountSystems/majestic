//! JSON-RPC 2.0 MCP stdio server (`memex mcp`).
//!
//! Default is JSON, one object per line on stdin/stdout. `--toon` accepts and
//! returns [TOON](https://github.com/toon-format/spec) (accessed: 2026-08-27)
//! for language-model tools: one TOON document then a blank line. Terminal
//! tracing is error and info on stderr. Full logs go to the systemd journal
//! (`journalctl --user -t memex`).

use std::io;

use serde_json::{Value, json};

use crate::Error;
use crate::rpc::{
    JsonRpcRequest, JsonRpcResponse, LOCAL_FUNCTION_METHODS, METHOD_NOT_FOUND, RpcContext,
    call_local, respond_err, respond_ok, serve_ndjson,
};
use crate::wire;

/// MCP protocol version this server speaks when the client omits one.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Run the MCP server on stdin/stdout until stdin closes.
pub fn serve_stdio(ctx: &RpcContext) -> Result<(), Error> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve_ndjson(ctx, stdin.lock(), stdout.lock(), handle_value)
}

/// Handle one JSON or TOON document. Parse errors become JSON-RPC `-32700`.
pub fn handle_line(ctx: &RpcContext, line: &str) -> Option<JsonRpcResponse> {
    match wire::parse_rpc_value(line) {
        Ok((value, _)) => handle_value(ctx, value),
        Err(err) => Some(JsonRpcResponse::parse_error(err)),
    }
}

/// Handle a parsed JSON value as one MCP request.
pub fn handle_value(ctx: &RpcContext, value: Value) -> Option<JsonRpcResponse> {
    match serde_json::from_value::<JsonRpcRequest>(value) {
        Ok(req) => handle_request(ctx, &req),
        Err(err) => Some(JsonRpcResponse::parse_error(err.to_string())),
    }
}

/// Dispatch one MCP JSON-RPC method.
pub fn handle_request(ctx: &RpcContext, request: &JsonRpcRequest) -> Option<JsonRpcResponse> {
    tracing::debug!(method = %request.method, "received an MCP request");
    match request.method.as_str() {
        "initialize" => respond_ok(request, initialize_result(&request.params)),
        "notifications/initialized" | "initialized" => None,
        "ping" => respond_ok(request, json!({})),
        "tools/list" => respond_ok(request, json!({ "tools": tool_defs() })),
        "tools/call" => respond_ok(request, tools_call(ctx, &request.params)),
        other => {
            request.id.as_ref()?;
            respond_err(
                request,
                METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            )
        }
    }
}

fn initialize_result(params: &Value) -> Value {
    let protocol_version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(MCP_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "memex",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn tools_call(ctx: &RpcContext, params: &Value) -> Value {
    let empty = json!({});
    let params = match params {
        Value::Null => &empty,
        Value::Object(_) => params,
        _ => return tool_error("tools/call params must be an object"),
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return tool_error("tools/call requires name");
    };
    if !LOCAL_FUNCTION_METHODS.contains(&name) {
        return tool_error(format!("unknown tool {name}"));
    }
    let arguments = params.get("arguments").unwrap_or(&empty);
    match call_local(ctx, name, arguments) {
        Ok(value) => json!({
            "content": [{ "type": "text", "text": value.to_string() }]
        }),
        Err(err) => tool_error(err.to_string()),
    }
}

fn tool_error(message: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": message.into() }],
        "isError": true
    })
}

fn tool_defs() -> Vec<Value> {
    vec![
        tool(
            "list_archives",
            "List *.majestic under $HOME/memex, plus leftover *.archive with no sibling .majestic. Does not list .zst.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "search",
            "Search mmap archives under $HOME/memex. Default is every archive. It does not walk $HOME for live export dumps. Optional service+account or archive searches one file. Pattern is a human query unless slash-wrapped /regex/flags (PCRE2, same as rg -P; no pcre2 flag). ignore_case is case insensitive (-i). fixed_strings is a phrase or literal (-F). word_regexp is whole word (-w). Human OR: lizard OR catfooding. Human AND: lizard AND the (bare words are implicit AND). Phrase: \"hello world\". Regex: /Catfooding/i. Hits group identical packed bodies: one snippet plus an occurrences array (archive, conversation_id, field).",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Human query or /regex/flags. OR: lizard OR catfooding. AND: lizard AND the. Phrase: \"hello world\". Regex: /Catfooding/i. -F / fixed_strings is a literal. Case insensitive: ignore_case or /pattern/i. Whole word: word_regexp."
                    },
                    "ignore_case": {
                        "type": "boolean",
                        "description": "Case insensitive (Unicode). Default false."
                    },
                    "fixed_strings": {
                        "type": "boolean",
                        "description": "Phrase or literal string (-F). Default false."
                    },
                    "word_regexp": {
                        "type": "boolean",
                        "description": "Whole word (-w). Default false."
                    },
                    "service": {
                        "type": "string",
                        "description": "Folder under $HOME/memex, such as agents/grok."
                    },
                    "account": {
                        "type": "string",
                        "description": "Username file stem."
                    },
                    "archive": {
                        "type": "string",
                        "description": "One archive path. Wins over service and account."
                    }
                },
                "required": ["pattern"]
            }),
        ),
        tool(
            "stats",
            "Archive counts. Needs service+account or archive path. Does not print auth keys.",
            json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string" },
                    "account": { "type": "string" },
                    "archive": {
                        "type": "string",
                        "description": "One archive path. Wins over service and account."
                    }
                }
            }),
        ),
        tool(
            "ingest",
            "Ingest Grok dumps, ChatGPT conversations zips and dirs, Facebook DYI, X account archives, Telegram result.json, session JSONL, markdown, Obsidian vaults, agent reports, or session_docs sqlite. Omitting inputs scans $HOME for known export shapes including ChatGPT zips and X archive zips. Omitting service and account infers from input shape. Home scan always infers per source; pass explicit paths to force service or account. Never copy live export JSON into git. Do not ingest grok_oss.db.",
            json!({
                "type": "object",
                "properties": {
                    "inputs": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Export directories, zips, prod-grok-backend.json, ChatGPT conversations-*.json, Facebook DYI, X account archives (data/account.js), Telegram result.json, grok-oss session JSONL, markdown dirs, Obsidian vaults, .agents/reports, or session_search.sqlite. Omit to scan $HOME (max 8 directory levels)."
                    },
                    "service": { "type": "string" },
                    "account": { "type": "string" },
                    "output": {
                        "type": "string",
                        "description": "Output path. Wins over service, account, and shape inference. Error with a home scan (many archives)."
                    }
                }
            }),
        ),
        tool(
            "scoped_path",
            "Resolve service folder plus account stem to an archive path under $HOME/memex.",
            json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string" },
                    "account": { "type": "string" }
                },
                "required": ["service", "account"]
            }),
        ),
        tool(
            "infer_grok_export",
            "Infer service agents/grok and the account file stem from an official Grok dump (user.xUsername). Do not print a live username.",
            json!({
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "Export directory or backend JSON path."
                    }
                },
                "required": ["input"]
            }),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema
    })
}
