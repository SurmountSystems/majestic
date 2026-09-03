//! Synthetic local-source ingest: reports, Obsidian, markdown, session_docs sqlite.
//!
//! Do not read live `~/.agents/reports`, a live Obsidian vault, or live
//! `session_search.sqlite`. Fixtures under `tests/fixtures/tiny-*` only.

use std::fs;
use std::path::{Path, PathBuf};

use majestic::archive::{Archive, HEADER_LEN};
use majestic::config::Config;
use majestic::ingest::{
    infer_ingest_scope, ingest, ingest_from_flags, ingest_from_flags_with_config, ingest_home,
    ingest_home_with_config,
};
use majestic::list_memex_archives;
use majestic::schema::JsonAtom;
use majestic::{resolve_ingest_archive, search};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
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

fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("copy dest");
    for entry in fs::read_dir(src).expect("read fixture tree") {
        let entry = entry.expect("dir entry");
        let dest = dst.join(entry.file_name());
        let file_type = entry.file_type().expect("file type");
        if file_type.is_dir() {
            copy_tree(&entry.path(), &dest);
        } else if file_type.is_file() {
            fs::copy(entry.path(), dest).expect("copy fixture file");
        }
    }
}

fn write_tiny_session_docs(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("sqlite parent");
    }
    let sql = include_str!("fixtures/tiny-session-docs.sql");
    let conn = rusqlite::Connection::open(path).expect("create synthetic session_docs sqlite");
    conn.execute_batch(sql)
        .expect("apply synthetic session_docs SQL");
}

fn assert_not_operator_memex(path: &Path) {
    assert!(
        !path.starts_with(operator_memex()),
        "tests must not ingest into the operator memex directory"
    );
}

