//! Shared JSON-RPC 2.0 types and local-function dispatch.
//!
//! One JSON object per line on stdin/stdout. Tracing stays on stderr.
//! This crate stays small: no MCP/ACP SDK.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Error;
use crate::archive::Archive;
use crate::config::Config;
use crate::home_dir;
use crate::ingest::{infer_grok_export_scope, ingest_from_flags_with_config};
use crate::list_memex_archives;
use crate::scoped_archive_path;
use crate::wire::{self, WireFormat};
use crate::{SearchExec, SearchFlags, SearchOrigin, search_default_exec, search_with};

/// JSON-RPC 2.0 parse error.
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC 2.0 method not found.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC 2.0 invalid params.
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC 2.0 application error (product [`Error`]).
pub const APPLICATION_ERROR: i64 = -32000;

/// Canonical local functions. MCP tools and ACP methods use these names.
pub const LOCAL_FUNCTION_METHODS: &[&str] = &[
    "list_archives",
    "search",
    "stats",
    "ingest",
    "scoped_path",
    "infer_grok_export",
];

/// Home and loaded config used to resolve archives. Tests pass a fake directory.
#[derive(Debug, Clone)]
pub struct RpcContext {
    /// Process home (usually `$HOME`). Home scan walks this tree.
    pub home: PathBuf,
    /// Archive directory (`memex_dir`, default `home/memex`).
    pub memex_dir: PathBuf,
    /// Layered memex.toml after tilde expansion.
    pub config: Config,
    /// When true, stdio and HTTP respond in TOON (`memex mcp --toon`).
    pub prefer_toon: bool,
}

impl RpcContext {
    /// Use this home and crate defaults (`home/memex`).
    pub fn new(home: impl AsRef<Path>) -> Self {
        Self::from_home_and_config(home, Config::crate_defaults())
    }

    /// Use this home and this config. Expands tildes in `config` against `home`.
    pub fn from_home_and_config(home: impl AsRef<Path>, mut config: Config) -> Self {
        let home = home.as_ref().to_path_buf();
        config.expand_tildes(&home);
        let memex_dir = config.memex_dir.clone();
        Self {
            home,
            memex_dir,
            config,
            prefer_toon: false,
        }
    }

    /// Respond in TOON when `prefer_toon` is true.
    pub fn with_prefer_toon(mut self, prefer_toon: bool) -> Self {
        self.prefer_toon = prefer_toon;
        self
    }

    /// Read `HOME` and load memex.toml. Empty or unset home is [`Error::HomeUnset`].
    pub fn from_env() -> Result<Self, Error> {
        let home = home_dir()?;
        let config = Config::load().unwrap_or_else(|_| Config::crate_defaults());
        Ok(Self::from_home_and_config(home, config))
    }
}

/// One JSON-RPC 2.0 request object.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// One JSON-RPC 2.0 response object.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorObject>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
}

impl JsonRpcResponse {
    /// Success response.
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Error response.
    pub fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: None,
            error: Some(JsonRpcErrorObject {
                code,
                message: message.into(),
            }),
        }
    }

    /// Parse error with `id` null.
    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::err(Value::Null, PARSE_ERROR, message)
    }
}

/// Respond to a request, or `None` for a notification.
pub fn respond_ok(request: &JsonRpcRequest, result: Value) -> Option<JsonRpcResponse> {
    Some(JsonRpcResponse::ok(request.id.clone()?, result))
}

/// JSON-RPC error tied to the request id when present.
pub fn respond_err(
    request: &JsonRpcRequest,
    code: i64,
    message: impl Into<String>,
) -> Option<JsonRpcResponse> {
    Some(JsonRpcResponse::err(
        request.id.clone().unwrap_or(Value::Null),
        code,
        message,
    ))
}

/// Map a product error onto a JSON-RPC error object.
pub fn product_rpc_error(request: &JsonRpcRequest, error: &Error) -> Option<JsonRpcResponse> {
    let code = match error {
        Error::UnknownMethod(_) => METHOD_NOT_FOUND,
        Error::InvalidParams(_) => INVALID_PARAMS,
        _ => APPLICATION_ERROR,
    };
    respond_err(request, code, error.to_string())
}

