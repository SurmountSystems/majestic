//! Synthetic ChatGPT zip and dir ingest. Never copy a live Downloads zip.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use majestic::archive::Archive;
use majestic::ingest::{infer_ingest_scope, ingest};
use majestic::schema::JsonAtom;
use majestic::{resolve_ingest_archive, search};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-chatgpt")
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("majestic-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn process_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .or_else(|| {
            std::env::var("USERNAME")
                .ok()
                .filter(|user| !user.is_empty())
        })
        .expect("USER or USERNAME must be set for process-account inference")
}

fn operator_memex() -> &'static Path {
    Path::new("/home/hunter/memex")
}

fn assert_not_operator_memex(path: &Path) {
    assert!(
        !path.starts_with(operator_memex()),
        "tests must not ingest into the operator memex directory"
    );
}

fn write_fixture_zip(path: &Path) {
    let file = File::create(path).expect("create synthetic chatgpt zip");
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for name in [
        "user.json",
        "conversations-000.json",
        "conversations-001.json",
    ] {
        let body = fs::read(fixtures().join(name)).expect("synthetic chatgpt fixture");
        zip.start_file(name, options).expect("zip start_file");
        zip.write_all(&body).expect("zip write");
    }
    zip.finish().expect("zip finish");
}

fn user_fixture_has_no_email() {
    let raw = fs::read_to_string(fixtures().join("user.json")).expect("user.json");
    assert!(
        !raw.contains('@'),
        "synthetic user.json must not contain an email"
    );
}

