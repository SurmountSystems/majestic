use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use majestic::MAGIC;
use majestic::archive::{Archive, AssetEntry};
use majestic::hash::{self, Digest};
use majestic::ingest::ingest;
use majestic::schema::JsonAtom;

fn conversation_json(
    conversation_id: &str,
    title: &str,
    modify_time: &str,
    response_id: &str,
    message: &str,
) -> String {
    format!(
        r#"{{
  "conversations": [
    {{
      "conversation": {{
        "id": "{conversation_id}",
        "title": "{title}",
        "create_time": "2026-08-01T12:00:00.000Z",
        "modify_time": "{modify_time}",
        "starred": false,
        "temporary": false,
        "picky_gate": true
      }},
      "responses": [
        {{
          "response": {{
            "_id": "{response_id}",
            "conversation_id": "{conversation_id}",
            "message": "{message}",
            "sender": "assistant",
            "model": "synthetic-model"
          }},
          "share_link": null
        }}
      ]
    }}
  ],
  "media_posts": [],
  "projects": [],
  "tasks": []
}}"#
    )
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-export.json")
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("majestic-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write_tiny_backend(export_dir: &Path) {
    fs::create_dir_all(export_dir).expect("export dir");
    let fixture = fs::read_to_string(fixture_path()).expect("synthetic fixture must exist");
    fs::write(export_dir.join("prod-grok-backend.json"), fixture).unwrap();
}

fn write_asset_content(export_dir: &Path, bucket: &str, uuid: &str, payload: &[u8]) -> PathBuf {
    let dir = export_dir.join(bucket).join(uuid);
    fs::create_dir_all(&dir).expect("asset dir");
    let path = dir.join("content");
    fs::write(&path, payload).unwrap();
    path
}

fn set_unix_mtime(path: &Path, secs: u64) {
    let file = fs::File::options()
        .write(true)
        .open(path)
        .expect("open content to set mtime");
    file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .expect("set mtime");
}

fn stored_asset_digest(archive: &Archive, entry: &AssetEntry) -> Digest {
    let blob = archive.bao_blob().expect("bao blob");
    hash::hash_range(blob, entry.blob_off, entry.blob_len).expect("stored asset hash")
}

fn stored_asset_bytes<'a>(archive: &'a Archive, entry: &AssetEntry) -> &'a [u8] {
    let blob = archive.bao_blob().expect("bao blob");
    let start = usize::try_from(entry.blob_off).expect("blob_off");
    let extra = usize::try_from(entry.blob_len).expect("blob_len");
    blob.get(start..start + extra)
        .expect("asset body range in bao blob")
}

fn assert_payload_in_archive(archive: &Archive, entry: &AssetEntry, payload: &[u8]) {
    assert_eq!(entry.size, payload.len() as u64);
    assert_eq!(entry.blob_len, payload.len() as u64);
    assert_eq!(
        stored_asset_bytes(archive, entry),
        payload,
        "uploaded file body must be stored in the archive blob"
    );
    assert_eq!(
        stored_asset_digest(archive, entry),
        hash::hash_bytes(payload)
    );
}

#[test]
fn ingest_two_exports_keeps_provenance() {
    let dir = test_dir("two-exports");
    let fixture = fs::read_to_string(fixture_path()).expect("synthetic fixture must exist");
    let first = dir.join("export-a.json");
    let second = dir.join("export-b.json");
    fs::write(&first, &fixture).unwrap();
    fs::write(&second, &fixture).unwrap();
    let out = dir.join("two.majestic");

    let report = ingest(&out, &[first.clone(), second.clone()]).expect("ingest two exports");
    assert_eq!(report.exports, 2);
    assert_eq!(report.conversations, 1);

    let archive = Archive::open(&out).expect("open archive");
    let stats = archive.stats().expect("stats");
    assert_eq!(stats.exports, 2);
    assert_eq!(stats.conversations, 1);
    assert_eq!(stats.export_labels.len(), 2);
    assert_ne!(stats.export_labels[0], stats.export_labels[1]);
    assert!(stats.export_labels[0].contains("export-a.json"));
    assert!(stats.export_labels[1].contains("export-b.json"));

    let root = archive.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root.conversations.len(), 1);
    let record = &root.conversations[0];
    assert_eq!(record.export_index, 0);
    assert!(record.export_indices.contains(&0));
    assert!(record.export_indices.contains(&1));
    assert_eq!(record.item.responses.len(), 1);
    assert_eq!(
        record.item.conversation.id.as_deref(),
        Some("synthetic-conversation-1")
    );
    assert_eq!(record.id.as_deref(), Some("synthetic-conversation-1"));
    assert_eq!(record.blob_len, record.byte_len);
    assert!(record.byte_len > 0);
    assert_ne!(root.exports[0].source_path, root.exports[1].source_path);
    assert_eq!(
        archive.bao_blob().expect("bao blob").len() as u64,
        record.blob_len
    );
    assert!(archive.bao_root().expect("bao root").is_some());
}

