use std::fs;
use std::path::{Path, PathBuf};

use majestic::archive::{Archive, TEXT_PAGE_BYTES};
use majestic::ingest::ingest;
use majestic::{
    SearchExec, SearchFlags, scoped_archive_path, search, search_all_archives, search_with,
    write_search_all, write_search_all_exec,
};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-export.json")
}

fn telegram_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-telegram/result.json")
}

fn copy_telegram_export(home: &Path, export_dir_name: &str) -> PathBuf {
    let export_dir = home
        .join("Downloads")
        .join("Telegram Desktop")
        .join(export_dir_name);
    fs::create_dir_all(&export_dir).expect("telegram export dir");
    let dest = export_dir.join("result.json");
    fs::copy(telegram_fixture_path(), &dest).expect("copy Catfooding telegram fixture");
    dest
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

fn memex_dir(home: &Path) -> PathBuf {
    home.join("memex")
}

fn write_grok_export(path: &Path, rows: &[(&str, &[&str])]) {
    let mut conversations = Vec::new();
    for (i, (id, messages)) in rows.iter().enumerate() {
        let mut responses = Vec::new();
        for (j, message) in messages.iter().enumerate() {
            responses.push(format!(
                r#"{{"response":{{"_id":"r-{i}-{j}","conversation_id":"{id}","message":"{message}","sender":"assistant"}}}}"#
            ));
        }
        conversations.push(format!(
            r#"{{"conversation":{{"id":"{id}","title":"Synthetic {id}"}},"responses":[{}]}}"#,
            responses.join(",")
        ));
    }
    let body = format!(
        r#"{{"conversations":[{}],"media_posts":[],"projects":[],"tasks":[]}}"#,
        conversations.join(",")
    );
    fs::write(path, body).expect("write synthetic grok export");
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
                "search must not write /home/hunter/memex/archive.majestic"
            );
        }
    }
}

#[test]
fn search_case_sensitive_skips_wrong_case() {
    let dir = test_dir("search-cs");
    let out = dir.join("archive.majestic");
    ingest(&out, &[fixture_path()]).expect("ingest fixture");
    let archive = Archive::open(&out).expect("mmap open");

    let miss = search(&archive, "catfooding", false).expect("case-sensitive miss");
    assert!(
        miss.is_empty(),
        "case-sensitive search for catfooding must skip Catfooding"
    );

    let hits = search(&archive, "Catfooding", false).expect("case-sensitive hit");
    assert!(
        !hits.is_empty(),
        "case-sensitive search for Catfooding must hit"
    );
    assert!(
        archive.text_slice_is_mmap_view(),
        "search must use the mmap text slice, not a heap copy of the blob"
    );
    assert_eq!(
        hits[0].conversation_id.as_deref(),
        Some("synthetic-conversation-1")
    );
    assert_eq!(hits[0].field, "message");
    assert!(hits[0].snippet.contains("Catfooding"));
}

#[test]
fn search_ignore_case_hits_folded() {
    let dir = test_dir("search-ci");
    let out = dir.join("archive.majestic");
    ingest(&out, &[fixture_path()]).expect("ingest fixture");
    let archive = Archive::open(&out).expect("mmap open");

    let hits = search(&archive, "catfooding", true).expect("ignore-case hit");
    assert!(
        !hits.is_empty(),
        "ignore-case search for catfooding must hit Catfooding"
    );
    assert_eq!(
        hits[0].conversation_id.as_deref(),
        Some("synthetic-conversation-1")
    );
    assert_eq!(hits[0].field, "message");
    assert!(hits[0].snippet.contains("Catfooding"));
}

#[test]
fn search_finds_string_inside_a_word() {
    let dir = test_dir("search-inside-word");
    let out = dir.join("archive.majestic");
    ingest(&out, &[fixture_path()]).expect("ingest fixture");
    let archive = Archive::open(&out).expect("mmap open");

    let hits = search(&archive, "food", false).expect("inside-word hit");
    assert!(
        !hits.is_empty(),
        "search for food must find the string inside Catfooding"
    );
    assert_eq!(
        hits[0].conversation_id.as_deref(),
        Some("synthetic-conversation-1")
    );
    assert_eq!(hits[0].field, "message");
    assert!(
        hits[0].snippet.contains("Catfooding"),
        "snippet must still show the stored word, got {:?}",
        hits[0].snippet
    );
}

