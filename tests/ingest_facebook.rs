//! Synthetic Facebook Download Your Information zip ingest.
//!
//! Never copy a live Downloads zip or JSON. Never print live post titles,
//! names, or photo filenames from Downloads.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use majestic::archive::Archive;
use majestic::ingest::{infer_ingest_scope, ingest};
use majestic::schema::JsonAtom;
use majestic::{resolve_ingest_archive, search};

const SYNTH_TITLE: &str = "Catfooding facebook fixture";

const POSTS_JSON: &str = r#"[
  {
    "timestamp": 1700000000,
    "title": "Catfooding facebook fixture",
    "data": [{"post": "Catfooding facebook fixture"}],
    "synth_gate": true
  }
]"#;

/// 1x1 PNG. Synthetic fixture bytes, not a live export photo.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
    0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xFE, 0xD4, 0xEF, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

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

fn write_facebook_zip(path: &Path) {
    let file = File::create(path).expect("create synthetic facebook zip");
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("your_facebook_activity/posts/your_posts.json", options)
        .expect("zip start json");
    zip.write_all(POSTS_JSON.as_bytes())
        .expect("zip write json");
    zip.start_file("your_facebook_activity/posts/media/synth-cat.png", options)
        .expect("zip start png");
    zip.write_all(TINY_PNG).expect("zip write png");
    zip.finish().expect("zip finish");
}

#[test]
fn infer_facebook_zip_service_is_social_meta() {
    let root = test_dir("infer-facebook-zip");
    let zip_path = root.join("facebook-synthuser-2026-01-01-aaaa.zip");
    write_facebook_zip(&zip_path);

    let scope = infer_ingest_scope(&zip_path).expect("infer facebook zip");
    assert_eq!(scope.service, "social/meta");
    assert_eq!(scope.account, "synthuser");
    assert!(
        !scope.account.contains('@'),
        "account must never be an email"
    );

    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&zip_path))
        .expect("resolve facebook zip archive");
    assert_not_operator_memex(&archive);
    assert_eq!(
        archive,
        home.join("memex")
            .join("social")
            .join("meta")
            .join("synthuser.majestic")
    );
    let name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .expect("archive file name");
    assert!(
        name.ends_with(".majestic"),
        "social/meta must use .majestic, got {name}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn infer_instagram_zip_service_is_social_meta() {
    let root = test_dir("infer-instagram-zip");
    let zip_path = root.join("instagram-synthuser-2026-01-01-aaaa.zip");
    let file = File::create(&zip_path).expect("create synthetic instagram zip");
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("your_instagram_activity/posts/your_posts.json", options)
        .expect("zip start json");
    zip.write_all(POSTS_JSON.as_bytes())
        .expect("zip write json");
    zip.finish().expect("zip finish");

    let scope = infer_ingest_scope(&zip_path).expect("infer instagram zip");
    assert_eq!(scope.service, "social/meta");
    assert_eq!(scope.account, "synthuser");
    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&zip_path))
        .expect("resolve instagram zip archive");
    assert_eq!(
        archive,
        home.join("memex")
            .join("social")
            .join("meta")
            .join("synthuser.majestic")
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ingest_facebook_skips_duplicate_zip_bytes() {
    let root = test_dir("ingest-facebook-dup-zip");
    let first = root.join("facebook-synthuser-2026-01-01-aaaa.zip");
    let second = root.join("facebook-synthuser-2026-01-01-bbbb.zip");
    write_facebook_zip(&first);
    fs::copy(&first, &second).expect("copy identical facebook zip bytes under a second name");

    let home = root.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&first))
        .expect("resolve facebook archive");
    assert_not_operator_memex(&archive);

    let report =
        ingest(&archive, &[first.clone(), second.clone()]).expect("ingest synthetic facebook zips");
    assert_eq!(
        report.exports, 1,
        "duplicate zip bytes must not be ingested, got {} exports",
        report.exports
    );
    assert_eq!(
        report.conversations, 1,
        "one synthetic post, not two from the duplicate zip, got {}",
        report.conversations
    );

    let opened = Archive::open(&archive).expect("open facebook archive");
    let root_rec = opened.deserialize_root().expect("rkyv deserialize");
    assert_eq!(root_rec.conversations.len(), 1);
    let post = &root_rec.conversations[0];
    assert_eq!(post.item.conversation.title.as_deref(), Some(SYNTH_TITLE));
    assert_eq!(
        post.item.conversation.extra.get("synth_gate"),
        Some(&JsonAtom::Bool(true)),
        "unknown Facebook keys must survive as leftover extra"
    );

    let hits = search(&opened, SYNTH_TITLE, false).expect("search facebook title");
    assert!(
        !hits.is_empty(),
        "synthetic facebook title must be searchable"
    );
    let ids: std::collections::HashSet<_> =
        hits.iter().map(|hit| hit.conversation_id.clone()).collect();
    assert_eq!(
        ids.len(),
        1,
        "duplicate zip must not produce a second searchable post, got {hits:?}"
    );

    assert_eq!(
        root_rec.assets.len(),
        1,
        "synthetic png must be cataloged once"
    );
    let asset = &root_rec.assets[0];
    assert_eq!(asset.size, TINY_PNG.len() as u64);
    assert_eq!(
        asset.blob_len, 0,
        "facebook photo bodies must not be embedded"
    );
    assert!(
        asset.relative_path.ends_with("synth-cat.png"),
        "catalog path must keep the synthetic inner name, got {}",
        asset.relative_path
    );

    let _ = fs::remove_dir_all(&root);
}
