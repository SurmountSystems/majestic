//! Synthetic X (Twitter) account archive ingest.
//!
//! Never copy a live Downloads zip or `data/*.js`. Never print a live username.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use majestic::archive::Archive;
use majestic::ingest::{infer_ingest_scope, ingest};
use majestic::schema::JsonAtom;
use majestic::{resolve_ingest_archive, search, write_search_all};

const SYNTH_TWEET: &str = "Catfooding x fixture";
const SYNTH_LIKE: &str = "Catfooding liked fixture";
const SYNTH_DM: &str = "Catfooding dm fixture";
const SYNTH_EMAIL: &str = "hidden@example.invalid";

const ACCOUNT_JS: &str = r#"window.YTD.account.part0 = [
  {
    "account" : {
      "email" : "hidden@example.invalid",
      "createdVia" : "web",
      "username" : "synthuser",
      "accountId" : "111",
      "createdAt" : "2020-01-01T00:00:00.000Z",
      "accountDisplayName" : "Synth User"
    }
  }
]
"#;

const TWEETS_JS: &str = r#"window.YTD.tweets.part0 = [
  {
    "tweet" : {
      "id" : "123",
      "id_str" : "123",
      "full_text" : "Catfooding x fixture",
      "created_at" : "Wed Jan 01 00:00:00 +0000 2020",
      "lang" : "en",
      "synth_gate" : true
    }
  }
]
"#;

const LIKE_JS: &str = r#"window.YTD.like.part0 = [
  {
    "like" : {
      "tweetId" : "999",
      "fullText" : "Catfooding liked fixture"
    }
  }
]
"#;

const DM_JS: &str = r#"window.YTD.direct_messages.part0 = [
  {
    "dmConversation" : {
      "conversationId" : "111-222",
      "messages" : [
        {
          "messageCreate" : {
            "id" : "555",
            "text" : "Catfooding dm fixture",
            "createdAt" : "2020-01-01T00:00:00.000Z",
            "senderId" : "111"
          }
        }
      ]
    }
  }
]
"#;

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

fn write_x_zip(path: &Path) {
    let file = File::create(path).expect("create synthetic x archive zip");
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("data/account.js", options)
        .expect("zip start account");
    zip.write_all(ACCOUNT_JS.as_bytes())
        .expect("zip write account");
    zip.start_file("data/tweets.js", options)
        .expect("zip start tweets");
    zip.write_all(TWEETS_JS.as_bytes())
        .expect("zip write tweets");
    zip.start_file("data/like.js", options)
        .expect("zip start like");
    zip.write_all(LIKE_JS.as_bytes()).expect("zip write like");
    zip.start_file("data/direct-messages.js", options)
        .expect("zip start dm");
    zip.write_all(DM_JS.as_bytes()).expect("zip write dm");
    zip.finish().expect("zip finish");
}

fn write_x_dir(dir: &Path) {
    let data = dir.join("data");
    fs::create_dir_all(&data).expect("synthetic x data dir");
    fs::write(data.join("account.js"), ACCOUNT_JS).expect("write account.js");
    fs::write(data.join("tweets.js"), TWEETS_JS).expect("write tweets.js");
    fs::write(data.join("like.js"), LIKE_JS).expect("write like.js");
    fs::write(data.join("direct-messages.js"), DM_JS).expect("write direct-messages.js");
}