#[test]
fn infer_reports_dir_service_is_agents_reports() {
    let root = test_dir("infer-reports");
    let reports = root.join(".agents").join("reports");
    fs::create_dir_all(&reports).expect("synthetic .agents/reports");
    let scope = infer_ingest_scope(&reports).expect("infer reports dir");
    assert_eq!(scope.service, "agents/reports");
    assert_eq!(scope.account, process_user());
    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&reports))
        .expect("resolve reports archive");
    assert_not_operator_memex(&archive);
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    assert_eq!(
        archive,
        home.join("memex")
            .join("agents")
            .join("reports")
            .join(format!("{}.majestic", process_user()))
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn infer_obsidian_service_is_notes_obsidian() {
    let root = test_dir("infer-obsidian");
    let vault = root.join("Obsidian Vault");
    copy_tree(&fixtures().join("tiny-obsidian"), &vault);
    let scope = infer_ingest_scope(&vault).expect("infer obsidian vault");
    assert_eq!(scope.service, "notes/obsidian");
    assert_eq!(scope.account, "Obsidian Vault");
    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&vault))
        .expect("resolve obsidian archive");
    assert_not_operator_memex(&archive);
    assert_eq!(
        archive,
        home.join("memex")
            .join("notes")
            .join("obsidian")
            .join("Obsidian Vault.majestic")
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn infer_sqlite_service_is_agents_grok_oss() {
    let root = test_dir("infer-sqlite");
    let db = root.join("session_search.sqlite");
    fs::write(&db, b"").expect("touch session_search.sqlite name");
    let scope = infer_ingest_scope(&db).expect("infer sqlite by file name");
    assert_eq!(scope.service, "agents/grok-oss");
    assert_eq!(scope.account, process_user());
    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&db))
        .expect("resolve sqlite archive");
    assert_not_operator_memex(&archive);
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    assert_eq!(
        archive,
        home.join("memex")
            .join("agents")
            .join("grok-oss")
            .join(format!("{}.majestic", process_user()))
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ingest_agents_reports_md_searchable() {
    let root = test_dir("ingest-reports");
    let reports = root.join(".agents").join("reports");
    copy_tree(&fixtures().join("tiny-reports"), &reports);
    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&reports))
        .expect("resolve reports archive");
    assert_not_operator_memex(&archive);
    ingest(&archive, std::slice::from_ref(&reports)).expect("ingest synthetic reports");

    let opened = Archive::open(&archive).expect("open reports archive");
    let root_rec = opened.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root_rec.conversations.len(), 2);
    let alpha = root_rec
        .conversations
        .iter()
        .find(|record| record.id.as_deref() == Some("alpha.md"))
        .expect("alpha.md conversation");
    assert_eq!(
        alpha.item.conversation.title.as_deref(),
        Some("Synthetic reports alpha")
    );
    assert_eq!(
        alpha.item.conversation.extra.get("synthetic_gate"),
        Some(&JsonAtom::Bool(true)),
        "unknown YAML frontmatter must survive as leftover keys"
    );
    assert_eq!(
        alpha.item.conversation.extra.get("leftover_topic"),
        Some(&JsonAtom::String("reports-fixture".to_owned()))
    );

    let hits_alpha = search(&opened, "reports-search-aa11", false).expect("search alpha");
    assert!(
        !hits_alpha.is_empty(),
        "reports ingest must index alpha body text"
    );
    let hits_beta = search(&opened, "reports-search-bb22", false).expect("search beta");
    assert!(
        !hits_beta.is_empty(),
        "reports ingest must index beta body text"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ingest_obsidian_vault_skips_dot_obsidian() {
    let root = test_dir("ingest-obsidian");
    let vault = root.join("Obsidian Vault");
    copy_tree(&fixtures().join("tiny-obsidian"), &vault);
    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&vault))
        .expect("resolve obsidian archive");
    assert_not_operator_memex(&archive);
    ingest(&archive, std::slice::from_ref(&vault)).expect("ingest synthetic vault");

    let opened = Archive::open(&archive).expect("open obsidian archive");
    let root_rec = opened.deserialize_root().expect("rkyv deserialize");
    assert!(
        !root_rec.conversations.is_empty(),
        "vault notes must become conversations"
    );
    for record in &root_rec.conversations {
        let id = record.id.as_deref().unwrap_or("");
        assert!(
            !id.contains(".obsidian"),
            "files under .obsidian must not be notes, got {id}"
        );
    }
    let note_hits = search(&opened, "obsidian-note-search-token-yy88", false).expect("note search");
    assert!(
        !note_hits.is_empty(),
        "Obsidian note body must be searchable"
    );
    let skipped = search(&opened, "obsidian-config-must-not-ingest-zz99", false)
        .expect("dot-obsidian search");
    assert!(
        skipped.is_empty(),
        ".obsidian markdown must not be searchable as notes"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ingest_markdown_dir_without_obsidian() {
    let markdown = fixtures().join("tiny-markdown");
    assert!(
        !markdown.join(".obsidian").exists(),
        "tiny-markdown fixture must not contain .obsidian"
    );
    let root = test_dir("ingest-markdown");
    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&markdown))
        .expect("resolve markdown archive");
    assert_not_operator_memex(&archive);
    assert_eq!(
        archive,
        home.join("memex")
            .join("notes")
            .join("markdown")
            .join("tiny-markdown.majestic")
    );
    ingest(&archive, std::slice::from_ref(&markdown)).expect("ingest markdown dir");

    let opened = Archive::open(&archive).expect("open markdown archive");
    let hits = search(&opened, "markdown-dir-search-token-cc33", false).expect("search markdown");
    assert!(
        !hits.is_empty(),
        "markdown directory ingest must index note body text"
    );
    let nested = search(&opened, "markdown-nested-search-token-ee55", false).expect("nested");
    assert!(
        !nested.is_empty(),
        "nested markdown files must ingest with the directory"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn yaml_front_matter_does_not_eat_the_body() {
    let root = test_dir("yaml-front-matter-body");
    let home = root.join("fake-home");
    let note = fixtures().join("tiny-yaml-body.md");
    let reports = ingest_from_flags(
        &home,
        None,
        Some("notes/markdown"),
        Some("yaml-body"),
        std::slice::from_ref(&note),
    )
    .expect("ingest markdown with YAML front matter");
    let archive = &reports[0].output;
    assert_not_operator_memex(archive);

    let opened = Archive::open(archive).expect("open yaml-body archive");
    let root_rec = opened.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root_rec.conversations.len(), 1);
    let item = &root_rec.conversations[0].item;
    assert_eq!(
        item.conversation.extra.get("leftover_gate"),
        Some(&JsonAtom::Bool(true)),
        "YAML front matter keys must survive as leftover extra"
    );
    assert_eq!(
        item.conversation.extra.get("title"),
        Some(&JsonAtom::String("yaml-frontmatter-title-ee55".to_owned())),
        "YAML front matter title must stay leftover extra, not eat the body"
    );
    let message = item
        .responses
        .first()
        .and_then(|response| response.response.message.as_ref())
        .expect("markdown body is a message");
    match message {
        JsonAtom::String(body) => {
            assert!(
                body.contains("yaml-body-token-ff66"),
                "body after the closing --- must remain, got {body:?}"
            );
            assert!(
                !body.contains("leftover_gate"),
                "YAML front matter must not replace the body, got {body:?}"
            );
        }
        other => panic!("markdown body must be a string, got {other:?}"),
    }
    let body_hits = search(&opened, "yaml-body-token-ff66", false).expect("body search");
    assert!(
        !body_hits.is_empty(),
        "YAML front matter must not eat the unique body after the closing ---"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ingest_session_docs_sqlite_searchable() {
    let root = test_dir("ingest-sqlite");
    let db = root.join("tiny-session-docs.sqlite");
    write_tiny_session_docs(&db);
    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&db))
        .expect("resolve session_docs archive");
    assert_not_operator_memex(&archive);
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    assert!(
        archive.ends_with(format!("agents/grok-oss/{}.majestic", process_user())),
        "session_docs must land under agents/grok-oss with .majestic, got {}",
        archive.display()
    );

    ingest(&archive, std::slice::from_ref(&db)).expect("ingest session_docs");
    let report = ingest(&archive, std::slice::from_ref(&db)).expect("re-ingest same sqlite bytes");
    assert_eq!(
        report.conversations, 1,
        "same session id and same bytes must stay one conversation"
    );

    let opened = Archive::open(&archive).expect("open sqlite archive");
    let root_rec = opened.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root_rec.conversations.len(), 1);
    let record = &root_rec.conversations[0];
    assert_eq!(record.id.as_deref(), Some("synthetic-session-docs-1"));
    assert_eq!(
        record.item.conversation.title.as_deref(),
        Some("Synthetic sqlite session")
    );
    assert_eq!(
        record.item.conversation.extra.get("cwd"),
        Some(&JsonAtom::String("/tmp/synthetic-cwd".to_owned()))
    );
    assert_eq!(
        record.item.conversation.extra.get("content_hash"),
        Some(&JsonAtom::String("synth-hash-aaaa".to_owned()))
    );
    assert_eq!(
        record.item.conversation.extra.get("extra_col"),
        Some(&JsonAtom::String("leftover-sqlite-col".to_owned()))
    );
    let hits = search(&opened, "unique-token-session-docs-dd44", false).expect("search sqlite");
    assert!(!hits.is_empty(), "session_docs content must be searchable");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn telegram_result_json_infers_social_telegram() {
    let root = test_dir("infer-telegram");
    let export = root.join("ChatExport_synth");
    fs::create_dir_all(&export).expect("synthetic ChatExport dir");
    let result = export.join("result.json");
    fs::copy(
        fixtures().join("tiny-telegram").join("result.json"),
        &result,
    )
    .expect("copy synthetic telegram result.json");

    let scope = infer_ingest_scope(&result).expect("infer telegram result.json");
    assert_eq!(scope.service, "social/telegram");
    assert_eq!(scope.account, process_user());

    let from_dir = infer_ingest_scope(&export).expect("infer ChatExport directory");
    assert_eq!(from_dir.service, "social/telegram");
    assert_eq!(from_dir.account, process_user());

    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&result))
        .expect("resolve telegram archive");
    assert_not_operator_memex(&archive);
    assert_eq!(
        archive,
        home.join("memex")
            .join("social")
            .join("telegram")
            .join(format!("{}.majestic", process_user()))
    );

    let account_export = root.join("ChatExport_account");
    fs::create_dir_all(&account_export).expect("synthetic full-account dir");
    let account_result = account_export.join("result.json");
    fs::write(
        &account_result,
        r#"{
  "personal_information": {
    "user_id": 1,
    "username": "synthtelegramuser"
  },
  "chats": {
    "list": [
      {
        "name": "synth chat",
        "type": "personal_chat",
        "id": 1,
        "messages": [
          {
            "id": 1,
            "type": "message",
            "date": "2026-01-01T00:00:00",
            "from": "synth-from",
            "from_id": "user1",
            "text": "Catfooding telegram fixture"
          }
        ]
      }
    ]
  }
}"#,
    )
    .expect("write synthetic full-account result.json");
    let account_scope = infer_ingest_scope(&account_result)
        .expect("infer telegram username from personal_information");
    assert_eq!(account_scope.service, "social/telegram");
    assert_eq!(account_scope.account, "synthtelegramuser");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ingest_telegram_single_chat_searchable() {
    let root = test_dir("ingest-telegram");
    let export = root.join("ChatExport_synth");
    fs::create_dir_all(&export).expect("synthetic ChatExport dir");
    let result = export.join("result.json");
    fs::copy(
        fixtures().join("tiny-telegram").join("result.json"),
        &result,
    )
    .expect("copy synthetic telegram result.json");
    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&result))
        .expect("resolve telegram archive");
    assert_not_operator_memex(&archive);
    ingest(&archive, std::slice::from_ref(&result)).expect("ingest synthetic telegram");
    let report =
        ingest(&archive, std::slice::from_ref(&result)).expect("re-ingest same telegram bytes");
    assert_eq!(
        report.conversations, 1,
        "same telegram chat id and same bytes must stay one conversation"
    );

    let opened = Archive::open(&archive).expect("open telegram archive");
    let root_rec = opened.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root_rec.conversations.len(), 1);
    let record = &root_rec.conversations[0];
    assert_eq!(record.id.as_deref(), Some("1"));
    assert_eq!(
        record.item.conversation.title.as_deref(),
        Some("synth chat")
    );
    let response = record
        .item
        .responses
        .first()
        .expect("telegram message becomes a response");
    assert_eq!(
        response.response.extra.get("from"),
        Some(&JsonAtom::String("synth-from".to_owned()))
    );
    assert_eq!(
        response.response.extra.get("from_id"),
        Some(&JsonAtom::String("user1".to_owned()))
    );
    assert_eq!(
        response.response.extra.get("date"),
        Some(&JsonAtom::String("2026-01-01T00:00:00".to_owned()))
    );
    let hits = search(&opened, "Catfooding telegram fixture", false).expect("search telegram");
    assert!(!hits.is_empty(), "telegram message body must be searchable");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn home_scan_finds_telegram_and_skips_git() {
    let home = test_dir("home-scan-telegram");
    assert_not_operator_memex(&home);

    let export = home
        .join("Downloads")
        .join("Telegram Desktop")
        .join("ChatExport_synth");
    fs::create_dir_all(&export).expect("synthetic ChatExport under fake HOME");
    fs::copy(
        fixtures().join("tiny-telegram").join("result.json"),
        export.join("result.json"),
    )
    .expect("copy synthetic telegram result.json");

    let git_trap = home.join(".git").join("ChatExport_trap");
    fs::create_dir_all(&git_trap).expect("synthetic .git trap");
    fs::write(
        git_trap.join("result.json"),
        r#"{
  "name": "trap chat",
  "type": "personal_chat",
  "id": 99,
  "messages": [
    {
      "id": 1,
      "type": "message",
      "date": "2026-01-01T00:00:00",
      "from": "trap-from",
      "from_id": "user99",
      "text": "must-not-ingest-git-trap"
    }
  ]
}"#,
    )
    .expect("write telegram-shaped trap under .git");

    let notes = home.join("Notes");
    fs::create_dir_all(&notes).expect("markdown notes dir");
    fs::write(notes.join("plain.md"), "must-not-ingest-home-markdown\n")
        .expect("write markdown that home scan must ignore");

    let memex_trap = home.join("memex").join("social").join("telegram");
    fs::create_dir_all(&memex_trap).expect("fake memex dest");
    fs::write(
        memex_trap.join("result.json"),
        r#"{
  "name": "memex trap",
  "type": "personal_chat",
  "id": 77,
  "messages": [
    {
      "id": 1,
      "type": "message",
      "date": "2026-01-01T00:00:00",
      "from": "trap-from",
      "from_id": "user77",
      "text": "must-not-ingest-memex-source"
    }
  ]
}"#,
    )
    .expect("write telegram-shaped trap under fake HOME/memex");

    let output_err = ingest_from_flags(
        &home,
        Some(Path::new("/tmp/majestic-home-scan-must-not-write.majestic")),
        None,
        None,
        &[],
    )
    .expect_err("home scan with -o is an error");
    assert!(
        output_err.to_string().contains("-o") || output_err.to_string().contains("many archives"),
        "home scan plus -o must error in plain English, got {output_err}"
    );

    let service_err = ingest_from_flags(&home, None, Some("social/telegram"), None, &[])
        .expect_err("home scan with --service is skipped");
    assert!(
        service_err.to_string().contains("--service")
            || service_err.to_string().contains("explicit paths"),
        "home scan plus --service must tell the operator to pass explicit paths, got {service_err}"
    );

    let reports = ingest_home(&home).expect("home scan fake HOME");
    assert_eq!(
        reports.len(),
        1,
        "one inferred telegram archive, got {reports:?}"
    );
    let archive = &reports[0].output;
    assert_not_operator_memex(archive);
    assert!(
        archive.starts_with(&home),
        "home scan must write under the injected home, got {}",
        archive.display()
    );
    assert!(
        archive.ends_with(format!("social/telegram/{}.majestic", process_user())),
        "telegram home scan must land social/telegram/<USER>.majestic, got {}",
        archive.display()
    );
    assert!(
        !archive.starts_with(operator_memex()),
        "home scan must not write /home/hunter/memex"
    );

    let listed = list_memex_archives(&home.join("memex")).expect("list scanned archives");
    assert_eq!(
        listed,
        vec![archive.clone()],
        "home scan must not ingest arbitrary markdown or skip-list traps, got {listed:?}"
    );

    let opened = Archive::open(archive).expect("open scanned telegram archive");
    let hits =
        search(&opened, "Catfooding telegram fixture", false).expect("search scanned telegram");
    assert!(
        !hits.is_empty(),
        "home scan must ingest the ChatExport telegram fixture"
    );
    let git_hits = search(&opened, "must-not-ingest-git-trap", false).expect("search git trap");
    assert!(git_hits.is_empty(), ".git must not be walked as a source");
    let memex_hits =
        search(&opened, "must-not-ingest-memex-source", false).expect("search memex trap");
    assert!(
        memex_hits.is_empty(),
        "$HOME/memex must not be walked as a source"
    );
    let md_hits = search(&opened, "must-not-ingest-home-markdown", false).expect("search markdown");
    assert!(
        md_hits.is_empty(),
        "home scan must not ingest arbitrary markdown trees"
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn chatexport_result_json_is_telegram_not_grok_even_when_peek_fails() {
    let home = test_dir("chatexport-not-grok");
    assert_not_operator_memex(&home);
    let export = home.join("Downloads").join("ChatExport_2026-05-06");
    fs::create_dir_all(&export).expect("ChatExport dir");
    let result = export.join("result.json");
    fs::write(
        &result,
        r#"{"about":"Telegram HTML dump without chat keys"}"#,
    )
    .expect("write about-only result.json");

    let scope = infer_ingest_scope(&result).expect("ChatExport_ result.json is Telegram");
    assert_eq!(scope.service, "social/telegram");
    assert!(
        !scope.service.contains("grok"),
        "must not classify ChatExport_ as Grok"
    );

    let reports = ingest_home(&home).expect("home scan classifies ChatExport_ as telegram");
    assert_eq!(reports.len(), 1, "one telegram archive, got {reports:?}");
    assert!(
        reports[0]
            .output
            .ends_with(format!("social/telegram/{}.majestic", process_user())),
        "ChatExport_ must land social/telegram, got {}",
        reports[0].output.display()
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ingest_telegram_two_chats_are_two_conversations() {
    let root = test_dir("telegram-two-chats-ingest");
    let export = root.join("ChatExport_two");
    fs::create_dir_all(&export).expect("ChatExport dir");
    let result = export.join("result.json");
    fs::write(
        &result,
        r#"{
  "personal_information": {"user_id": 1, "username": "synthtelegramuser"},
  "chats": {
    "list": [
      {
        "name": "first chat",
        "type": "personal_chat",
        "id": 1,
        "messages": [{"id": 1, "date": "2026-01-01T00:00:00", "text": "alpha token"}]
      },
      {
        "name": "second chat",
        "type": "personal_chat",
        "id": 2,
        "messages": [{"id": 1, "date": "2026-01-01T00:00:01", "text": "beta token"}]
      }
    ]
  }
}"#,
    )
    .expect("write two-chat result.json");
    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&result))
        .expect("resolve two-chat archive");
    assert_not_operator_memex(&archive);
    let report = ingest(&archive, std::slice::from_ref(&result)).expect("ingest two chats");
    assert_eq!(
        report.conversations, 2,
        "two chats.list entries are two conversations, got {}",
        report.conversations
    );
    assert_eq!(report.exports, 1);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ingest_telegram_overlapping_dumps_keep_unique_chats() {
    let root = test_dir("telegram-overlap-chats");
    let first = root.join("ChatExport_one");
    let second = root.join("ChatExport_two");
    fs::create_dir_all(&first).expect("first ChatExport");
    fs::create_dir_all(&second).expect("second ChatExport");
    let body = r#"{
  "name": "shared chat",
  "type": "personal_chat",
  "id": 1,
  "messages": [{"id": 1, "date": "2026-01-01T00:00:00", "text": "same bytes"}]
}"#;
    fs::write(first.join("result.json"), body).expect("write first dump");
    fs::write(second.join("result.json"), body).expect("write overlapping dump");
    let home = root.join("fake-home");
    let a = first.join("result.json");
    let b = second.join("result.json");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&a))
        .expect("resolve overlapping telegram");
    assert_not_operator_memex(&archive);
    let report = ingest(&archive, &[a, b]).expect("ingest overlapping dumps");
    assert_eq!(
        report.conversations, 1,
        "same chat id and same bytes stay one unique chat, got {}",
        report.conversations
    );
    assert_eq!(
        report.exports, 2,
        "two dumps still count as two export dumps, got {}",
        report.exports
    );
    let _ = fs::remove_dir_all(&root);
}

