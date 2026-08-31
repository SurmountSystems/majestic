use std::fs;
use std::path::{Path, PathBuf};

use majestic::archive::{Archive, TEXT_PAGE_BYTES};
use majestic::ingest::ingest;
use majestic::{
    SearchExec, SearchFlags, SearchFormat, SearchHit, SearchStatus, SearchStatusSink,
    compile_query, group_snippet_occurrences, scoped_archive_path, search, search_all_archives,
    search_all_archives_with_status, search_query, search_with, write_search, write_search_all,
    write_search_all_exec,
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

/// One conversation with an explicit response id so overlapping dumps can
/// pack the same message body twice (different `_id`, same bytes).
fn write_grok_export_named_response(
    path: &Path,
    conversation_id: &str,
    response_id: &str,
    message: &str,
) {
    let body = format!(
        r#"{{"conversations":[{{"conversation":{{"id":"{conversation_id}","title":"Synthetic {conversation_id}"}},"responses":[{{"response":{{"_id":"{response_id}","conversation_id":"{conversation_id}","message":"{message}","sender":"assistant"}}}}]}}],"media_posts":[],"projects":[],"tasks":[]}}"#
    );
    fs::write(path, body).expect("write overlapping grok export");
}

/// One packed message with many copies of a short token so PCRE2 fires at
/// many offsets.
fn repeated_token_message(token: &str, copies: usize) -> String {
    vec![token; copies].join(" ")
}

fn conversation_ids(hits: &[SearchHit]) -> Vec<&str> {
    let mut ids: Vec<_> = hits
        .iter()
        .map(|hit| hit.conversation_id.as_deref().unwrap_or("-"))
        .collect();
    ids.sort();
    ids
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

/// Packed spans sit next to each other in the UTF-8 blob with no separator.
/// AND is both terms in one title or one message, not later in the archive.
#[test]
fn search_and_requires_both_words_in_the_same_packed_span() {
    let dir = test_dir("search-and-same-span");
    let export = dir.join("and-span.json");
    write_grok_export(
        &export,
        &[
            ("convo-a", ["the cat sat"].as_slice()),
            ("convo-b", ["the lizard sat"].as_slice()),
            ("convo-gov", ["government keeps secret"].as_slice()),
            ("convo-phrase", ["hello world"].as_slice()),
            ("convo-split", [r"hello\nworld"].as_slice()),
        ],
    );
    let out = dir.join("archive.majestic");
    ingest(&out, &[export]).expect("ingest AND span fixture");
    let archive = Archive::open(&out).expect("mmap open");
    assert!(
        archive.text_slice_is_mmap_view(),
        "search must use the mmap text slice, not a heap copy of the blob"
    );
    let text = archive.text().expect("mmap text");
    assert!(
        text.contains("the cat sat") && text.contains("the lizard sat"),
        "fixture must pack both messages in one blob, got {text:?}"
    );

    let lookahead =
        search(&archive, "(?=.*lizard)(?=.*the)", false).expect("lookahead AND per span");
    assert_eq!(
        conversation_ids(&lookahead),
        ["convo-b"],
        "lookahead AND must hit only the span that contains both lizard and the, got {lookahead:?}"
    );
    assert_eq!(lookahead[0].field, "message");
    assert!(
        lookahead[0].snippet.contains("lizard") && lookahead[0].snippet.contains("the"),
        "AND snippet must come from the matching span and show both terms, got {:?}",
        lookahead[0].snippet
    );
    assert!(
        !lookahead[0].snippet.contains("cat"),
        "AND snippet must not be a window into the cat-only message, got {:?}",
        lookahead[0].snippet
    );

    let human = search_query(&archive, "lizard AND the", SearchFlags::default())
        .expect("human AND per span");
    assert_eq!(
        conversation_ids(&human),
        ["convo-b"],
        "human AND must hit only the span that contains both words, got {human:?}"
    );
    assert_eq!(human[0].span_text, lookahead[0].span_text);

    let or_hits = search_query(&archive, "lizard OR government", SearchFlags::default())
        .expect("human OR across messages");
    assert_eq!(
        conversation_ids(&or_hits),
        ["convo-b", "convo-gov"],
        "OR may hit different messages, got {or_hits:?}"
    );

    let phrase = search_query(&archive, r#""hello world""#, SearchFlags::default())
        .expect("quoted phrase still contiguous in one span");
    assert_eq!(
        conversation_ids(&phrase),
        ["convo-phrase"],
        "quoted phrase must stay contiguous inside one span, got {phrase:?}"
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
    // PCRE2 once, then searches each packed span of already-mapped text.
    // There is no four-map cap.
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
    // Same packed body in two archives: one snippet, two occurrence rows.
    assert_eq!(
        printed.matches("Catfooding").count(),
        1,
        "identical packed bodies across archives must print the snippet once, got {printed:?}"
    );
    assert!(
        printed.contains("archive agents/grok/a.majestic  conversation"),
        "grok archive must be an occurrence row, got {printed:?}"
    );
    assert!(
        printed.contains("archive social/x/b.majestic  conversation"),
        "social archive must be an occurrence row, got {printed:?}"
    );
    assert!(printed.contains("present in:"), "got {printed:?}");
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
        printed.contains("archive agents/grok/a.majestic  conversation"),
        "report must list the memex archive on an occurrence row, got {printed:?}"
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
fn search_repeated_token_in_one_message_is_one_unique_hit() {
    let dir = test_dir("search-slide-one-message");
    let export = dir.join("slide.json");
    let body = repeated_token_message("aaa", 80);
    write_grok_export(&export, &[("slide-convo", [body.as_str()].as_slice())]);
    let out = dir.join("archive.majestic");
    ingest(&out, &[export]).expect("ingest repeating token message");
    let archive = Archive::open(&out).expect("mmap open");
    assert!(
        archive.text_slice_is_mmap_view(),
        "search must use the mmap text slice, not a heap copy of the blob"
    );
    let text = archive.text().expect("mmap text");
    let packed = text.matches("aaa").count();
    assert!(
        packed >= 40,
        "fixture must pack many token offsets in one message, got {packed} in {text:?}"
    );
    let hits = search(&archive, "aaa", false).expect("search repeating token");
    assert_eq!(
        hits.len(),
        1,
        "many PCRE2 matches in one packed message must be one unique hit, got {hits:?}"
    );
    assert_eq!(hits[0].conversation_id.as_deref(), Some("slide-convo"));
    assert_eq!(hits[0].field, "message");
    assert!(
        hits[0].snippet.contains("aaa"),
        "snippet must come from the first match in that message, got {:?}",
        hits[0].snippet
    );
}

#[test]
fn search_two_conversations_with_repeated_tokens_are_two_unique_hits() {
    let dir = test_dir("search-slide-two-convos");
    let export = dir.join("two-slide.json");
    let alpha = repeated_token_message("aaa", 80);
    let beta = repeated_token_message("aaa", 80);
    write_grok_export(
        &export,
        &[
            ("convo-alpha", [alpha.as_str()].as_slice()),
            ("convo-beta", [beta.as_str()].as_slice()),
        ],
    );
    let out = dir.join("archive.majestic");
    ingest(&out, &[export]).expect("ingest two repeating conversations");
    let archive = Archive::open(&out).expect("mmap open");
    let hits = search(&archive, "aaa", false).expect("search both repeating conversations");
    assert_eq!(
        hits.len(),
        2,
        "uniqueness still keeps both conversation ids; grouping must not drop an occurrence, got {hits:?}"
    );
    let groups = group_snippet_occurrences(hits.iter().map(|hit| (None, hit)));
    assert_eq!(
        groups.len(),
        1,
        "the same packed body in two conversation ids is one snippet, got {groups:?}"
    );
    assert_eq!(
        groups[0].occurrences.len(),
        2,
        "two conversation ids must be two occurrence rows, got {groups:?}"
    );
    let mut ids: Vec<_> = groups[0]
        .occurrences
        .iter()
        .map(|occurrence| occurrence.conversation_id.as_deref().unwrap_or("-"))
        .collect();
    ids.sort();
    assert_eq!(ids, ["convo-alpha", "convo-beta"]);
    assert!(
        hits.iter().all(|hit| hit.field == "message"),
        "unique hits must be the packed messages, not titles, got {hits:?}"
    );
}

#[test]
fn search_aaa_does_not_hit_titles_that_do_not_contain_aaa() {
    let dir = test_dir("search-aaa-no-title");
    let export = dir.join("two-slide.json");
    let alpha = repeated_token_message("aaa", 80);
    let beta = repeated_token_message("aaa", 80);
    let ids = ["convo-alpha", "convo-beta"];
    write_grok_export(
        &export,
        &[
            (ids[0], [alpha.as_str()].as_slice()),
            (ids[1], [beta.as_str()].as_slice()),
        ],
    );
    for id in ids {
        let title = format!("Synthetic {id}");
        assert!(
            !title.contains("aaa"),
            "fixture title must not contain aaa, got {title:?}"
        );
    }
    let out = dir.join("archive.majestic");
    ingest(&out, &[export]).expect("ingest two repeating conversations");
    let archive = Archive::open(&out).expect("mmap open");
    let hits = search(&archive, "aaa", false).expect("search aaa");
    assert!(
        hits.iter().all(|hit| hit.field != "title"),
        "pattern aaa must not hit a title that does not contain aaa, got {hits:?}"
    );
    assert_eq!(
        hits.len(),
        2,
        "uniqueness still keeps both conversation ids; grouping must not drop an occurrence, got {hits:?}"
    );
    assert!(hits.iter().all(|hit| hit.field == "message"));
    let groups = group_snippet_occurrences(hits.iter().map(|hit| (None, hit)));
    assert_eq!(
        groups.len(),
        1,
        "the same packed body in two conversation ids is one snippet, got {groups:?}"
    );
    assert_eq!(groups[0].occurrences.len(), 2);
}

#[test]
fn search_two_messages_in_one_conversation_are_two_unique_hits() {
    let dir = test_dir("search-two-messages");
    let export = dir.join("two-msg.json");
    write_grok_export(
        &export,
        &[(
            "dup-convo",
            ["alpha-unique-msg", "beta-unique-msg"].as_slice(),
        )],
    );
    let out = dir.join("archive.majestic");
    ingest(&out, &[export]).expect("ingest two messages");
    let archive = Archive::open(&out).expect("mmap open");
    let hits = search(&archive, "unique-msg", false).expect("search two messages");
    assert_eq!(
        hits.len(),
        2,
        "distinct packed messages in one conversation must stay two hits, got {hits:?}"
    );
    assert!(hits.iter().all(|hit| hit.field == "message"));
    assert!(
        hits.iter()
            .all(|hit| hit.conversation_id.as_deref() == Some("dup-convo"))
    );
}

#[test]
fn search_overlapping_dumps_same_message_bytes_are_one_unique_hit() {
    let dir = test_dir("search-overlap-same-bytes");
    let first = dir.join("dump-a.json");
    let second = dir.join("dump-b.json");
    let body = "packed-copy-token";
    write_grok_export_named_response(&first, "dump-convo", "r-dump-a", body);
    write_grok_export_named_response(&second, "dump-convo", "r-dump-b", body);
    let title = "Synthetic dump-convo";
    assert!(
        !title.contains(body),
        "fixture title must not contain {body}, got {title:?}"
    );
    let out = dir.join("archive.majestic");
    ingest(&out, &[first, second]).expect("ingest overlapping dumps");
    let archive = Archive::open(&out).expect("mmap open");
    let text = archive.text().expect("mmap text");
    let packed = text.matches(body).count();
    assert_eq!(
        packed, 2,
        "fixture must pack two copies of the same message body, got {packed} in {text:?}"
    );
    let hits = search(&archive, body, false).expect("search overlapping packed copies");
    assert_eq!(
        hits.len(),
        1,
        "same conversation, same field, identical packed span text must be one hit, got {hits:?}"
    );
    assert_eq!(hits[0].conversation_id.as_deref(), Some("dump-convo"));
    assert_eq!(hits[0].field, "message");
    assert!(
        hits[0].snippet.contains(body),
        "snippet must come from the first kept packed copy, got {:?}",
        hits[0].snippet
    );
    let groups = group_snippet_occurrences(hits.iter().map(|hit| (None, hit)));
    assert_eq!(
        groups.len(),
        1,
        "overlapping dumps of the same id and bytes are one snippet, got {groups:?}"
    );
    assert_eq!(
        groups[0].occurrences.len(),
        1,
        "overlapping dumps of the same id and bytes are one occurrence, got {groups:?}"
    );
    let mut printed = Vec::new();
    write_search(&out, body, false, &mut printed).expect("print overlapping dumps");
    let printed = String::from_utf8(printed).expect("utf-8");
    assert_eq!(
        printed.matches(body).count(),
        1,
        "overlapping dumps must print the packed body once, got {printed:?}"
    );
    assert_eq!(
        printed.matches("present in:").count(),
        1,
        "overlapping dumps must be one snippet group, got {printed:?}"
    );
    assert_eq!(
        printed
            .matches("conversation dump-convo  field message")
            .count(),
        1,
        "overlapping dumps must be one occurrence row, got {printed:?}"
    );
}

#[test]
fn search_dot_on_one_conversation_is_title_and_message_not_windows() {
    let dir = test_dir("search-slide-dot-fields");
    let export = dir.join("dot.json");
    let body = repeated_token_message("aaa", 80);
    write_grok_export(&export, &[("slide-dot", [body.as_str()].as_slice())]);
    let out = dir.join("archive.majestic");
    ingest(&out, &[export]).expect("ingest title plus long message");
    let archive = Archive::open(&out).expect("mmap open");
    let hits = search(&archive, ".", false).expect("search every character");
    assert_eq!(
        hits.len(),
        2,
        "title and message must stay two unique hits, not one window per offset, got {} hits",
        hits.len()
    );
    let mut fields: Vec<_> = hits.iter().map(|hit| hit.field).collect();
    fields.sort();
    assert_eq!(fields, ["message", "title"]);
    let groups = group_snippet_occurrences(hits.iter().map(|hit| (None, hit)));
    assert_eq!(
        groups.len(),
        2,
        "title body and message body differ, so they stay two snippet blocks, got {groups:?}"
    );
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
fn search_identical_sentence_two_conversation_ids_is_one_snippet_two_occurrences() {
    let dir = test_dir("search-two-convos-same-body");
    let export = dir.join("two.json");
    write_grok_export(
        &export,
        &[
            ("convo-alpha", ["same-sentence-token"].as_slice()),
            ("convo-beta", ["same-sentence-token"].as_slice()),
        ],
    );
    let out = dir.join("archive.majestic");
    ingest(&out, &[export]).expect("ingest two conversations with the same sentence");
    let archive = Archive::open(&out).expect("mmap open");
    let hits = search(&archive, "same-sentence-token", false)
        .expect("search identical bodies in two conversations");
    assert_eq!(
        hits.len(),
        2,
        "uniqueness still keeps both conversation ids; grouping must not drop an occurrence, got {hits:?}"
    );
    let groups = group_snippet_occurrences(hits.iter().map(|hit| (None, hit)));
    assert_eq!(
        groups.len(),
        1,
        "presentation is one snippet for the same packed body, not two copies of the paragraph, got {groups:?}"
    );
    assert_eq!(
        groups[0].occurrences.len(),
        2,
        "two conversation ids must be two occurrence rows, got {groups:?}"
    );
    let mut ids: Vec<_> = groups[0]
        .occurrences
        .iter()
        .map(|occurrence| occurrence.conversation_id.as_deref().unwrap_or("-"))
        .collect();
    ids.sort();
    assert_eq!(ids, ["convo-alpha", "convo-beta"]);
    assert!(hits.iter().all(|hit| hit.field == "message"));
    let mut printed = Vec::new();
    write_search(&out, "same-sentence-token", false, &mut printed)
        .expect("print one snippet and two occurrences");
    let printed = String::from_utf8(printed).expect("utf-8");
    assert_eq!(
        printed.matches("same-sentence-token").count(),
        1,
        "CLI must print the sentence once, got {printed:?}"
    );
    assert!(printed.contains("present in:"), "got {printed:?}");
    assert!(
        printed.contains("conversation convo-alpha  field message"),
        "got {printed:?}"
    );
    assert!(
        printed.contains("conversation convo-beta  field message"),
        "got {printed:?}"
    );
}

#[test]
fn search_two_different_bodies_are_two_snippet_blocks() {
    let dir = test_dir("search-two-bodies-two-blocks");
    let export = dir.join("two.json");
    write_grok_export(
        &export,
        &[
            ("convo-alpha", ["alpha-unique-lizard"].as_slice()),
            ("convo-beta", ["beta-unique-lizard"].as_slice()),
        ],
    );
    let out = dir.join("archive.majestic");
    ingest(&out, &[export]).expect("ingest two different bodies");
    let archive = Archive::open(&out).expect("mmap open");
    let hits = search(&archive, "unique-lizard", false).expect("search different bodies");
    assert_eq!(
        hits.len(),
        2,
        "two different messages stay two unique hits, got {hits:?}"
    );
    let groups = group_snippet_occurrences(hits.iter().map(|hit| (None, hit)));
    assert_eq!(
        groups.len(),
        2,
        "two different packed bodies stay two snippet blocks, got {groups:?}"
    );
    assert!(
        groups.iter().all(|group| group.occurrences.len() == 1),
        "each distinct body is one occurrence, got {groups:?}"
    );
    let mut printed = Vec::new();
    write_search(&out, "unique-lizard", false, &mut printed).expect("print two snippet blocks");
    let printed = String::from_utf8(printed).expect("utf-8");
    assert_eq!(
        printed.matches("present in:").count(),
        2,
        "two different bodies must print two snippet blocks, got {printed:?}"
    );
    assert!(printed.contains("alpha-unique-lizard"), "got {printed:?}");
    assert!(printed.contains("beta-unique-lizard"), "got {printed:?}");
}

#[test]
fn search_output_groups_by_archive_then_conversation() {
    let home = test_dir("search-group-output");
    let grok = scoped_archive_path(&memex_dir(&home), "agents/grok", "a").expect("scope");
    let export = home.join("dup.json");
    write_grok_export(
        &export,
        &[("dup-convo", ["grouped-unique-token"].as_slice())],
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
        printed.contains("archive agents/grok/a.majestic  conversation dup-convo  field message"),
        "report must list archive, conversation id, and field on an occurrence row, got {printed:?}"
    );
    assert_eq!(
        printed.matches("grouped-unique-token").count(),
        1,
        "one packed message must print once, got {printed:?}"
    );
    assert!(
        printed.contains("present in:"),
        "snippet groups must list occurrences, got {printed:?}"
    );
    assert!(
        !printed.contains("  dup-convo  message"),
        "must not dump a flat wall of id field snippet, got {printed:?}"
    );
}

#[test]
fn search_repeated_token_print_cap_counts_unique_messages() {
    let home = test_dir("search-slide-print-cap");
    let grok = scoped_archive_path(&memex_dir(&home), "agents/grok", "a").expect("scope");
    let export = home.join("slide.json");
    let body = repeated_token_message("aaa", 80);
    write_grok_export(&export, &[("slide-convo", [body.as_str()].as_slice())]);
    ingest(&grok, &[export]).expect("ingest repeating token message");
    let mut printed = Vec::new();
    write_search_all_exec(
        &memex_dir(&home),
        "aaa",
        SearchExec {
            max_count: 3,
            ..SearchExec::default()
        },
        &mut printed,
    )
    .expect("print unique messages");
    let printed = String::from_utf8(printed).expect("utf-8");
    assert_eq!(
        printed.matches("present in:").count(),
        1,
        "print cap applies to snippet groups, not sliding windows, got {printed:?}"
    );
    assert!(
        printed.contains("duplicate hits omitted"),
        "extra PCRE2 offsets in one message must count as duplicates, got {printed:?}"
    );
    assert!(
        !printed.contains("more snippet groups not shown"),
        "one unique message must not look like a unique-hit flood, got {printed:?}"
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
        printed.contains("more snippet groups not shown"),
        "must report snippet groups not printed, got {printed:?}"
    );
}

#[test]
fn human_and_compiles_to_lookahead_and_hits_the_same_as_raw() {
    let dir = test_dir("search-human-and");
    let out = dir.join("archive.majestic");
    ingest(&out, &[fixture_path()]).expect("ingest fixture");
    let archive = Archive::open(&out).expect("mmap open");
    let compiled =
        compile_query("Catfooding AND fixture", SearchFlags::default()).expect("compile human AND");
    assert!(
        compiled.pattern.contains("(?=.*Catfooding)") && compiled.pattern.contains("(?=.*fixture)"),
        "human AND must compile to lookaheads, got {:?}",
        compiled.pattern
    );
    let human = search_query(&archive, "Catfooding AND fixture", SearchFlags::default())
        .expect("human AND search");
    let raw = search(&archive, "(?=.*Catfooding)(?=.*fixture)", false).expect("raw lookahead");
    assert!(
        !human.is_empty(),
        "Catfooding AND fixture must hit the fixture"
    );
    assert_eq!(
        human.len(),
        raw.len(),
        "human AND must hit the same messages as the lookahead form"
    );
}

#[test]
fn human_or_matches_either_term() {
    let dir = test_dir("search-human-or");
    let export = dir.join("or.json");
    write_grok_export(
        &export,
        &[
            ("alpha-convo", ["alpha-only-token"].as_slice()),
            ("beta-convo", ["beta-only-token"].as_slice()),
        ],
    );
    let out = dir.join("archive.majestic");
    ingest(&out, &[export]).expect("ingest OR fixture");
    let archive = Archive::open(&out).expect("mmap open");
    let hits = search_query(
        &archive,
        "alpha-only-token OR beta-only-token",
        SearchFlags::default(),
    )
    .expect("human OR");
    let mut ids: Vec<_> = hits
        .iter()
        .map(|hit| hit.conversation_id.as_deref().unwrap_or("-"))
        .collect();
    ids.sort();
    assert_eq!(ids, ["alpha-convo", "beta-convo"]);
}

#[test]
fn quoted_phrase_does_not_match_hello_newline_world() {
    let dir = test_dir("search-human-phrase");
    let export = dir.join("phrase.json");
    write_grok_export(
        &export,
        &[
            ("phrase-convo", ["hello world"].as_slice()),
            ("split-convo", [r"hello\nworld"].as_slice()),
        ],
    );
    let out = dir.join("archive.majestic");
    ingest(&out, &[export]).expect("ingest phrase fixture");
    let archive = Archive::open(&out).expect("mmap open");
    let compiled = compile_query(r#""hello world""#, SearchFlags::default()).expect("phrase");
    assert_eq!(compiled.pattern, "hello world");
    let hits = search_query(&archive, r#""hello world""#, SearchFlags::default())
        .expect("quoted phrase search");
    assert!(
        hits.iter()
            .any(|hit| hit.conversation_id.as_deref() == Some("phrase-convo")),
        "contiguous hello world must hit, got {hits:?}"
    );
    assert!(
        hits.iter()
            .all(|hit| hit.conversation_id.as_deref() != Some("split-convo")),
        "hello then newline then world must not match a quoted phrase, got {hits:?}"
    );
}

#[test]
fn slash_regex_i_is_case_insensitive() {
    let dir = test_dir("search-slash-regex-i");
    let out = dir.join("archive.majestic");
    ingest(&out, &[fixture_path()]).expect("ingest fixture");
    let archive = Archive::open(&out).expect("mmap open");
    let miss = search(&archive, "catfooding", false).expect("case-sensitive miss");
    assert!(miss.is_empty(), "raw catfooding must miss Catfooding");
    let hits =
        search_query(&archive, "/Catfooding/i", SearchFlags::default()).expect("/Catfooding/i");
    assert!(
        !hits.is_empty(),
        "/Catfooding/i must hit Catfooding case-insensitively"
    );
}

#[test]
fn search_progress_records_n_of_m_for_two_archives() {
    let home = test_dir("search-progress-two");
    let memex = memex_dir(&home);
    let grok = scoped_archive_path(&memex, "agents/grok", "a").expect("grok");
    let social = scoped_archive_path(&memex, "social/x", "b").expect("social");
    ingest(&grok, &[fixture_path()]).expect("ingest grok");
    ingest(&social, &[fixture_path()]).expect("ingest social");
    let sink = SearchStatusSink::new();
    let groups =
        search_all_archives_with_status(&memex, "Catfooding", SearchFlags::default(), &sink)
            .expect("search two archives");
    assert_eq!(groups.len(), 2);
    let events = sink.events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SearchStatus::Starting { archives: 2 })),
        "must log searching 2 archives, got {events:?}"
    );
    let archive_events: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SearchStatus::Archive {
                completed,
                total,
                archive,
                unique_hits,
            } => Some((*completed, *total, archive.clone(), *unique_hits)),
            _ => None,
        })
        .collect();
    assert_eq!(
        archive_events.len(),
        2,
        "must record one status line per archive, got {events:?}"
    );
    assert!(
        archive_events.iter().all(|(_, total, _, _)| *total == 2),
        "total must be 2, got {archive_events:?}"
    );
    let mut completed: Vec<_> = archive_events
        .iter()
        .map(|(completed, _, _, _)| *completed)
        .collect();
    completed.sort();
    assert_eq!(completed, [1, 2]);
    assert!(
        archive_events.iter().any(|(_, _, archive, _)| {
            archive.ends_with("a.majestic") || archive.to_string_lossy().contains("agents/grok")
        }),
        "status must name an archive path, got {archive_events:?}"
    );
    assert!(
        archive_events.iter().any(|(_, _, _, unique)| *unique > 0),
        "running unique-hit count must appear, got {archive_events:?}"
    );
}

#[test]
fn search_format_json_stdout_is_json() {
    let home = test_dir("search-format-json");
    let grok = scoped_archive_path(&memex_dir(&home), "agents/grok", "a").expect("scope");
    ingest(&grok, &[fixture_path()]).expect("ingest");
    let mut printed = Vec::new();
    write_search_all_exec(
        &memex_dir(&home),
        "Catfooding",
        SearchExec {
            format: SearchFormat::Json,
            max_count: 0,
            ..SearchExec::default()
        },
        &mut printed,
    )
    .expect("write json");
    let text = String::from_utf8(printed).expect("utf-8");
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("stdout is JSON");
    assert!(
        value.get("hits").and_then(|hits| hits.as_array()).is_some(),
        "JSON report must have a hits array, got {text}"
    );
    assert!(
        !text.contains("present in:"),
        "JSON stdout must not mix human CLI chrome, got {text}"
    );
}

#[test]
fn search_format_toon_stdout_is_not_json_object() {
    let home = test_dir("search-format-toon");
    let grok = scoped_archive_path(&memex_dir(&home), "agents/grok", "a").expect("scope");
    ingest(&grok, &[fixture_path()]).expect("ingest");
    let mut printed = Vec::new();
    write_search_all_exec(
        &memex_dir(&home),
        "Catfooding",
        SearchExec {
            format: SearchFormat::Toon,
            max_count: 0,
            ..SearchExec::default()
        },
        &mut printed,
    )
    .expect("write toon");
    let text = String::from_utf8(printed).expect("utf-8");
    assert!(
        !text.trim_start().starts_with('{'),
        "TOON stdout must not look like JSON, got {text:?}"
    );
    assert!(
        text.contains("Catfooding") || text.contains("hits"),
        "TOON must carry the hit, got {text:?}"
    );
}