/// Stdio RPC loop. Default is one JSON object per line. `--toon` uses one
/// TOON document then a blank line. Responses match the request encoding
/// unless `ctx.prefer_toon`.
pub fn serve_ndjson<R, W, F>(
    ctx: &RpcContext,
    mut input: R,
    mut output: W,
    mut handle: F,
) -> Result<(), Error>
where
    R: BufRead,
    W: Write,
    F: FnMut(&RpcContext, Value) -> Option<JsonRpcResponse>,
{
    if ctx.prefer_toon {
        serve_toon_records(ctx, &mut input, &mut output, &mut handle)
    } else {
        serve_json_lines(ctx, input, &mut output, &mut handle)
    }
}

fn serve_json_lines<R, W, F>(
    ctx: &RpcContext,
    input: R,
    output: &mut W,
    handle: &mut F,
) -> Result<(), Error>
where
    R: BufRead,
    W: Write,
    F: FnMut(&RpcContext, Value) -> Option<JsonRpcResponse>,
{
    for line in input.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        dispatch_document(ctx, line, WireFormat::Json, output, handle)?;
    }
    Ok(())
}

fn serve_toon_records<R, W, F>(
    ctx: &RpcContext,
    input: &mut R,
    output: &mut W,
    handle: &mut F,
) -> Result<(), Error>
where
    R: BufRead,
    W: Write,
    F: FnMut(&RpcContext, Value) -> Option<JsonRpcResponse>,
{
    let mut buf = String::new();
    loop {
        let mut line = String::new();
        let n = input.read_line(&mut line)?;
        if n == 0 {
            if !buf.trim().is_empty() {
                dispatch_document(ctx, buf.trim(), WireFormat::Toon, output, handle)?;
            }
            break;
        }
        if line.trim().is_empty() {
            if !buf.trim().is_empty() {
                dispatch_document(ctx, buf.trim(), WireFormat::Toon, output, handle)?;
                buf.clear();
            }
            continue;
        }
        buf.push_str(&line);
    }
    Ok(())
}

fn dispatch_document<W, F>(
    ctx: &RpcContext,
    text: &str,
    default_format: WireFormat,
    output: &mut W,
    handle: &mut F,
) -> Result<(), Error>
where
    W: Write,
    F: FnMut(&RpcContext, Value) -> Option<JsonRpcResponse>,
{
    match wire::parse_rpc_value(text) {
        Ok((value, request_format)) => {
            let format = if ctx.prefer_toon {
                WireFormat::Toon
            } else {
                request_format
            };
            if let Some(resp) = handle(ctx, value) {
                write_rpc_document(output, &resp, format)?;
            }
        }
        Err(err) => {
            let format = if ctx.prefer_toon {
                WireFormat::Toon
            } else {
                default_format
            };
            write_rpc_document(output, &JsonRpcResponse::parse_error(err), format)?;
        }
    }
    Ok(())
}

fn write_rpc_document<W: Write>(
    output: &mut W,
    resp: &JsonRpcResponse,
    format: WireFormat,
) -> Result<(), Error> {
    let body = wire::encode_rpc(resp, format).map_err(Error::InvalidParams)?;
    output.write_all(body.as_bytes())?;
    if !body.ends_with('\n') {
        output.write_all(b"\n")?;
    }
    if format == WireFormat::Toon {
        output.write_all(b"\n")?;
    }
    output.flush()?;
    Ok(())
}

/// Dispatch a local function by name.
///
/// `search` with no service, account, or archive path searches every archive
/// under `ctx.memex_dir`. Home live-scan is ingest's job.
/// `ingest` with no service, account, or output infers
/// from input shape (Grok dump, ChatGPT zip/dir, Facebook DYI, X account
/// archive, Telegram result.json, Obsidian, sqlite, reports, markdown).
/// Empty `inputs` scans `ctx.home` for known export shapes.
pub fn call_local(ctx: &RpcContext, method: &str, params: &Value) -> Result<Value, Error> {
    let empty = Value::Object(serde_json::Map::new());
    let params = match params {
        Value::Null => &empty,
        Value::Object(_) => params,
        _ => {
            return Err(Error::InvalidParams(
                "params must be a JSON object".to_owned(),
            ));
        }
    };
    match method {
        "list_archives" => list_archives_rpc(ctx),
        "search" => search_rpc(ctx, params),
        "stats" => stats_rpc(ctx, params),
        "ingest" => ingest_rpc(ctx, params),
        "scoped_path" => scoped_path_rpc(ctx, params),
        "infer_grok_export" => infer_rpc(params),
        other => Err(Error::UnknownMethod(other.to_owned())),
    }
}

