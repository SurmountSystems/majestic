use std::fs;
use std::path::PathBuf;

use majestic::schema::{BackendExport, JsonAtom, Timestamp};
use serde_json::Value;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-export.json")
}

fn load_export() -> BackendExport {
    let raw = fs::read_to_string(fixture_path()).expect("synthetic fixture must exist");
    serde_json::from_str(&raw).expect("synthetic fixture must parse")
}

#[test]
fn lossless_unknown_key_roundtrip() {
    let parsed = load_export();
    assert_eq!(
        parsed.conversations[0].conversation.extra.get("picky_gate"),
        Some(&JsonAtom::Bool(true))
    );

    let value = serde_json::to_value(&parsed).expect("typed export must serialize");
    assert_eq!(
        value.pointer("/conversations/0/conversation/picky_gate"),
        Some(&Value::Bool(true))
    );
}

#[test]
fn bson_date_not_dropped() {
    let parsed = load_export();
    let create_time = parsed.conversations[0].responses[0]
        .response
        .create_time
        .as_ref()
        .expect("response create_time must parse");
    match create_time {
        Timestamp::BsonDate { date } => match date {
            JsonAtom::Object(fields) => {
                assert_eq!(
                    fields.get("$numberLong"),
                    Some(&JsonAtom::String("1234567890123".to_owned()))
                );
            }
            other => panic!("expected BSON $date object, got {other:?}"),
        },
        other => panic!("expected BSON $date wrapper, got {other:?}"),
    }

    let value = serde_json::to_value(&parsed).expect("typed export must serialize");
    assert_eq!(
        value.pointer("/conversations/0/responses/0/response/create_time/$date/$numberLong"),
        Some(&Value::String("1234567890123".to_owned()))
    );
}
