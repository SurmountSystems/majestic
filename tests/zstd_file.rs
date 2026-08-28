//! Named zstd CLI contracts: level 3, no dict, keep the input file.

use std::fs;
use std::path::PathBuf;

use majestic::archive::Archive;
use majestic::ingest::ingest;
use majestic::zstd_file::{
    COMPRESS_LEVEL, ZSTD_MAGIC, compress_to_zst, compress_to_zst_with_level, decompress_zst,
};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-export.json")
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("majestic-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn compress_writes_majestic_zst_keeps_original() {
    let dir = test_dir("compress-majestic-zst");
    let majestic = dir.join("foo.majestic");
    ingest(&majestic, std::slice::from_ref(&fixture_path())).expect("ingest uncompressed");
    assert!(
        majestic.is_file(),
        "ingest must write foo.majestic before compress"
    );

    let zst = compress_to_zst(&majestic).expect("compress next to input");
    assert_eq!(zst, dir.join("foo.majestic.zst"));
    let bytes = fs::read(&zst).expect("read zst");
    assert!(
        bytes.len() >= 4,
        "zst must include the 4-byte frame magic, got {} bytes",
        bytes.len()
    );
    assert_eq!(
        &bytes[..4],
        &ZSTD_MAGIC,
        "identify output by bytes 28 B5 2F FD"
    );
    assert!(
        majestic.is_file(),
        "compress must not delete the uncompressed input"
    );
    Archive::open(&majestic).expect("original still opens as mmap after compress");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn decompress_restores_majestic_without_deleting_zst() {
    let dir = test_dir("decompress-majestic-zst");
    let majestic = dir.join("foo.majestic");
    ingest(&majestic, std::slice::from_ref(&fixture_path())).expect("ingest uncompressed");
    let original = fs::read(&majestic).expect("read uncompressed");
    let zst = compress_to_zst(&majestic).expect("compress");
    fs::remove_file(&majestic).expect("remove uncompressed copy to prove restore");

    let restored = decompress_zst(&zst).expect("decompress");
    assert_eq!(restored, dir.join("foo.majestic"));
    assert!(zst.is_file(), "decompress must not delete the .zst");
    assert_eq!(
        fs::read(&restored).expect("read restored"),
        original,
        "decompress must write FILE without .zst"
    );
    Archive::open(&restored).expect("restored file opens as mmap");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn compress_to_zst_with_level_does_not_ignore_level() {
    let dir = test_dir("compress-config-level");
    let majestic = dir.join("foo.majestic");
    ingest(&majestic, std::slice::from_ref(&fixture_path())).expect("ingest uncompressed");
    let low_src = dir.join("foo-level1.majestic");
    let high_src = dir.join("foo-level19.majestic");
    fs::copy(&majestic, &low_src).expect("copy for level 1");
    fs::copy(&majestic, &high_src).expect("copy for level 19");
    let low = compress_to_zst_with_level(&low_src, 1).expect("level 1");
    let high = compress_to_zst_with_level(&high_src, 19).expect("level 19");
    let low_bytes = fs::read(&low).expect("read level 1");
    let high_bytes = fs::read(&high).expect("read level 19");
    assert_eq!(&low_bytes[..4], &ZSTD_MAGIC);
    assert_eq!(&high_bytes[..4], &ZSTD_MAGIC);
    assert_ne!(
        low_bytes, high_bytes,
        "zstd level from config must change the compressed bytes; level 1 and 19 must not match"
    );
    let default = compress_to_zst(&majestic).expect("default compress");
    let default_bytes = fs::read(&default).expect("read default");
    fs::remove_file(&default).expect("remove default zst so the next write is a new file");
    let via_const = compress_to_zst_with_level(&majestic, COMPRESS_LEVEL).expect("level const");
    let via_const_bytes = fs::read(&via_const).expect("read const level");
    assert_eq!(
        default_bytes, via_const_bytes,
        "compress_to_zst must use COMPRESS_LEVEL when no config level is passed"
    );
    let _ = fs::remove_dir_all(&dir);
}