fn list_archives_rpc(ctx: &RpcContext) -> Result<Value, Error> {
    let archives = list_memex_archives(&ctx.memex_dir)?;
    Ok(json!({ "archives": archives }))
}

fn search_rpc(ctx: &RpcContext, params: &Value) -> Result<Value, Error> {
    let pattern = req_str(params, "pattern")?;
    let flags = SearchFlags {
        ignore_case: opt_bool(params, "ignore_case")?.unwrap_or(false),
        fixed_strings: opt_bool(params, "fixed_strings")?.unwrap_or(false),
        word_regexp: opt_bool(params, "word_regexp")?.unwrap_or(false),
    };
    let max_count = opt_usize(params, "max_count")?.unwrap_or(ctx.config.search.max_count);
    match search_target(ctx, params)? {
        SearchTarget::One(path) => search_one_json(&path, pattern, flags, max_count),
        SearchTarget::All => search_all_json(&ctx.memex_dir, pattern, flags, max_count),
    }
}

fn stats_rpc(ctx: &RpcContext, params: &Value) -> Result<Value, Error> {
    let path = archive_from_params(ctx, params)?;
    let archive = Archive::open(&path)?;
    let stats = archive.stats()?;
    Ok(json!({
        "path": stats.path,
        "version": stats.version,
        "exports": stats.exports,
        "conversations": stats.conversations,
        "media_posts": stats.media_posts,
        "projects": stats.projects,
        "tasks": stats.tasks,
        "auth_files": stats.auth_files,
        "billing_files": stats.billing_files,
        "assets": stats.assets,
        "payload_bytes": stats.payload_bytes,
        "text_bytes": stats.text_bytes,
        "spans": stats.spans,
        "export_labels": stats.export_labels,
    }))
}

fn ingest_rpc(ctx: &RpcContext, params: &Value) -> Result<Value, Error> {
    let inputs = opt_paths(params, "inputs")?;
    let service = opt_str(params, "service")?;
    let account = opt_str(params, "account")?;
    let output = opt_str(params, "output")?.map(PathBuf::from);
    let reports = ingest_from_flags_with_config(
        &ctx.home,
        output.as_deref(),
        service,
        account,
        &inputs,
        &ctx.config,
    )?;
    if reports.len() == 1 {
        let report = &reports[0];
        return Ok(json!({
            "output": report.output,
            "exports": report.exports,
            "conversations": report.conversations,
        }));
    }
    let exports: usize = reports.iter().map(|report| report.exports).sum();
    let conversations: usize = reports.iter().map(|report| report.conversations).sum();
    Ok(json!({
        "reports": reports.iter().map(|report| json!({
            "output": report.output,
            "exports": report.exports,
            "conversations": report.conversations,
        })).collect::<Vec<_>>(),
        "exports": exports,
        "conversations": conversations,
    }))
}

fn scoped_path_rpc(ctx: &RpcContext, params: &Value) -> Result<Value, Error> {
    let service = req_str(params, "service")?;
    let account = req_str(params, "account")?;
    let path = scoped_archive_path(&ctx.memex_dir, service, account)?;
    Ok(json!({ "path": path }))
}

fn infer_rpc(params: &Value) -> Result<Value, Error> {
    let input = req_str(params, "input")?;
    let inferred = infer_grok_export_scope(Path::new(input))?;
    Ok(json!({
        "service": inferred.service,
        "account": inferred.account,
    }))
}

enum SearchTarget {
    All,
    One(PathBuf),
}

fn search_target(ctx: &RpcContext, params: &Value) -> Result<SearchTarget, Error> {
    if let Some(path) = opt_str(params, "archive")? {
        return Ok(SearchTarget::One(PathBuf::from(path)));
    }
    match (opt_str(params, "service")?, opt_str(params, "account")?) {
        (Some(service), Some(account)) => Ok(SearchTarget::One(scoped_archive_path(
            &ctx.memex_dir,
            service,
            account,
        )?)),
        (None, None) => Ok(SearchTarget::All),
        _ => Err(Error::ArchiveUnspecified),
    }
}