#[test]
fn same_id_different_bytes_enriches_responses() {
    let dir = test_dir("enrich-responses");
    let first = dir.join("export-a.json");
    let second = dir.join("export-b.json");
    fs::write(
        &first,
        conversation_json(
            "synthetic-conversation-1",
            "Synthetic lossless fixture",
            "2026-08-01T13:00:00.000Z",
            "synthetic-response-1",
            "Catfooding the synthetic fixture.",
        ),
    )
    .unwrap();
    fs::write(
        &second,
        conversation_json(
            "synthetic-conversation-1",
            "Richer later title",
            "2026-08-01T14:00:00.000Z",
            "synthetic-response-2",
            "A second unique message.",
        ),
    )
    .unwrap();
    let out = dir.join("enrich.majestic");

    let report = ingest(&out, &[first, second]).expect("ingest differing bodies");
    assert_eq!(report.exports, 2);
    assert_eq!(report.conversations, 1);

    let archive = Archive::open(&out).expect("open archive");
    let root = archive.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root.conversations.len(), 1);
    let record = &root.conversations[0];
    assert_eq!(record.item.responses.len(), 2);
    let ids: Vec<Option<&str>> = record
        .item
        .responses
        .iter()
        .map(|item| item.response._id.as_deref())
        .collect();
    assert!(ids.contains(&Some("synthetic-response-1")));
    assert!(ids.contains(&Some("synthetic-response-2")));
    assert_eq!(
        record.item.conversation.title.as_deref(),
        Some("Richer later title")
    );
}

#[test]
fn true_duplicate_skips_second_body() {
    let dir = test_dir("true-dup");
    let first = dir.join("export-a.json");
    let second = dir.join("export-b.json");
    let fixture = fs::read_to_string(fixture_path()).expect("synthetic fixture must exist");
    fs::write(&first, &fixture).unwrap();
    fs::write(&second, &fixture).unwrap();
    let out = dir.join("dup.majestic");

    ingest(&out, &[first]).expect("first ingest");
    let report = ingest(&out, &[second]).expect("incremental true duplicate");
    assert_eq!(report.exports, 2);
    assert_eq!(report.conversations, 1);

    let archive = Archive::open(&out).expect("open archive");
    let root = archive.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root.conversations.len(), 1);
    assert_eq!(root.conversations[0].item.responses.len(), 1);
    assert!(root.conversations[0].export_indices.contains(&0));
    assert!(root.conversations[0].export_indices.contains(&1));
}

#[test]
fn bao_tree_not_sha2() {
    let cargo = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
    assert!(
        cargo.contains("bao-tree"),
        "bao-tree must be a direct crates.io dependency"
    );
    assert!(
        cargo.contains("0.16"),
        "bao-tree 0.16 must be pinned in Cargo.toml"
    );
    for line in cargo.lines() {
        let trimmed = line.trim();
        assert!(
            !trimmed.starts_with("sha2"),
            "sha2 must not appear as a dependency"
        );
        assert!(
            !trimmed.starts_with("blake3"),
            "direct blake3 dependency is forbidden; hash through bao-tree"
        );
    }
}

#[test]
fn stats_opens_mmap_without_json() {
    let dir = test_dir("mmap-stats");
    let out = dir.join("from-fixture.majestic");
    ingest(&out, &[fixture_path()]).expect("ingest fixture");

    let bytes = fs::read(&out).expect("archive bytes");
    assert_eq!(&bytes[..MAGIC.len()], MAGIC.as_slice());
    assert_eq!(bytes[MAGIC.len() - 1], 0x01);

    let archive = Archive::open(&out).expect("mmap open");
    let stats = archive.stats().expect("stats from mmap");
    assert!(stats.conversations >= 1);
    assert!(stats.exports >= 1);

    let text = archive.text().expect("utf-8 text blob");
    assert!(text.contains("Catfooding"));
    assert!(text.contains("Synthetic lossless fixture"));

    let root = archive.deserialize_root().expect("rkyv deserialize");
    assert_eq!(
        root.conversations[0]
            .item
            .conversation
            .extra
            .get("picky_gate"),
        Some(&JsonAtom::Bool(true))
    );
}