#[test]
fn infer_chatgpt_zip_service_is_agents_chatgpt() {
    user_fixture_has_no_email();
    let root = test_dir("infer-chatgpt-zip");
    let zip_path = root.join("tiny-chatgpt.zip");
    write_fixture_zip(&zip_path);

    let scope = infer_ingest_scope(&zip_path).expect("infer chatgpt zip");
    assert_eq!(scope.service, "agents/chatgpt");
    assert_eq!(scope.account, process_user());
    assert!(
        !scope.account.contains('@'),
        "account must be process $USER, never an email"
    );

    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&zip_path))
        .expect("resolve chatgpt zip archive");
    assert_not_operator_memex(&archive);
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    assert_eq!(
        archive,
        home.join("memex")
            .join("agents")
            .join("chatgpt")
            .join(format!("{}.majestic", process_user()))
    );
    let name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .expect("archive file name");
    assert!(
        name.ends_with(".majestic"),
        "agents/chatgpt must use .majestic, got {name}"
    );
    assert!(
        !name.ends_with(".archive"),
        "agents/chatgpt must not write leftover .archive, got {name}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ingest_chatgpt_sharded_conversations_searchable() {
    user_fixture_has_no_email();
    let root = test_dir("ingest-chatgpt-zip");
    let zip_path = root.join("tiny-chatgpt.zip");
    write_fixture_zip(&zip_path);
    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&zip_path))
        .expect("resolve chatgpt archive");
    assert_not_operator_memex(&archive);

    let report =
        ingest(&archive, std::slice::from_ref(&zip_path)).expect("ingest synthetic chatgpt zip");
    assert_eq!(report.conversations, 2, "two sharded conversations");

    let opened = Archive::open(&archive).expect("open chatgpt archive");
    let root_rec = opened.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root_rec.conversations.len(), 2);
    let zero = root_rec
        .conversations
        .iter()
        .find(|record| record.id.as_deref() == Some("synth-chatgpt-convo-0"))
        .expect("shard 0 conversation");
    assert_eq!(
        zero.item.conversation.title.as_deref(),
        Some("Synthetic ChatGPT shard zero")
    );
    assert_eq!(
        zero.item.conversation.extra.get("synth_gate"),
        Some(&JsonAtom::Bool(true)),
        "unknown ChatGPT keys must survive as leftover extra"
    );
    assert!(
        zero.item.conversation.extra.contains_key("mapping"),
        "mapping must stay leftover extra"
    );
    assert!(
        zero.item.conversation.extra.contains_key("title"),
        "title must stay leftover extra"
    );
    assert!(
        zero.item.conversation.extra.contains_key("create_time"),
        "create_time must stay leftover extra"
    );
    assert!(
        zero.item.conversation.extra.contains_key("conversation_id"),
        "conversation_id must stay leftover extra"
    );

    let lizard =
        search(&opened, "chatgpt-search-token-lizard-aa11", false).expect("search lizard token");
    assert!(
        !lizard.is_empty(),
        "content.parts from conversations-000.json must be searchable"
    );
    let shard =
        search(&opened, "chatgpt-search-token-shard-bb22", false).expect("search shard token");
    assert!(
        !shard.is_empty(),
        "content.parts from conversations-001.json must be searchable"
    );
    let the_hits = search(&opened, "the synthetic ChatGPT reply", false).expect("search the");
    assert!(!the_hits.is_empty(), "assistant parts must be searchable");

    let dir_home = root.join("fake-home-dir");
    let dir_archive =
        resolve_ingest_archive(&dir_home, None, None, std::slice::from_ref(&fixtures()))
            .expect("resolve chatgpt dir");
    assert_not_operator_memex(&dir_archive);
    ingest(&dir_archive, std::slice::from_ref(&fixtures())).expect("ingest unzipped chatgpt dir");
    let dir_opened = Archive::open(&dir_archive).expect("open dir archive");
    let dir_hits = search(&dir_opened, "chatgpt-search-token-lizard-aa11", false)
        .expect("search unzipped dir");
    assert!(
        !dir_hits.is_empty(),
        "unzipped ChatGPT dir must ingest the same parts text"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn skip_encrypted_chatgpt_zip_entry_ingests_plaintext_shards() {
    user_fixture_has_no_email();
    let root = test_dir("ingest-chatgpt-encrypted-shard");
    let zip_path = root.join("tiny-chatgpt-encrypted-shard.zip");
    write_chatgpt_zip_with_encrypted_zero_shard(&zip_path);

    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&zip_path))
        .expect("resolve chatgpt archive");
    assert_not_operator_memex(&archive);

    let report = ingest(&archive, std::slice::from_ref(&zip_path)).expect(
        "encrypted conversations-000.json must be skipped; plaintext shards must still ingest",
    );
    assert_eq!(
        report.conversations, 1,
        "only the plaintext conversations-001.json shard must ingest, got {}",
        report.conversations
    );

    let opened = Archive::open(&archive).expect("open chatgpt archive");
    let shard =
        search(&opened, "chatgpt-search-token-shard-bb22", false).expect("search plaintext shard");
    assert!(
        !shard.is_empty(),
        "plaintext conversations-001.json parts must be searchable"
    );
    let lizard =
        search(&opened, "chatgpt-search-token-lizard-aa11", false).expect("search encrypted shard");
    assert!(
        lizard.is_empty(),
        "encrypted conversations-000.json must not be ingested"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ingest_chatgpt_dir_and_zip_of_same_tree_keep_one_export() {
    user_fixture_has_no_email();
    let root = test_dir("ingest-chatgpt-dir-and-zip");
    let dir = root.join("tiny-chatgpt");
    fs::create_dir_all(&dir).expect("chatgpt dir");
    for name in [
        "user.json",
        "conversations-000.json",
        "conversations-001.json",
    ] {
        fs::copy(fixtures().join(name), dir.join(name)).expect("copy chatgpt fixture file");
    }
    let zip_path = root.join("tiny-chatgpt.zip");
    write_fixture_zip(&zip_path);
    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&dir))
        .expect("resolve chatgpt archive");
    assert_not_operator_memex(&archive);

    let report = ingest(&archive, &[dir.clone(), zip_path.clone()])
        .expect("ingest dir and zip of the same tree");
    assert_eq!(
        report.exports, 1,
        "directory and zip of the same dump keep one export, got {}",
        report.exports
    );
    assert_eq!(report.conversations, 2, "two sharded conversations");
    assert!(
        dir.join("user.json").is_file(),
        "must not delete the dir dump"
    );
    assert!(zip_path.is_file(), "must not delete the zip dump");
    let _ = fs::remove_dir_all(&root);
}

fn write_chatgpt_zip_with_encrypted_zero_shard(path: &Path) {
    let file = File::create(path).expect("create synthetic mixed-encryption chatgpt zip");
    let mut zip = zip::ZipWriter::new(file);
    let plain = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let encrypted = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .with_aes_encryption(zip::AesMode::Aes128, "synth-test-password");
    for (name, options) in [
        ("user.json", plain),
        ("conversations-000.json", encrypted),
        ("conversations-001.json", plain),
    ] {
        let body = fs::read(fixtures().join(name)).expect("synthetic chatgpt fixture");
        zip.start_file(name, options).expect("zip start_file");
        zip.write_all(&body).expect("zip write");
    }
    zip.finish().expect("zip finish");
}
