use std::fs;
use std::path::{Path, PathBuf};

use majestic::config::Config;
use majestic::ingest::ingest;
use majestic::mcp::{handle_line, handle_value};
use majestic::rpc::{JsonRpcResponse, RpcContext, call_local};
use majestic::scoped_archive_path;
use majestic::{WireFormat, encode_rpc};
use serde_json::{Value, json};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-export.json")
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("majestic-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn operator_memex() -> &'static Path {
    Path::new("/home/hunter/memex")
}

fn operator_leftover_archive() -> PathBuf {
    operator_memex().join("archive.majestic")
}

fn operator_leftover_fingerprint() -> Option<(std::time::SystemTime, u64)> {
    let meta = fs::metadata(operator_leftover_archive()).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn assert_operator_memex_untouched(before: Option<(std::time::SystemTime, u64)>) {
    match before {
        Some((modified, len)) => {
            let after = fs::metadata(operator_leftover_archive())
                .expect("leftover operator archive.majestic must stay untouched");
            assert_eq!(after.len(), len);
            assert_eq!(after.modified().ok(), Some(modified));
        }
        None => {
            assert!(
                !operator_leftover_archive().exists(),
                "MCP search must not write /home/hunter/memex/archive.majestic"
            );
        }
    }
}

fn result_value(resp: &JsonRpcResponse) -> &Value {
    assert!(
        resp.error.is_none(),
        "expected JSON-RPC result, got error {:?}",
        resp.error
    );
    resp.result.as_ref().expect("JSON-RPC result")
}

fn tool_names(list_result: &Value) -> Vec<String> {
    list_result["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name").to_owned())
        .collect()
}

#[test]
fn mcp_initialize_and_tools_list_names_search() {
    let ctx = RpcContext::new("/tmp/majestic-mcp-unused-home");
    let init = handle_value(
        &ctx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "majestic-test", "version": "0"}
            }
        }),
    )
    .expect("initialize is a request, not a notification");
    let init_result = result_value(&init);
    assert_eq!(init.id, json!(1));
    assert_eq!(init_result["serverInfo"]["name"], "memex");
    assert!(
        init_result["protocolVersion"].as_str().is_some(),
        "initialize must name a protocol version"
    );
    assert!(
        init_result["capabilities"].get("tools").is_some(),
        "initialize must advertise tools"
    );

    let list = handle_value(
        &ctx,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        }),
    )
    .expect("tools/list is a request");
    let list_result = result_value(&list);
    let names = tool_names(list_result);
    for required in ["search", "ingest", "stats", "list_archives"] {
        assert!(
            names.iter().any(|name| name == required),
            "tools/list must include {required}, got {names:?}"
        );
    }
    let search = list_result["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .find(|tool| tool["name"] == "search")
        .expect("search tool");
    let props = &search["inputSchema"]["properties"];
    for key in ["pattern", "ignore_case", "fixed_strings", "word_regexp"] {
        assert!(
            props.get(key).is_some(),
            "search tool schema must include {key}, got {props:?}"
        );
    }
    assert!(
        props.get("pcre2").is_none(),
        "search is always PCRE2; do not add a pcre2 flag, got {props:?}"
    );
}

#[test]
fn mcp_handle_line_accepts_toon_tiny_rpc_object() {
    let ctx = RpcContext::new("/tmp/majestic-mcp-toon-home");
    let obj = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "ping",
        "params": {}
    });
    let toon = encode_rpc(&obj, WireFormat::Toon).expect("encode TOON ping");
    assert!(
        !toon.trim_start().starts_with('{'),
        "fixture must be TOON, got {toon:?}"
    );
    let resp = handle_line(&ctx, &toon).expect("TOON ping is a request");
    assert!(
        resp.error.is_none(),
        "TOON ping must succeed, got {:?}",
        resp.error
    );
    assert_eq!(resp.id, json!(1));
}

#[test]
fn mcp_search_all_uses_fake_home() {
    let home = test_dir("mcp-search-fake-home");
    assert!(
        !home.starts_with(operator_memex()),
        "tests must not ingest into the operator memex directory"
    );
    let grok =
        scoped_archive_path(&home.join("memex"), "agents/grok", "fixture").expect("grok scope");
    assert!(
        !grok.starts_with(operator_memex()),
        "scoped archive must stay under the fake home"
    );
    let operator_before = operator_leftover_fingerprint();
    ingest(&grok, &[fixture_path()]).expect("ingest fixture into fake home");

    let ctx = RpcContext::new(&home);
    let resp = handle_value(
        &ctx,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "search",
                "arguments": {
                    "pattern": "Catfooding",
                    "ignore_case": false
                }
            }
        }),
    )
    .expect("tools/call search is a request");
    let result = result_value(&resp);
    assert_ne!(
        result.get("isError"),
        Some(&json!(true)),
        "search all on fake home must succeed, got {result}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .expect("MCP tool content text");
    let payload: Value = serde_json::from_str(text).expect("search content is JSON");
    let hits = payload["hits"].as_array().expect("hits array");
    assert!(
        !hits.is_empty(),
        "fake-home search for Catfooding must hit, got {payload}"
    );
    for hit in hits {
        let archive = hit["archive"].as_str().expect("archive path");
        assert!(
            !archive.starts_with("/home/hunter/memex"),
            "search must not read the operator memex directory, got {archive}"
        );
        assert!(
            Path::new(archive).starts_with(&home),
            "search hits must come from the fake home {home:?}, got {archive}"
        );
        assert!(
            hit["snippet"]
                .as_str()
                .expect("snippet")
                .contains("Catfooding"),
            "snippet must show the stored word, got {:?}",
            hit["snippet"]
        );
    }
    assert_operator_memex_untouched(operator_before);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn list_archives_uses_config_memex_dir_not_home_memex() {
    let home = test_dir("rpc-list-custom-memex-dir");
    assert!(
        !home.starts_with(operator_memex()),
        "tests must not ingest into the operator memex directory"
    );
    let memex_dir = home.join("custom-archives");
    let wanted = scoped_archive_path(&memex_dir, "agents/grok", "fixture").expect("custom scope");
    ingest(&wanted, &[fixture_path()]).expect("ingest into config memex_dir");
    let decoy_dir = home.join("memex").join("agents").join("grok");
    fs::create_dir_all(&decoy_dir).expect("default home/memex decoy");
    let decoy = decoy_dir.join("decoy.majestic");
    fs::write(&decoy, b"").expect("touch decoy under $HOME/memex");

    let mut config = Config::crate_defaults();
    config.memex_dir = memex_dir.clone();
    let ctx = RpcContext::from_home_and_config(&home, config);
    let value =
        call_local(&ctx, "list_archives", &json!({})).expect("list_archives with config memex_dir");
    let archives = value["archives"]
        .as_array()
        .expect("archives array")
        .iter()
        .map(|entry| PathBuf::from(entry.as_str().expect("archive path string")))
        .collect::<Vec<_>>();
    assert!(
        archives.iter().any(|path| path == &wanted),
        "list_archives must use config memex_dir, got {archives:?}"
    );
    assert!(
        !archives.iter().any(|path| path == &decoy),
        "list_archives must not list $HOME/memex when memex_dir is set, got {archives:?}"
    );

    let scoped = call_local(
        &ctx,
        "scoped_path",
        &json!({ "service": "agents/grok", "account": "fixture" }),
    )
    .expect("scoped_path with config memex_dir");
    assert_eq!(
        PathBuf::from(scoped["path"].as_str().expect("path string")),
        wanted,
        "scoped_path must join service under config memex_dir"
    );
    let _ = fs::remove_dir_all(&home);
}