#[test]
fn infer_x_zip_service_is_social_x() {
    let root = test_dir("infer-x-zip");
    let zip_path = root.join("twitter-2026-01-01-aaaa.zip");
    write_x_zip(&zip_path);

    let scope = infer_ingest_scope(&zip_path).expect("infer x zip");
    assert_eq!(scope.service, "social/x");
    assert_eq!(scope.account, "synthuser");
    assert!(
        !scope.account.contains('@'),
        "account must never be an email from account.js"
    );
    assert_ne!(scope.account, SYNTH_EMAIL);

    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&zip_path))
        .expect("resolve x zip archive");
    assert_not_operator_memex(&archive);
    assert_eq!(
        archive,
        home.join("memex")
            .join("social")
            .join("x")
            .join("synthuser.majestic")
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn infer_x_dir_service_is_social_x() {
    let root = test_dir("infer-x-dir");
    let export = root.join("twitter-export");
    write_x_dir(&export);

    let scope = infer_ingest_scope(&export).expect("infer x dir");
    assert_eq!(scope.service, "social/x");
    assert_eq!(scope.account, "synthuser");

    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&export))
        .expect("resolve x dir archive");
    assert_not_operator_memex(&archive);
    assert_eq!(
        archive,
        home.join("memex")
            .join("social")
            .join("x")
            .join("synthuser.majestic")
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ingest_x_zip_is_lossless_and_searchable() {
    let root = test_dir("ingest-x-zip");
    let zip_path = root.join("twitter-2026-01-01-aaaa.zip");
    write_x_zip(&zip_path);
    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&zip_path))
        .expect("resolve x archive");
    assert_not_operator_memex(&archive);

    let report = ingest(&archive, std::slice::from_ref(&zip_path)).expect("ingest synthetic x zip");
    assert_eq!(report.exports, 1);
    assert_eq!(
        report.conversations, 3,
        "one tweet, one like, one dm conversation, got {}",
        report.conversations
    );

    let opened = Archive::open(&archive).expect("open x archive");
    let root_rec = opened.deserialize_root().expect("rkyv deserialize");
    let tweet = root_rec
        .conversations
        .iter()
        .find(|row| row.item.conversation.id.as_deref() == Some("tweet-123"))
        .expect("tweet conversation");
    let extra = match tweet.item.conversation.extra.get("tweet") {
        Some(JsonAtom::Object(fields)) => fields,
        other => panic!("tweet leftover must keep the inner object, got {other:?}"),
    };
    assert_eq!(
        extra.get("synth_gate"),
        Some(&JsonAtom::Bool(true)),
        "unknown tweet keys must survive as leftover extra"
    );
    assert_eq!(
        extra.get("full_text"),
        Some(&JsonAtom::String(SYNTH_TWEET.to_owned()))
    );

    let tweet_hits = search(&opened, SYNTH_TWEET, false).expect("search tweet");
    assert!(
        !tweet_hits.is_empty(),
        "synthetic tweet text must be searchable"
    );
    let like_hits = search(&opened, SYNTH_LIKE, false).expect("search like");
    assert!(
        !like_hits.is_empty(),
        "synthetic liked text must be searchable"
    );
    let dm_hits = search(&opened, SYNTH_DM, false).expect("search dm");
    assert!(!dm_hits.is_empty(), "synthetic dm text must be searchable");

    let email_hits = search(&opened, SYNTH_EMAIL, false).expect("search email");
    assert!(
        email_hits.is_empty(),
        "account.js email must not become a searchable span"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ingest_x_skips_duplicate_zip_bytes() {
    let root = test_dir("ingest-x-dup-zip");
    let first = root.join("twitter-2026-01-01-aaaa.zip");
    let second = root.join("twitter-2026-01-01-bbbb.zip");
    write_x_zip(&first);
    fs::copy(&first, &second).expect("copy identical x zip bytes under a second name");

    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&first))
        .expect("resolve x archive");
    assert_not_operator_memex(&archive);

    let report =
        ingest(&archive, &[first.clone(), second.clone()]).expect("ingest synthetic x zips");
    assert_eq!(
        report.exports, 1,
        "duplicate zip bytes must not be ingested, got {} exports",
        report.exports
    );
    assert_eq!(
        report.conversations, 3,
        "one tweet, one like, one dm from the kept zip, got {}",
        report.conversations
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn default_search_does_not_scan_live_x_zip() {
    let home = test_dir("default-search-x-zip");
    assert!(
        !home.starts_with(operator_memex()),
        "tests must not ingest into the operator memex directory"
    );
    let downloads = home.join("Downloads");
    fs::create_dir_all(&downloads).expect("downloads dir");
    let zip_path = downloads.join("twitter-2026-01-01-aaaa.zip");
    write_x_zip(&zip_path);
    assert!(!home.join("memex").exists(), "this case is no archives yet");

    let err = write_search_all(&home.join("memex"), SYNTH_TWEET, false, &mut Vec::new())
        .expect_err("default search does not walk live X zips");
    let text = err.to_string();
    assert!(
        text.contains("no archives found"),
        "empty memex must say no archives found, got {text}"
    );

    let _ = fs::remove_dir_all(&home);
}
