//! JSON and TOON request bodies for MCP, ACP, HTTP `/mcp`, and stdio RPC.
//!
//! Default is JSON for humans. [Token-Oriented Object Notation (TOON)](https://github.com/toon-format/spec)
//! (accessed: 2026-08-27) is the compact encoding for language-model tools.
//! This crate uses the maintained [`toon_format`] crate (official Rust
//! implementation). It does not invent a dialect.
//!
//! Media type is `text/toon` (UTF-8), from the spec. HTTP uses `Content-Type`
//! on the request and `Accept` or `format=toon` on the response. CLI `--toon`
//! on `memex mcp`, `memex acp`, and `memex serve` forces TOON responses.
//! Stdio default is one JSON object per line. `--toon` uses one TOON document
//! then a blank line. HTTP bodies are one document; the server sniffs JSON
//! (`{` or `[`) versus TOON when `Content-Type` is unset.

use serde::Serialize;
use serde_json::Value;

/// Wire encoding for one RPC document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireFormat {
    /// JSON (RFC 8259). Default for humans.
    Json,
    /// TOON. See [the TOON spec](https://github.com/toon-format/spec)
    /// (accessed: 2026-08-27).
    Toon,
}

impl WireFormat {
    /// JSON media type.
    pub const JSON_MEDIA: &'static str = "application/json";
    /// TOON media type from the spec (`text/toon`, provisional, UTF-8).
    pub const TOON_MEDIA: &'static str = "text/toon";

    /// Parse a `Content-Type` or `Accept` media type (parameters ignored).
    pub fn from_media_type(raw: &str) -> Option<Self> {
        let media = raw
            .split(';')
            .next()
            .unwrap_or(raw)
            .trim()
            .to_ascii_lowercase();
        match media.as_str() {
            "application/json" | "text/json" => Some(Self::Json),
            "text/toon" | "application/toon" => Some(Self::Toon),
            _ => None,
        }
    }

    /// `format=toon` or `format=json` query value.
    pub fn from_query_format(value: Option<&str>) -> Option<Self> {
        match value.map(|value| value.trim().to_ascii_lowercase()) {
            Some(ref value) if value == "toon" => Some(Self::Toon),
            Some(ref value) if value == "json" => Some(Self::Json),
            _ => None,
        }
    }

    /// Response encoding. `--toon` wins, then `format=`, then `Accept`, then
    /// the request encoding.
    pub fn response(
        prefer_toon: bool,
        query: Option<&str>,
        accept: Option<&str>,
        request: Self,
    ) -> Self {
        if prefer_toon {
            return Self::Toon;
        }
        if let Some(query) = Self::from_query_format(query) {
            return query;
        }
        if let Some(accept) = accept {
            for part in accept.split(',') {
                if let Some(format) = Self::from_media_type(part) {
                    return format;
                }
            }
        }
        request
    }

    /// `Content-Type` value for a response in this format.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Json => Self::JSON_MEDIA,
            Self::Toon => "text/toon; charset=utf-8",
        }
    }
}

fn looks_like_json(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with('{') || text.starts_with('[')
}

/// Parse one RPC document. JSON if the text starts with `{` or `[`, else TOON.
pub fn parse_rpc_value(text: &str) -> Result<(Value, WireFormat), String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("empty request".to_owned());
    }
    if looks_like_json(trimmed) {
        return serde_json::from_str(trimmed)
            .map(|value| (value, WireFormat::Json))
            .map_err(|error| error.to_string());
    }
    match toon_format::decode_default::<Value>(trimmed) {
        Ok(value) => Ok((value, WireFormat::Toon)),
        Err(toon_error) => serde_json::from_str(trimmed)
            .map(|value| (value, WireFormat::Json))
            .map_err(|_| toon_error.to_string()),
    }
}

/// Encode one RPC document as JSON or TOON.
pub fn encode_rpc(value: &impl Serialize, format: WireFormat) -> Result<String, String> {
    match format {
        WireFormat::Json => serde_json::to_string(value).map_err(|error| error.to_string()),
        WireFormat::Toon => toon_format::encode_default(value).map_err(|error| error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tiny_rpc() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "ping",
            "params": {}
        })
    }

    #[test]
    fn toon_roundtrip_tiny_rpc_object() {
        let obj = tiny_rpc();
        let toon = encode_rpc(&obj, WireFormat::Toon).expect("encode TOON");
        assert!(
            !toon.trim_start().starts_with('{'),
            "TOON encoding must not look like JSON, got {toon:?}"
        );
        assert!(
            toon.contains("ping") || toon.contains("method"),
            "TOON must carry the method, got {toon:?}"
        );
        let (back, format) = parse_rpc_value(&toon).expect("parse TOON");
        assert_eq!(format, WireFormat::Toon);
        assert_eq!(back["method"], "ping");
        assert_eq!(back["id"], 1);
        assert_eq!(back["jsonrpc"], "2.0");
    }

    #[test]
    fn json_object_is_not_sniffed_as_toon() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let (value, format) = parse_rpc_value(line).expect("JSON");
        assert_eq!(format, WireFormat::Json);
        assert_eq!(value["method"], "ping");
    }

    #[test]
    fn content_type_text_toon_selects_toon() {
        assert_eq!(
            WireFormat::from_media_type("text/toon; charset=utf-8"),
            Some(WireFormat::Toon)
        );
        assert_eq!(
            WireFormat::from_query_format(Some("toon")),
            Some(WireFormat::Toon)
        );
        assert_eq!(
            WireFormat::response(false, Some("toon"), None, WireFormat::Json),
            WireFormat::Toon
        );
        assert_eq!(
            WireFormat::response(false, None, Some("application/json"), WireFormat::Toon),
            WireFormat::Json
        );
    }
}