fn archive_from_params(ctx: &RpcContext, params: &Value) -> Result<PathBuf, Error> {
    if let Some(path) = opt_str(params, "archive")? {
        return Ok(PathBuf::from(path));
    }
    match (opt_str(params, "service")?, opt_str(params, "account")?) {
        (Some(service), Some(account)) => scoped_archive_path(&ctx.memex_dir, service, account),
        _ => Err(Error::ArchiveUnspecified),
    }
}

fn search_one_json(
    path: &Path,
    pattern: &str,
    flags: SearchFlags,
    max_count: usize,
) -> Result<Value, Error> {
    let hits = {
        let archive = Archive::open(path)?;
        search_with(&archive, pattern, flags)?
    };
    let unique = hits.len();
    let hits = cap_hits(hits, max_count);
    tracing::info!(
        archive = %path.display(),
        unique,
        printed = hits.len(),
        "search finished"
    );
    let hits: Vec<Value> = hits
        .into_iter()
        .map(|hit| {
            json!({
                "archive": path,
                "conversation_id": hit.conversation_id,
                "field": hit.field,
                "snippet": hit.snippet,
            })
        })
        .collect();
    Ok(json!({ "hits": hits }))
}

fn search_all_json(
    home: &Path,
    pattern: &str,
    flags: SearchFlags,
    max_count: usize,
) -> Result<Value, Error> {
    let groups = search_default_exec(home, pattern, SearchExec { flags, max_count })?;
    let mut hits = Vec::new();
    let mut unique = 0usize;
    let mut duplicate_omitted = 0usize;
    for group in groups {
        unique = unique.saturating_add(group.hits.len());
        duplicate_omitted = duplicate_omitted.saturating_add(group.duplicate_omitted);
        for hit in cap_hits(group.hits, max_count) {
            let mut row = json!({
                "conversation_id": hit.conversation_id,
                "field": hit.field,
                "snippet": hit.snippet,
            });
            match group.origin {
                SearchOrigin::Archive => {
                    row["archive"] = json!(group.path);
                }
            }
            hits.push(row);
        }
    }
    tracing::info!(
        unique,
        printed = hits.len(),
        duplicate_omitted,
        "search finished"
    );
    Ok(json!({ "hits": hits }))
}

fn cap_hits<T>(mut hits: Vec<T>, max_count: usize) -> Vec<T> {
    if max_count == 0 || hits.len() <= max_count {
        hits
    } else {
        hits.truncate(max_count);
        hits
    }
}

fn req_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, Error> {
    match params.get(key) {
        Some(Value::String(s)) => Ok(s.as_str()),
        Some(_) => Err(Error::InvalidParams(format!("{key} must be a string"))),
        None => Err(Error::InvalidParams(format!("missing {key}"))),
    }
}

fn opt_str<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, Error> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(Error::InvalidParams(format!("{key} must be a string"))),
    }
}

fn opt_bool(params: &Value, key: &str) -> Result<Option<bool>, Error> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(Error::InvalidParams(format!("{key} must be a boolean"))),
    }
}

fn opt_usize(params: &Value, key: &str) -> Result<Option<usize>, Error> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            let Some(value) = n.as_u64() else {
                return Err(Error::InvalidParams(format!(
                    "{key} must be a non-negative integer"
                )));
            };
            usize::try_from(value)
                .map(Some)
                .map_err(|_| Error::InvalidParams(format!("{key} is too large")))
        }
        Some(_) => Err(Error::InvalidParams(format!(
            "{key} must be a non-negative integer"
        ))),
    }
}

fn opt_paths(params: &Value, key: &str) -> Result<Vec<PathBuf>, Error> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let Some(s) = item.as_str() else {
                    return Err(Error::InvalidParams(format!(
                        "{key} entries must be strings"
                    )));
                };
                out.push(PathBuf::from(s));
            }
            Ok(out)
        }
        Some(_) => Err(Error::InvalidParams(format!(
            "{key} must be an array of paths"
        ))),
    }
}