fn write_telegram_trap(dir: &Path, chat_id: u32, token: &str) {
    fs::create_dir_all(dir).expect("telegram trap dir");
    fs::write(
        dir.join("result.json"),
        format!(
            r#"{{
  "name": "trap chat {chat_id}",
  "type": "personal_chat",
  "id": {chat_id},
  "messages": [{{"id": 1, "date": "2026-01-01T00:00:00", "text": "{token}"}}]
}}"#
        ),
    )
    .expect("write telegram trap");
}

#[test]
fn home_scan_skips_sandbox_blocked_dir_not_agents_trash() {
    let home = test_dir("home-scan-skip-sandbox-not-agents");
    assert_not_operator_memex(&home);

    let ok = home
        .join("Downloads")
        .join("Telegram Desktop")
        .join("ChatExport_ok");
    fs::create_dir_all(&ok).expect("ok ChatExport");
    fs::copy(
        fixtures().join("tiny-telegram").join("result.json"),
        ok.join("result.json"),
    )
    .expect("copy ok telegram");

    let blocked = home.join(".grok").join("sandbox-blocked-dir.0");
    write_telegram_trap(&blocked, 50, "must-not-ingest-sandbox-blocked");

    let trash = home.join(".agents").join("trash").join("ChatExport_trash");
    write_telegram_trap(&trash, 51, "must-ingest-agents-trash-on-crate-defaults");

    let other_grok = home.join(".grok").join("other").join("ChatExport_hidden");
    write_telegram_trap(&other_grok, 52, "must-not-ingest-grok-tree");

    let reports = ingest_home(&home).expect("home scan crate defaults");
    assert_eq!(
        reports.len(),
        1,
        "one telegram archive from Downloads plus agents trash, got {reports:?}"
    );
    let opened = Archive::open(&reports[0].output).expect("open scanned archive");
    let hits = search(&opened, "Catfooding telegram fixture", false).expect("search ok");
    assert!(!hits.is_empty(), "Downloads ChatExport must ingest");
    let agents_hits = search(&opened, "must-ingest-agents-trash-on-crate-defaults", false)
        .expect("search agents trash");
    assert!(
        !agents_hits.is_empty(),
        "crate defaults must ingest ~/.agents/trash"
    );
    for token in [
        "must-not-ingest-sandbox-blocked",
        "must-not-ingest-grok-tree",
    ] {
        let miss = search(&opened, token, false).expect("search trap");
        assert!(miss.is_empty(), "{token} must not ingest");
    }
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn home_scan_skips_xdg_trash_not_agents_trash_unless_listed() {
    let home = test_dir("home-scan-xdg-trash-agents");
    assert_not_operator_memex(&home);

    let ok = home
        .join("Downloads")
        .join("Telegram Desktop")
        .join("ChatExport_ok");
    fs::create_dir_all(&ok).expect("ok ChatExport");
    fs::copy(
        fixtures().join("tiny-telegram").join("result.json"),
        ok.join("result.json"),
    )
    .expect("copy ok telegram");

    let xdg_files = home
        .join(".local")
        .join("share")
        .join("Trash")
        .join("files");
    fs::create_dir_all(&xdg_files).expect("XDG Trash files");
    fs::write(xdg_files.join("secret.json"), "{\"secret\":true}\n").expect("write secret.json");
    write_telegram_trap(
        &xdg_files.join("ChatExport_xdg"),
        60,
        "must-not-ingest-xdg-trash",
    );

    let mount_trash = home
        .join(".Trash-1000")
        .join("files")
        .join("ChatExport_mnt");
    write_telegram_trap(&mount_trash, 61, "must-not-ingest-mount-trash");

    let user_trash = home.join(".Trash").join("files").join("ChatExport_user");
    write_telegram_trap(&user_trash, 62, "must-not-ingest-user-trash");

    let agents = home.join(".agents").join("trash").join("ChatExport_agents");
    write_telegram_trap(&agents, 63, "must-ingest-agents-trash-on-crate-defaults");

    let default_reports = ingest_home(&home).expect("home scan crate defaults");
    assert_eq!(
        default_reports.len(),
        1,
        "one telegram archive on crate defaults, got {default_reports:?}"
    );
    let default_opened =
        Archive::open(&default_reports[0].output).expect("open default scanned archive");
    assert!(
        !search(&default_opened, "Catfooding telegram fixture", false)
            .expect("search ok")
            .is_empty(),
        "Downloads ChatExport must ingest"
    );
    assert!(
        !search(
            &default_opened,
            "must-ingest-agents-trash-on-crate-defaults",
            false
        )
        .expect("search agents trash")
        .is_empty(),
        "crate defaults must ingest ~/.agents/trash"
    );
    for token in [
        "must-not-ingest-xdg-trash",
        "must-not-ingest-mount-trash",
        "must-not-ingest-user-trash",
        "secret.json",
    ] {
        let miss = search(&default_opened, token, false).expect("search trash trap");
        assert!(
            miss.is_empty(),
            "{token} must not ingest under crate-default system trash skip"
        );
    }

    let _ = fs::remove_dir_all(home.join("memex"));
    let mut listed = Config::crate_defaults();
    listed.scan.skip_directories = vec![home.join(".agents").join("trash")];
    let listed_reports =
        ingest_home_with_config(&home, &listed).expect("home scan with skip_directories");
    assert_eq!(
        listed_reports.len(),
        1,
        "Downloads only when skip_directories lists agents trash, got {listed_reports:?}"
    );
    let listed_opened = Archive::open(&listed_reports[0].output).expect("open listed-skip archive");
    assert!(
        !search(&listed_opened, "Catfooding telegram fixture", false)
            .expect("search ok listed")
            .is_empty(),
        "Downloads ChatExport must ingest when skip_directories is set"
    );
    let agents_miss = search(
        &listed_opened,
        "must-ingest-agents-trash-on-crate-defaults",
        false,
    )
    .expect("search listed agents trash");
    assert!(
        agents_miss.is_empty(),
        "listed skip_directories must skip ~/.agents/trash"
    );
    let _ = fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[test]
fn home_scan_unreadable_sandbox_blocked_dirs_do_not_abort() {
    use std::os::unix::fs::PermissionsExt;
    let home = test_dir("home-scan-mode-000");
    assert_not_operator_memex(&home);
    let ok = home
        .join("Downloads")
        .join("Telegram Desktop")
        .join("ChatExport_ok");
    fs::create_dir_all(&ok).expect("ok ChatExport");
    fs::copy(
        fixtures().join("tiny-telegram").join("result.json"),
        ok.join("result.json"),
    )
    .expect("copy ok telegram");

    let grok = home.join(".grok");
    fs::create_dir_all(&grok).expect(".grok");
    let mut blocked = Vec::new();
    for i in 0..32 {
        let dir = grok.join(format!("sandbox-blocked-dir.{i}"));
        fs::create_dir_all(&dir).expect("sandbox-blocked-dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).expect("mode 000");
        blocked.push(dir);
    }

    let result = ingest_home(&home);
    for dir in &blocked {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    }
    let reports = result.expect("mode 000 sandbox dirs must not abort home scan");
    assert_eq!(reports.len(), 1);
    let opened = Archive::open(&reports[0].output).expect("open");
    let hits = search(&opened, "Catfooding telegram fixture", false).expect("search");
    assert!(!hits.is_empty());
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ingest_fails_when_existing_archive_is_not_majestic_v1() {
    let dir = test_dir("fail-old-magic");
    // Leftover `.archive` is still a readable path; ingest still fails on old magic.
    let out = dir.join("old.archive");
    let mut bogus = vec![b'X'; HEADER_LEN];
    bogus[..8].copy_from_slice(b"NOTMAGIC");
    fs::write(&out, &bogus).expect("write header-sized file with wrong magic");
    let fixture = fixtures().join("tiny-telegram/result.json");
    let err = ingest(&out, std::slice::from_ref(&fixture)).expect_err("old magic must fail");
    assert!(
        matches!(err, majestic::Error::InvalidMagic),
        "existing non-v1 archive must fail ingest so the operator can delete it, got {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn ingest_from_flags_writes_under_config_memex_dir() {
    let root = test_dir("ingest-config-memex-dir");
    let home = root.join("fake-home");
    fs::create_dir_all(&home).expect("fake home");
    let memex_dir = root.join("custom-archives");
    let dump = root.join("dump");
    fs::create_dir_all(&dump).expect("dump dir");
    fs::copy(
        fixtures().join("tiny-markdown").join("plain.md"),
        dump.join("plain.md"),
    )
    .expect("copy markdown dump");

    let mut config = Config::crate_defaults();
    config.memex_dir = memex_dir.clone();
    let reports = ingest_from_flags_with_config(
        &home,
        None,
        None,
        None,
        std::slice::from_ref(&dump),
        &config,
    )
    .expect("ingest explicit dump with config memex_dir");
    assert_eq!(reports.len(), 1);
    let output = &reports[0].output;
    assert!(
        output.starts_with(&memex_dir),
        "explicit ingest must write under config memex_dir, got {}",
        output.display()
    );
    assert!(
        !output.starts_with(home.join("memex")),
        "must not write $HOME/memex when memex_dir is set, got {}",
        output.display()
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn explicit_markdown_dump_honors_skip_directories() {
    let root = test_dir("dump-skip-markdown");
    let home = root.join("fake-home");
    fs::create_dir_all(&home).expect("fake home");
    let dump = root.join("dump");
    fs::create_dir_all(dump.join("keep")).expect("keep dir");
    fs::write(
        dump.join("keep").join("keep.md"),
        "# keep\nkeep-token-aa11\n",
    )
    .expect("write keep note");
    let skip_me = dump.join("skip-me");
    fs::create_dir_all(&skip_me).expect("skip dir");
    fs::write(skip_me.join("secret.md"), "# secret\nskip-token-bb22\n")
        .expect("write skipped note");

    let mut config = Config::crate_defaults();
    config.memex_dir = home.join("memex");
    config.scan.skip_directories = vec![skip_me.clone()];
    let reports = ingest_from_flags_with_config(
        &home,
        None,
        None,
        None,
        std::slice::from_ref(&dump),
        &config,
    )
    .expect("ingest markdown dump");
    let opened = Archive::open(&reports[0].output).expect("open dump archive");
    let kept = search(&opened, "keep-token-aa11", false).expect("search keep");
    assert!(
        !kept.is_empty(),
        "notes outside skip_directories must ingest"
    );
    let skipped = search(&opened, "skip-token-bb22", false).expect("search skip");
    assert!(
        skipped.is_empty(),
        "explicit dump ingest must honor skip_directories, got {skipped:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn explicit_dump_walk_honors_skip_directories() {
    let root = test_dir("dump-skip-walk");
    let home = root.join("fake-home");
    fs::create_dir_all(&home).expect("fake home");
    let dump = root.join("dump");
    let keep = dump.join("keep").join("ChatExport_keep");
    fs::create_dir_all(&keep).expect("keep telegram dir");
    fs::copy(
        fixtures().join("tiny-telegram").join("result.json"),
        keep.join("result.json"),
    )
    .expect("copy keep telegram");
    let skip_me = dump.join("skip-me").join("ChatExport_skip");
    fs::create_dir_all(&skip_me).expect("skip telegram dir");
    let skip_json = serde_json::json!({
        "name": "skipped chat",
        "type": "personal_chat",
        "id": 2,
        "messages": [{
            "id": 1,
            "type": "message",
            "date": "2026-01-01T00:00:00",
            "from": "synth-from",
            "from_id": "user1",
            "text": "skip-walk-token-cc33"
        }]
    });
    fs::write(
        skip_me.join("result.json"),
        serde_json::to_vec_pretty(&skip_json).expect("skip json"),
    )
    .expect("write skipped telegram");

    let mut config = Config::crate_defaults();
    config.memex_dir = home.join("memex");
    config.scan.skip_directories = vec![dump.join("skip-me")];
    let reports = ingest_from_flags_with_config(
        &home,
        None,
        None,
        None,
        std::slice::from_ref(&dump),
        &config,
    )
    .expect("ingest nested dump");
    let opened = Archive::open(&reports[0].output).expect("open dump archive");
    let kept = search(&opened, "Catfooding telegram fixture", false).expect("search keep");
    assert!(
        !kept.is_empty(),
        "nested dump outside skip_directories must ingest"
    );
    let skipped = search(&opened, "skip-walk-token-cc33", false).expect("search skip walk");
    assert!(
        skipped.is_empty(),
        "walk_sources must honor skip_directories, got {skipped:?}"
    );
    let _ = fs::remove_dir_all(&root);
}