#[test]
fn ingest_catalogs_asset_server_content_stores_file_body() {
    // Catalog the asset, store the file body, sameness is the BaoTree hash
    // of those bytes (not uuid/size/mtime).
    let dir = test_dir("asset-server-catalog");
    let user = "00000000-0000-4000-8000-000000000001";
    let asset = "00000000-0000-4000-8000-0000000000aa";
    let export_dir = dir.join("ttl/30d/export_data").join(user);
    write_tiny_backend(&export_dir);
    let payload = b"MAJESTIC_SYNTH_ASSET_BODY_9c2e";
    write_asset_content(&export_dir, "prod-mc-asset-server", asset, payload);
    let out = dir.join("assets.majestic");

    ingest(&out, std::slice::from_ref(&dir)).expect("ingest export tree");

    let archive = Archive::open(&out).expect("open archive");
    let stats = archive.stats().expect("stats");
    assert_eq!(stats.assets, 1);
    let root = archive.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root.assets.len(), 1);
    let entry = &root.assets[0];
    assert_eq!(entry.export_index, 0);
    assert_eq!(entry.export_indices, vec![0]);
    assert_eq!(entry.uuid, asset);
    assert_eq!(
        entry.relative_path,
        format!("prod-mc-asset-server/{asset}/content")
    );
    assert_payload_in_archive(&archive, entry, payload);
    let blob = archive.bao_blob().expect("bao blob");
    assert_eq!(
        blob.len() as u64,
        root.conversations[0].blob_len + payload.len() as u64
    );
}

#[test]
fn archive_contains_uploaded_file_bytes() {
    let dir = test_dir("asset-bodies-stored");
    let user = "00000000-0000-4000-8000-000000000001";
    let asset = "00000000-0000-4000-8000-0000000000aa";
    let export_dir = dir.join("ttl/30d/export_data").join(user);
    write_tiny_backend(&export_dir);
    let payload = b"MAJESTIC_SYNTH_ASSET_BODY_STORED_a7e1";
    write_asset_content(&export_dir, "prod-mc-asset-server", asset, payload);
    let out = dir.join("assets.majestic");

    ingest(&out, std::slice::from_ref(&dir)).expect("ingest export tree");

    let archive = Archive::open(&out).expect("open archive");
    let root = archive.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root.assets.len(), 1);
    let entry = &root.assets[0];
    assert_payload_in_archive(&archive, entry, payload);
    let bytes = fs::read(&out).expect("archive bytes");
    assert!(
        bytes.windows(payload.len()).any(|window| window == payload),
        "the .majestic file must contain the uploaded file bytes"
    );
}

#[test]
fn same_asset_bytes_are_one_row_even_if_mtime_differs() {
    let dir = test_dir("same-asset-bytes");
    let user = "00000000-0000-4000-8000-000000000002";
    let uuid_a = "00000000-0000-4000-8000-0000000000aa";
    let uuid_b = "00000000-0000-4000-8000-0000000000bb";
    let export_dir = dir.join("ttl/30d/export_data").join(user);
    write_tiny_backend(&export_dir);
    let payload = b"MAJESTIC_SYNTH_ASSET_SAME_BYTES_4e81";
    let path_a = write_asset_content(&export_dir, "prod-mc-asset-server", uuid_a, payload);
    let path_b = write_asset_content(&export_dir, "prod-mc-asset-server", uuid_b, payload);
    set_unix_mtime(&path_a, 1_700_000_000);
    set_unix_mtime(&path_b, 1_800_000_000);
    let meta_a = fs::metadata(&path_a).expect("meta a");
    let meta_b = fs::metadata(&path_b).expect("meta b");
    assert_ne!(
        meta_a.modified().expect("mtime a"),
        meta_b.modified().expect("mtime b"),
        "fixture mtimes must differ"
    );
    let out = dir.join("same-bytes.majestic");

    ingest(&out, std::slice::from_ref(&dir)).expect("ingest matching bytes");

    let archive = Archive::open(&out).expect("open archive");
    let root = archive.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root.assets.len(), 1, "identical bytes are one catalog row");
    let entry = &root.assets[0];
    assert!(entry.uuid == uuid_a || entry.uuid == uuid_b);
    assert_eq!(entry.size, payload.len() as u64);
    assert_eq!(entry.export_indices, vec![0]);
    assert_payload_in_archive(&archive, entry, payload);
    let blob = archive.bao_blob().expect("bao blob");
    let copies = blob
        .windows(payload.len())
        .filter(|window| *window == payload)
        .count();
    assert_eq!(copies, 1, "same bytes stay one body in the archive");
}

