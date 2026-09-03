//! Story-card JSON arrays and character TOML. Explicit path plus --service
//! and --account. Home scan must not treat a random JSON array as cards.

use std::fs;
use std::path::{Path, PathBuf};

use majestic::archive::Archive;
use majestic::ingest::{infer_ingest_scope, ingest_from_flags, ingest_home};
use majestic::schema::JsonAtom;
use majestic::{list_memex_archives, scoped_archive_path, search, search_all_archives};

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

fn assert_not_operator_memex(path: &Path) {
    assert!(
        !path.starts_with(operator_memex()),
        "tests must not ingest into the operator memex directory"
    );
}

fn memex_dir(home: &Path) -> PathBuf {
    home.join("memex")
}

#[test]
fn story_card_json_requires_service_and_account() {
    let cards = fixtures().join("tiny-story-cards.json");
    let err = infer_ingest_scope(&cards).expect_err("story cards do not infer a service");
    let text = err.to_string();
    assert!(
        text.contains("--service") || text.contains("not an official Grok export"),
        "missing flags must ask for --service and --account, got {text}"
    );

    let root = test_dir("story-cards-need-flags");
    let home = root.join("fake-home");
    let err = ingest_from_flags(&home, None, None, None, std::slice::from_ref(&cards))
        .expect_err("ingest without flags must fail");
    let text = err.to_string();
    assert!(
        text.contains("--service") || text.contains("not an official Grok export"),
        "explicit story-card path still needs --service and --account, got {text}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ingest_story_card_json_hits_title_keys_and_value() {
    let root = test_dir("story-cards-search");
    let home = root.join("fake-home");
    let cards = fixtures().join("tiny-story-cards.json");
    let reports = ingest_from_flags(
        &home,
        None,
        Some("eridu/synth-campaign"),
        Some("play-a"),
        std::slice::from_ref(&cards),
    )
    .expect("ingest story cards with flags");
    assert_eq!(reports.len(), 1);
    let archive = &reports[0].output;
    assert_not_operator_memex(archive);
    assert_eq!(
        archive,
        &scoped_archive_path(&memex_dir(&home), "eridu/synth-campaign", "play-a").expect("scope")
    );

    let opened = Archive::open(archive).expect("open story-card archive");
    let root_rec = opened.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root_rec.conversations.len(), 2);
    let lizard = root_rec
        .conversations
        .iter()
        .find(|record| record.item.conversation.title.as_deref() == Some("Synth Lizard Card"))
        .expect("lizard card");
    assert_eq!(
        lizard.item.conversation.extra.get("type"),
        Some(&JsonAtom::String("character".to_owned()))
    );
    assert_eq!(
        lizard
            .item
            .conversation
            .extra
            .get("useForCharacterCreation"),
        Some(&JsonAtom::Bool(true))
    );
    assert_eq!(
        lizard.item.conversation.extra.get("isSpoiler"),
        Some(&JsonAtom::Bool(false))
    );
    assert_eq!(
        lizard.item.conversation.extra.get("showInStoryCards"),
        Some(&JsonAtom::Bool(true))
    );
    assert_eq!(
        lizard.item.conversation.extra.get("description"),
        Some(&JsonAtom::String(
            "optional leftover description".to_owned()
        )),
        "optional story-card description must stay leftover extra"
    );

    let title_hits = search(&opened, "Synth Lizard Card", false).expect("title");
    assert!(!title_hits.is_empty(), "search must hit the card title");
    let key_hits = search(&opened, "scard-key-aa11", false).expect("comma keys");
    assert!(
        !key_hits.is_empty(),
        "search must hit keys from a comma string"
    );
    let array_key = search(&opened, "scard-key-array-cc33", false).expect("array keys");
    assert!(
        !array_key.is_empty(),
        "search must hit keys from a JSON array"
    );
    let value_hits = search(&opened, "scard-value-bb22", false).expect("value");
    assert!(
        !value_hits.is_empty(),
        "search must hit the card value body"
    );
    let swamp = search(&opened, "scard-value-dd44", false).expect("second value");
    assert!(!swamp.is_empty(), "second card value must be searchable");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn two_account_story_card_archives_do_not_share_hits() {
    let root = test_dir("story-cards-two-accounts");
    let home = root.join("fake-home");
    let cards_a = root.join("play-a.json");
    let cards_b = root.join("play-b.json");
    fs::write(
        &cards_a,
        r#"[{"title":"Play A Card","type":"character","keys":"play-a-key-11","value":"play-a-value-unique-xx11"}]"#,
    )
    .expect("write play-a cards");
    fs::write(
        &cards_b,
        r#"[{"title":"Play B Card","type":"character","keys":"play-b-key-22","value":"play-b-value-unique-yy22"}]"#,
    )
    .expect("write play-b cards");

    ingest_from_flags(
        &home,
        None,
        Some("eridu/synth-campaign"),
        Some("play-a"),
        std::slice::from_ref(&cards_a),
    )
    .expect("ingest play-a");
    ingest_from_flags(
        &home,
        None,
        Some("eridu/synth-campaign"),
        Some("play-b"),
        std::slice::from_ref(&cards_b),
    )
    .expect("ingest play-b");

    let archive_a =
        scoped_archive_path(&memex_dir(&home), "eridu/synth-campaign", "play-a").expect("a");
    let archive_b =
        scoped_archive_path(&memex_dir(&home), "eridu/synth-campaign", "play-b").expect("b");
    assert_not_operator_memex(&archive_a);
    assert_not_operator_memex(&archive_b);

    let opened_a = Archive::open(&archive_a).expect("open a");
    let opened_b = Archive::open(&archive_b).expect("open b");
    assert!(
        !search(&opened_a, "play-a-value-unique-xx11", false)
            .expect("a own")
            .is_empty(),
        "play-a archive must hit its own value"
    );
    assert!(
        search(&opened_a, "play-b-value-unique-yy22", false)
            .expect("a must not see b")
            .is_empty(),
        "search in archive A must not see archive B"
    );
    assert!(
        search(&opened_b, "play-a-value-unique-xx11", false)
            .expect("b must not see a")
            .is_empty(),
        "search in archive B must not see archive A"
    );
    assert!(
        !search(&opened_b, "play-b-value-unique-yy22", false)
            .expect("b own")
            .is_empty(),
        "play-b archive must hit its own value"
    );

    let all_a = search_all_archives(&memex_dir(&home), "play-a-value-unique-xx11", false)
        .expect("systemwide a");
    let a_paths: Vec<_> = all_a
        .iter()
        .filter(|(_, hits)| !hits.is_empty())
        .map(|(path, _)| path.clone())
        .collect();
    assert_eq!(a_paths, vec![archive_a.clone()]);
    let all_b = search_all_archives(&memex_dir(&home), "play-b-value-unique-yy22", false)
        .expect("systemwide b");
    let b_paths: Vec<_> = all_b
        .iter()
        .filter(|(_, hits)| !hits.is_empty())
        .map(|(path, _)| path.clone())
        .collect();
    assert_eq!(b_paths, vec![archive_b]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn home_scan_does_not_ingest_random_json_array_as_story_cards() {
    let home = test_dir("home-scan-not-story-cards");
    assert_not_operator_memex(&home);
    let downloads = home.join("Downloads");
    fs::create_dir_all(&downloads).expect("Downloads");
    fs::copy(
        fixtures().join("tiny-story-cards.json"),
        downloads.join("random-cards.json"),
    )
    .expect("copy story-card shaped JSON under fake HOME");

    let reports = ingest_home(&home).expect("home scan with only a JSON array");
    assert!(
        reports.is_empty(),
        "home scan must not ingest a random JSON array as story cards, got {reports:?}"
    );
    let listed = list_memex_archives(&home.join("memex")).expect("list");
    assert!(
        listed.is_empty(),
        "home scan must not write a story-card archive, got {listed:?}"
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ingest_character_toml_with_service_and_account() {
    let root = test_dir("character-toml");
    let home = root.join("fake-home");
    let campaign = root.join("campaign");
    let characters = campaign.join("characters");
    fs::create_dir_all(&characters).expect("characters dir");
    fs::copy(
        fixtures().join("tiny-character.toml"),
        characters.join("synth-hero.toml"),
    )
    .expect("copy character toml");
    fs::write(
        characters.join("other.json"),
        r#"{"id":"must-not-ingest-json-character","value":"json-character-must-not-ingest-zz99"}"#,
    )
    .expect("json character files are not ingested");

    let err = ingest_from_flags(&home, None, None, None, std::slice::from_ref(&characters))
        .expect_err("character TOML still needs flags");
    let text = err.to_string();
    assert!(
        text.contains("--service") || text.contains("not an official Grok export"),
        "character TOML must not infer a service, got {text}"
    );

    let reports = ingest_from_flags(
        &home,
        None,
        Some("eridu/synth-campaign"),
        Some("play-toml"),
        std::slice::from_ref(&characters),
    )
    .expect("ingest characters dir");
    let archive = &reports[0].output;
    assert_not_operator_memex(archive);
    let opened = Archive::open(archive).expect("open character archive");
    let title_hits = search(&opened, "Synth Hero", false).expect("title");
    assert!(!title_hits.is_empty(), "character title must be searchable");
    let key_hits = search(&opened, "char-key-gg77", false).expect("keys");
    assert!(!key_hits.is_empty(), "character keys must be searchable");
    let value_hits = search(&opened, "char-value-hh88", false).expect("value");
    assert!(
        !value_hits.is_empty(),
        "character value body must be searchable"
    );
    let json_hits =
        search(&opened, "json-character-must-not-ingest-zz99", false).expect("json character skip");
    assert!(
        json_hits.is_empty(),
        "JSON files under characters/ must not ingest as character TOML"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn home_scan_does_not_ingest_character_toml() {
    let home = test_dir("home-scan-not-character-toml");
    assert_not_operator_memex(&home);
    let characters = home.join("Downloads").join("characters");
    fs::create_dir_all(&characters).expect("characters dir");
    fs::copy(
        fixtures().join("tiny-character.toml"),
        characters.join("synth-hero.toml"),
    )
    .expect("copy character TOML under fake HOME");

    let reports = ingest_home(&home).expect("home scan with only character TOML");
    assert!(
        reports.is_empty(),
        "home scan must not ingest characters/ TOML, got {reports:?}"
    );
    let listed = list_memex_archives(&home.join("memex")).expect("list");
    assert!(
        listed.is_empty(),
        "home scan must not write a character TOML archive, got {listed:?}"
    );
    let _ = fs::remove_dir_all(&home);
}