#[test]
fn search_ignore_case_finds_string_inside_a_word() {
    let dir = test_dir("search-inside-word-ci");
    let out = dir.join("archive.majestic");
    ingest(&out, &[fixture_path()]).expect("ingest fixture");
    let archive = Archive::open(&out).expect("mmap open");

    let hits = search(&archive, "FOOD", true).expect("inside-word ignore-case hit");
    assert!(
        !hits.is_empty(),
        "ignore-case search for FOOD must find the string inside Catfooding"
    );
    assert_eq!(
        hits[0].conversation_id.as_deref(),
        Some("synthetic-conversation-1")
    );
    assert_eq!(hits[0].field, "message");
    assert!(
        hits[0].snippet.contains("Catfooding"),
        "snippet must still show the stored word, got {:?}",
        hits[0].snippet
    );
}

#[test]
fn search_regex_metacharacters_run_on_mmap_text() {
    let dir = test_dir("search-regex");
    let out = dir.join("archive.majestic");
    ingest(&out, &[fixture_path()]).expect("ingest fixture");
    let archive = Archive::open(&out).expect("mmap open");

    let hits = search(&archive, r"C.tfooding", false).expect("regex hit");
    assert!(
        !hits.is_empty(),
        "regex C.tfooding must match Catfooding in the mmap text"
    );
    assert_eq!(hits[0].field, "message");
    assert!(hits[0].snippet.contains("Catfooding"));

    let leftover = search(&archive, "picky_gate", false).expect("leftover JSON key");
    assert!(
        leftover.is_empty(),
        "leftover JSON keys must not be searchable"
    );
}

#[test]
fn search_fixed_strings_does_not_treat_dot_as_regex() {
    let dir = test_dir("search-fixed-strings");
    let out = dir.join("archive.majestic");
    ingest(&out, &[fixture_path()]).expect("ingest fixture");
    let archive = Archive::open(&out).expect("mmap open");

    let miss = search_with(
        &archive,
        r"C.tfooding",
        SearchFlags {
            fixed_strings: true,
            ..SearchFlags::default()
        },
    )
    .expect("fixed-string miss");
    assert!(
        miss.is_empty(),
        "-F C.tfooding must not treat the dot as regex, so it must miss Catfooding"
    );

    let hits = search(&archive, r"C.tfooding", false).expect("regex hit");
    assert!(
        !hits.is_empty(),
        "without -F, regex C.tfooding must still match Catfooding"
    );
}

#[test]
fn search_word_regexp_does_not_match_inside_catfooding() {
    let dir = test_dir("search-word-regexp");
    let out = dir.join("archive.majestic");
    ingest(&out, &[fixture_path()]).expect("ingest fixture");
    let archive = Archive::open(&out).expect("mmap open");

    let miss = search_with(
        &archive,
        "food",
        SearchFlags {
            word_regexp: true,
            ..SearchFlags::default()
        },
    )
    .expect("word-regexp miss");
    assert!(miss.is_empty(), "-w food must not match inside Catfooding");

    let hits = search(&archive, "food", false).expect("inside-word hit");
    assert!(
        !hits.is_empty(),
        "without -w, food must still find the string inside Catfooding"
    );
}

#[test]
fn search_pcre2_lookahead_and_requires_both_words() {
    let dir = test_dir("search-pcre2-and");
    let out = dir.join("archive.majestic");
    ingest(&out, &[fixture_path()]).expect("ingest fixture");
    let archive = Archive::open(&out).expect("mmap open");

    let hits = search(&archive, "(?=.*Catfooding)(?=.*fixture)", false).expect("lookahead AND hit");
    assert!(
        !hits.is_empty(),
        "(?=.*Catfooding)(?=.*fixture) must hit the fixture line that has both words"
    );
    assert!(
        hits.iter().any(|hit| hit.snippet.contains("Catfooding")),
        "AND hit snippet must show Catfooding, got {hits:?}"
    );

    let miss = search(&archive, "(?=.*Catfooding)(?=.*nope)", false).expect("lookahead AND miss");
    assert!(
        miss.is_empty(),
        "(?=.*Catfooding)(?=.*nope) must miss when the second word is absent"
    );
}