#[test]
fn same_asset_uuid_different_bytes_are_not_merged() {
    let dir = test_dir("same-uuid-different-bytes");
    let user = "00000000-0000-4000-8000-000000000003";
    let uuid = "00000000-0000-4000-8000-0000000000cc";
    let export_dir = dir.join("ttl/30d/export_data").join(user);
    write_tiny_backend(&export_dir);
    let payload_a = b"MAJESTIC_SYNTH_ASSET_A_7f1c";
    let payload_b = b"MAJESTIC_SYNTH_ASSET_B_3d90";
    write_asset_content(&export_dir, "assets", uuid, payload_a);
    write_asset_content(&export_dir, "prod-mc-asset-server", uuid, payload_b);
    let out = dir.join("different-bytes.majestic");

    ingest(&out, std::slice::from_ref(&dir)).expect("ingest same uuid different bytes");

    let archive = Archive::open(&out).expect("open archive");
    let root = archive.deserialize_root().expect("rkyv deserialize");
    assert_eq!(
        root.assets.len(),
        2,
        "different bytes stay two catalog rows"
    );
    assert_eq!(root.assets[0].uuid, uuid);
    assert_eq!(root.assets[1].uuid, uuid);
    let digest_a = stored_asset_digest(&archive, &root.assets[0]);
    let digest_b = stored_asset_digest(&archive, &root.assets[1]);
    assert_ne!(digest_a, digest_b);
    let expected_a = hash::hash_bytes(payload_a);
    let expected_b = hash::hash_bytes(payload_b);
    assert!(
        (digest_a == expected_a && digest_b == expected_b)
            || (digest_a == expected_b && digest_b == expected_a)
    );
    let blob = archive.bao_blob().expect("bao blob");
    assert!(
        blob.windows(payload_a.len())
            .any(|window| window == payload_a),
        "first distinct body must be stored"
    );
    assert!(
        blob.windows(payload_b.len())
            .any(|window| window == payload_b),
        "second distinct body must be stored"
    );
}

#[test]
fn ingest_grok_oss_session_jsonl_keeps_unknown_keys() {
    let dir = test_dir("grok-oss-jsonl");
    let session_src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-session");
    let session = dir.join("synthetic-session-1");
    fs::create_dir_all(&session).expect("session dir");
    fs::copy(
        session_src.join("chat_history.jsonl"),
        session.join("chat_history.jsonl"),
    )
    .expect("copy synthetic chat_history.jsonl");
    fs::copy(
        session_src.join("summary.json"),
        session.join("summary.json"),
    )
    .expect("copy synthetic summary.json");
    let out = dir.join("session.majestic");

    ingest(&out, &[session]).expect("ingest grok-oss session jsonl");

    let archive = Archive::open(&out).expect("open archive");
    let root = archive.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root.conversations.len(), 1);
    let record = &root.conversations[0];
    assert_eq!(record.id.as_deref(), Some("synthetic-session-1"));
    assert_eq!(
        record.item.conversation.id.as_deref(),
        Some("synthetic-session-1")
    );
    assert_eq!(
        record.item.conversation.title.as_deref(),
        Some("Synthetic grok-oss session")
    );
    assert_eq!(
        record.item.conversation.extra.get("picky_summary_gate"),
        Some(&JsonAtom::Bool(true))
    );
    assert_eq!(record.item.responses.len(), 2);
    assert_eq!(
        record.item.responses[0]
            .response
            .extra
            .get("picky_session_gate"),
        Some(&JsonAtom::Bool(true)),
        "unknown JSONL keys must survive on the archive record"
    );
    assert_eq!(
        record.item.responses[0].response.sender.as_deref(),
        Some("user")
    );
    assert_eq!(
        record.item.responses[1].response.sender.as_deref(),
        Some("assistant")
    );
    let text = archive.text().expect("utf-8 text blob");
    assert!(text.contains("synthetic grok-oss user line"));
    assert!(text.contains("synthetic grok-oss assistant line"));
}