#[test]
fn search_indexes_text_part_not_discriminator() {
    let dir = test_dir("search-text-part");
    let session = dir.join("synthetic-session-text-part");
    fs::create_dir_all(&session).expect("session dir");
    fs::write(
        session.join("chat_history.jsonl"),
        concat!(
            r#"{"type":"user","content":[{"type":"text","text":"Catfooding the synthetic fixture."},{"type":"image_url","image_url":{"url":"https://example.invalid/synthetic.png"}}]}"#,
            "\n",
        ),
    )
    .expect("write array-content jsonl");
    let out = dir.join("archive.majestic");
    ingest(&out, &[session]).expect("ingest array-content session");
    let archive = Archive::open(&out).expect("mmap open");

    let food = search(&archive, "food", false).expect("text-part hit");
    assert!(
        !food.is_empty(),
        "search for food must hit the type=text part body"
    );
    assert!(food[0].snippet.contains("Catfooding"));

    let disc = search(&archive, "text", false).expect("discriminator miss");
    assert!(
        disc.is_empty(),
        "content-part discriminator text must not become a searchable span"
    );
    let url = search(&archive, "example.invalid", false).expect("image url miss");
    assert!(
        url.is_empty(),
        "image URLs must not become searchable spans"
    );
}

#[test]
fn search_all_archives_uses_memex_dir_not_home_join_memex() {
    let home = test_dir("search-all-custom-memex-dir");
    assert!(
        !home.starts_with(operator_memex()),
        "tests must not ingest into the operator memex directory"
    );
    let memex_dir = home.join("custom-archives");
    let grok = scoped_archive_path(&memex_dir, "agents/grok", "a").expect("grok scope");
    ingest(&grok, &[fixture_path()]).expect("ingest into custom memex_dir");
    let decoy_dir = home.join("memex").join("agents").join("grok");
    fs::create_dir_all(&decoy_dir).expect("default home/memex decoy");
    fs::write(decoy_dir.join("decoy.majestic"), b"not a majestic archive").expect("decoy");

    let groups = search_all_archives(&memex_dir, "Catfooding", false)
        .expect("search the configured data directory");
    assert!(
        groups
            .iter()
            .any(|(path, hits)| path == &grok && !hits.is_empty()),
        "must hit the archive under memex_dir, got {groups:?}"
    );
    assert!(
        groups
            .iter()
            .all(|(path, _)| !path.starts_with(home.join("memex"))),
        "must not list $HOME/memex when memex_dir is elsewhere, got {groups:?}"
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn new_archive_text_starts_on_two_mebibyte_file_offset() {
    let dir = test_dir("text-two-mib-offset");
    let out = dir.join("archive.majestic");
    ingest(&out, &[fixture_path()]).expect("ingest fixture");
    let archive = Archive::open(&out).expect("mmap open");
    assert_eq!(
        archive.text_file_offset() % TEXT_PAGE_BYTES as u64,
        0,
        "new ingest must start the UTF-8 text on a 2 MiB file offset, got {}",
        archive.text_file_offset()
    );
    assert!(
        archive.text_slice_is_mmap_view(),
        "search must use the mmap text slice, not a heap copy of the blob"
    );
    #[cfg(target_os = "linux")]
    {
        let text = archive.text_slice().expect("text slice");
        assert_eq!(
            text.as_ptr() as usize % TEXT_PAGE_BYTES,
            0,
            "text virtual address must be 2 MiB aligned so transparent huge pages can back the text"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn madvise_collapse_failure_does_not_abort_open() {
    let dir = test_dir("collapse-fail-open");
    let out = dir.join("archive.majestic");
    ingest(&out, &[fixture_path()]).expect("ingest fixture");
    Archive::open(&out).expect("MADV_COLLAPSE failure must not abort Archive::open");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn search_all_archives_in_fake_home_hits_both() {
    // Systemwide search maps every listed archive, holds those maps, compiles
    // PCRE2 once, then searches already-mapped text. There is no four-map cap.
    // This test (one query, two archives) replaced
    // search_archive_jobs_caps_unbounded_parallelism.
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    let home = test_dir("search-all-fake-home");
    assert!(
        !home.starts_with(operator_memex()),
        "tests must not ingest into the operator memex directory"
    );
    let memex = memex_dir(&home);
    let grok = scoped_archive_path(&memex, "agents/grok", "a").expect("grok scope");
    let social = scoped_archive_path(&memex, "social/x", "b").expect("social scope");
    assert!(
        !grok.starts_with(operator_memex()) && !social.starts_with(operator_memex()),
        "scoped archives must stay under the fake home"
    );
    let operator_before = operator_leftover_fingerprint();
    ingest(&grok, &[fixture_path()]).expect("ingest grok archive");
    ingest(&social, &[fixture_path()]).expect("ingest social archive");

    let groups = search_all_archives(&memex, "Catfooding", false).expect("search every archive");
    let grok_hits = groups
        .iter()
        .find(|(path, _)| path == &grok)
        .map(|(_, hits)| hits.as_slice())
        .expect("hits from agents/grok/a.majestic");
    let social_hits = groups
        .iter()
        .find(|(path, _)| path == &social)
        .map(|(_, hits)| hits.as_slice())
        .expect("hits from social/x/b.majestic");
    assert!(
        !grok_hits.is_empty(),
        "all-archive search must hit the grok archive"
    );
    assert!(
        !social_hits.is_empty(),
        "all-archive search must hit the social archive"
    );
    assert!(
        groups
            .iter()
            .all(|(path, _)| !path.starts_with(operator_memex())),
        "search must not open archives under /home/hunter/memex"
    );

    let mut printed = Vec::new();
    write_search_all(&memex, "Catfooding", false, &mut printed).expect("print labeled hits");
    let printed = String::from_utf8(printed).expect("search output is utf-8");
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    let grok_header = printed.find("agents/grok/a.majestic\n");
    let social_header = printed.find("social/x/b.majestic\n");
    assert!(
        grok_header.is_some(),
        "report must put the grok archive path on its own line, got {printed:?}"
    );
    assert!(
        social_header.is_some(),
        "report must put the social archive path on its own line, got {printed:?}"
    );
    let grok_at = grok_header.expect("grok header");
    let social_at = social_header.expect("social header");
    let first = grok_at.min(social_at);
    let second = grok_at.max(social_at);
    let between = &printed[first..second];
    assert!(
        between.contains("Catfooding"),
        "snippet must sit under its archive path, got {printed:?}"
    );
    assert!(printed.contains("Catfooding"));
    assert_operator_memex_untouched(operator_before);
}

#[test]
fn search_all_archives_skips_corrupt_and_hits_readable() {
    let home = test_dir("search-all-skips-junk");
    assert!(
        !home.starts_with(operator_memex()),
        "tests must not ingest into the operator memex directory"
    );
    let grok = scoped_archive_path(&memex_dir(&home), "agents/grok", "a").expect("grok scope");
    let operator_before = operator_leftover_fingerprint();
    ingest(&grok, &[fixture_path()]).expect("ingest readable archive");
    let junk_dir = home.join("memex").join("broken");
    fs::create_dir_all(&junk_dir).expect("junk dir");
    fs::write(junk_dir.join("junk.majestic"), b"not a majestic archive").expect("write junk");

    let groups = search_all_archives(&memex_dir(&home), "Catfooding", false)
        .expect("corrupt files must not abort the whole search");
    assert!(
        groups
            .iter()
            .any(|(path, hits)| path == &grok && !hits.is_empty()),
        "readable archive must still hit, got {groups:?}"
    );
    assert!(
        groups.iter().all(|(path, _)| path == &grok),
        "corrupt junk.majestic must be skipped, got {groups:?}"
    );
    assert_operator_memex_untouched(operator_before);
}

#[test]
fn search_all_archives_empty_memex_is_plain_error() {
    let home = test_dir("search-all-empty-memex");
    fs::create_dir_all(home.join("memex")).expect("empty memex dir");
    let err = search_all_archives(&memex_dir(&home), "Catfooding", false)
        .expect_err("empty memex dir must not search");
    let text = err.to_string();
    assert!(
        text.contains("no archives found"),
        "empty memex dir must say no archives found, got {text}"
    );
}

#[test]
fn default_search_empty_memex_errors_even_with_live_telegram() {
    let home = test_dir("default-search-empty-memex");
    assert!(
        !home.starts_with(operator_memex()),
        "tests must not ingest into the operator memex directory"
    );
    let telegram = copy_telegram_export(&home, "ChatExport_synth");
    assert!(
        !telegram.starts_with(operator_memex()),
        "telegram fixture copy must stay under the fake home"
    );
    assert!(!home.join("memex").exists(), "this case is no archives yet");
    let err = write_search_all(&memex_dir(&home), "Catfooding", false, &mut Vec::new())
        .expect_err("default search does not walk live home exports");
    let text = err.to_string();
    assert!(
        text.contains("no archives found"),
        "empty memex must say no archives found, got {text}"
    );
}

#[test]
fn default_search_hits_ingested_archives_not_live_sources() {
    let home = test_dir("default-search-archives-only");
    assert!(
        !home.starts_with(operator_memex()),
        "tests must not ingest into the operator memex directory"
    );
    let grok = scoped_archive_path(&memex_dir(&home), "agents/grok", "a").expect("grok scope");
    let operator_before = operator_leftover_fingerprint();
    ingest(&grok, &[fixture_path()]).expect("ingest grok archive");
    let telegram = copy_telegram_export(&home, "ChatExport_extra");
    assert!(
        !telegram.starts_with(operator_memex()),
        "telegram fixture copy must stay under the fake home"
    );

    let mut printed = Vec::new();
    write_search_all(&memex_dir(&home), "Catfooding", false, &mut printed)
        .expect("default search must read ingested archives");
    let printed = String::from_utf8(printed).expect("search output is utf-8");
    assert!(
        printed.contains("agents/grok/a.majestic\n"),
        "report must put the memex archive path on its own line, got {printed:?}"
    );
    assert!(
        printed.contains("Catfooding"),
        "ingested archive must hit, got {printed:?}"
    );
    assert!(
        !printed.contains("ChatExport_extra"),
        "default search must not walk live Telegram dumps, got {printed:?}"
    );
    assert_operator_memex_untouched(operator_before);
}

#[cfg(unix)]
#[test]
fn default_search_returns_archive_hits_with_unreadable_sandbox_dirs() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;
    let home = test_dir("default-search-unreadable-sandbox");
    assert!(
        !home.starts_with(operator_memex()),
        "tests must not ingest into the operator memex directory"
    );
    let grok = scoped_archive_path(&memex_dir(&home), "agents/grok", "a").expect("grok scope");
    ingest(&grok, &[fixture_path()]).expect("ingest archive with Catfooding");
    let grok_dir = home.join(".grok");
    fs::create_dir_all(&grok_dir).expect(".grok");
    let mut blocked = Vec::new();
    for i in 0..32 {
        let dir = grok_dir.join(format!("sandbox-blocked-dir.{i}"));
        fs::create_dir_all(&dir).expect("sandbox-blocked-dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).expect("mode 000");
        blocked.push(dir);
    }
    let started = Instant::now();
    let mut printed = Vec::new();
    let result = write_search_all(&memex_dir(&home), "catfooding", true, &mut printed);
    for dir in &blocked {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    }
    result.expect("unreadable sandbox dirs must not block archive search");
    assert!(
        started.elapsed().as_secs() < 5,
        "archive search must not walk thousands of unreadable dirs"
    );
    let printed = String::from_utf8(printed).expect("utf-8");
    assert!(
        printed.contains("Catfooding") || printed.contains("catfooding"),
        "must hit the ingested archive, got {printed:?}"
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn search_same_snippet_packed_twice_is_one_unique_hit() {
    let dir = test_dir("search-dup-snippet");
    let export = dir.join("dup.json");
    write_grok_export(
        &export,
        &[(
            "dup-convo",
            ["packed-twice-token", "packed-twice-token"].as_slice(),
        )],
    );
    let out = dir.join("archive.majestic");
    ingest(&out, &[export]).expect("ingest duplicate packed message");
    let archive = Archive::open(&out).expect("mmap open");
    assert!(
        archive.text_slice_is_mmap_view(),
        "search must use the mmap text slice, not a heap copy of the blob"
    );
    let text = archive.text().expect("mmap text");
    let packed = text.matches("packed-twice-token").count();
    assert!(
        packed >= 2,
        "fixture must pack the same message twice, got {packed} in {text:?}"
    );
    let hits = search(&archive, "packed-twice-token", false).expect("search packed twice");
    assert_eq!(
        hits.len(),
        1,
        "same archive, conversation, field, and snippet must print once, got {hits:?}"
    );
    assert_eq!(hits[0].conversation_id.as_deref(), Some("dup-convo"));
    assert_eq!(hits[0].field, "message");
    assert!(hits[0].snippet.contains("packed-twice-token"));
}

#[test]
fn search_two_conversations_are_two_unique_hits() {
    let dir = test_dir("search-two-convos");
    let export = dir.join("two.json");
    write_grok_export(
        &export,
        &[
            ("convo-alpha", ["alpha-unique-lizard"].as_slice()),
            ("convo-beta", ["beta-unique-lizard"].as_slice()),
        ],
    );
    let out = dir.join("archive.majestic");
    ingest(&out, &[export]).expect("ingest two conversations");
    let archive = Archive::open(&out).expect("mmap open");
    let hits = search(&archive, "unique-lizard", false).expect("search both conversations");
    assert_eq!(
        hits.len(),
        2,
        "distinct conversations must stay two hits, got {hits:?}"
    );
    let mut ids: Vec<_> = hits
        .iter()
        .map(|hit| hit.conversation_id.as_deref().unwrap_or("-"))
        .collect();
    ids.sort();
    assert_eq!(ids, ["convo-alpha", "convo-beta"]);
}

#[test]
fn search_output_groups_by_archive_then_conversation() {
    let home = test_dir("search-group-output");
    let grok = scoped_archive_path(&memex_dir(&home), "agents/grok", "a").expect("scope");
    let export = home.join("dup.json");
    write_grok_export(
        &export,
        &[(
            "dup-convo",
            ["grouped-unique-token", "grouped-unique-token"].as_slice(),
        )],
    );
    ingest(&grok, &[export]).expect("ingest");
    let mut printed = Vec::new();
    write_search_all(
        &memex_dir(&home),
        "grouped-unique-token",
        false,
        &mut printed,
    )
    .expect("print grouped hits");
    let printed = String::from_utf8(printed).expect("utf-8");
    assert!(
        printed.contains("agents/grok/a.majestic\n"),
        "report must put the archive path on its own line, got {printed:?}"
    );
    assert!(
        printed.contains("  dup-convo\n"),
        "report must group unique messages under the conversation id, got {printed:?}"
    );
    assert_eq!(
        printed.matches("grouped-unique-token").count(),
        1,
        "duplicate packed snippet must print once, got {printed:?}"
    );
    assert!(
        !printed.contains("  dup-convo  message"),
        "must not dump a flat wall of id field snippet, got {printed:?}"
    );
}

#[test]
fn search_default_print_cap_does_not_print_every_unique_hit() {
    let home = test_dir("search-print-cap");
    let grok = scoped_archive_path(&memex_dir(&home), "agents/grok", "a").expect("scope");
    let export = home.join("many.json");
    let mut conversations = Vec::new();
    for i in 0..8 {
        conversations.push(format!(
            r#"{{"conversation":{{"id":"convo-{i}","title":"Synthetic convo-{i}"}},"responses":[{{"response":{{"_id":"r-{i}","conversation_id":"convo-{i}","message":"print-cap-token-{i}","sender":"assistant"}}}}]}}"#
        ));
    }
    fs::write(
        &export,
        format!(
            r#"{{"conversations":[{}],"media_posts":[],"projects":[],"tasks":[]}}"#,
            conversations.join(",")
        ),
    )
    .expect("write many unique conversations");
    ingest(&grok, &[export]).expect("ingest many unique conversations");
    let mut printed = Vec::new();
    write_search_all_exec(
        &memex_dir(&home),
        "print-cap-token",
        SearchExec {
            max_count: 3,
            ..SearchExec::default()
        },
        &mut printed,
    )
    .expect("print capped unique hits");
    let printed = String::from_utf8(printed).expect("utf-8");
    let snippet_lines = printed
        .lines()
        .filter(|line| line.trim_start().starts_with("print-cap-token-"))
        .count();
    assert_eq!(
        snippet_lines, 3,
        "max_count 3 must print 3 unique snippets, got {printed:?}"
    );
    assert!(
        printed.contains("more unique messages not shown"),
        "must report unique hits not printed, got {printed:?}"
    );
}