#[test]
fn ingest_session_dir_does_not_turn_updates_jsonl_into_responses() {
    let dir = test_dir("grok-oss-updates-jsonl");
    let session_src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-session");
    let session = dir.join("synthetic-session-1");
    fs::create_dir_all(&session).expect("session dir");
    fs::copy(
        session_src.join("chat_history.jsonl"),
        session.join("chat_history.jsonl"),
    )
    .expect("copy synthetic chat_history.jsonl");
    fs::copy(
        session_src.join("summary.json"),
        session.join("summary.json"),
    )
    .expect("copy synthetic summary.json");
    fs::write(
        session.join("updates.jsonl"),
        concat!(
            r#"{"type":"agent_message_chunk","message":"ACP stream leftover, not a chat turn"}"#,
            "\n",
        ),
    )
    .expect("write synthetic updates.jsonl");
    let out = dir.join("session.majestic");

    ingest(&out, &[session]).expect("ingest session dir with sibling updates.jsonl");

    let archive = Archive::open(&out).expect("open archive");
    let root = archive.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root.conversations.len(), 1);
    let record = &root.conversations[0];
    assert_eq!(
        record.item.responses.len(),
        2,
        "only chat_history.jsonl lines are chat turns"
    );
    for item in &record.item.responses {
        match &item.response.message {
            Some(JsonAtom::String(body)) => {
                assert!(
                    !body.is_empty(),
                    "updates.jsonl must not add an empty-message response"
                );
                assert!(
                    !body.contains("ACP stream leftover"),
                    "updates.jsonl ACP lines must not be stored as chat messages"
                );
            }
            other => panic!("chat_history responses must keep string messages, got {other:?}"),
        }
    }
    let leftover = root.exports[0]
        .extra
        .get("updates.jsonl")
        .expect("sibling jsonl stays leftover JSON on the export");
    match leftover {
        JsonAtom::Array(lines) => {
            assert_eq!(lines.len(), 1);
            match &lines[0] {
                JsonAtom::Object(fields) => {
                    assert_eq!(
                        fields.get("message"),
                        Some(&JsonAtom::String(
                            "ACP stream leftover, not a chat turn".to_owned()
                        ))
                    );
                }
                other => panic!("expected leftover object line, got {other:?}"),
            }
        }
        other => panic!("expected leftover jsonl array, got {other:?}"),
    }
}

#[test]
fn ingest_standalone_jsonl_keeps_the_file_as_conversation() {
    let dir = test_dir("standalone-jsonl");
    let notes = dir.join("notes.jsonl");
    fs::write(
        &notes,
        concat!(r#"{"type":"user","content":"standalone jsonl line"}"#, "\n",),
    )
    .expect("write standalone notes.jsonl");
    let out = dir.join("archive.majestic");

    ingest(&out, &[notes]).expect("ingest standalone jsonl");

    let archive = Archive::open(&out).expect("open archive");
    let root = archive.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root.conversations.len(), 1);
    assert_eq!(
        root.conversations[0].item.responses.len(),
        1,
        "a standalone jsonl the operator passed must become the conversation, not an empty record"
    );
    match &root.conversations[0].item.responses[0].response.message {
        Some(JsonAtom::String(body)) => assert_eq!(body, "standalone jsonl line"),
        other => panic!("expected the standalone line as the message, got {other:?}"),
    }
    assert_eq!(
        root.conversations[0].item.responses[0]
            .response
            .sender
            .as_deref(),
        Some("user")
    );
}

#[test]
fn ingest_grok_dir_and_zip_of_same_backend_keep_one_export() {
    let root = test_dir("ingest-grok-dir-and-zip");
    let backend = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "tests/fixtures/tiny-grok-export/ttl/30d/export_data/synth-user/prod-grok-backend.json",
    );
    let zip_path = root.join("tiny-grok.zip");
    {
        let file = File::create(&zip_path).expect("create grok zip");
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("prod-grok-backend.json", options)
            .expect("zip start_file");
        zip.write_all(&fs::read(&backend).expect("read backend"))
            .expect("zip write");
        zip.finish().expect("zip finish");
    }
    let out = root.join("archive.majestic");
    let report = ingest(&out, &[backend.clone(), zip_path.clone()])
        .expect("ingest unpacked backend and zip of the same bytes");
    assert_eq!(
        report.exports, 1,
        "directory file and zip of the same Grok backend keep one export, got {}",
        report.exports
    );
    assert!(backend.is_file(), "must not delete the unpacked dump");
    assert!(zip_path.is_file(), "must not delete the zip dump");
    let _ = fs::remove_dir_all(&root);
}
