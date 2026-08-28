use std::fs;
use std::path::{Path, PathBuf};

use majestic::ingest::ingest;
use majestic::{
    infer_grok_export_scope, list_memex_archives, resolve_ingest_archive, scoped_archive_path,
};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-export.json")
}

fn operator_archive() -> PathBuf {
    PathBuf::from("/home/hunter/memex/archive.majestic")
}

fn memex_dir(home: &Path) -> PathBuf {
    home.join("memex")
}

fn operator_archive_fingerprint() -> Option<(std::time::SystemTime, u64)> {
    let meta = fs::metadata(operator_archive()).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[test]
fn scoped_archive_path_uses_memex_dir_not_a_memex_child() {
    let memex_dir = PathBuf::from("/tmp/custom-memex-dir");
    let got = scoped_archive_path(&memex_dir, "agents/grok", "personal").expect("scope");
    assert_eq!(
        got,
        memex_dir
            .join("agents")
            .join("grok")
            .join("personal.majestic"),
        "first argument is the data directory; do not append another memex folder"
    );
    assert_ne!(
        got,
        memex_dir
            .join("memex")
            .join("agents")
            .join("grok")
            .join("personal.majestic"),
        "must not join memex onto an already-resolved memex_dir"
    );
}

#[test]
fn list_memex_archives_lists_memex_dir_not_a_memex_child() {
    let root = std::env::temp_dir().join(format!("majestic-list-memex-dir-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let memex_dir = root.join("custom-archives");
    let grok_dir = memex_dir.join("agents").join("grok");
    fs::create_dir_all(&grok_dir).expect("custom grok dir");
    let wanted = grok_dir.join("custom.majestic");
    fs::write(&wanted, b"").expect("touch custom archive");
    let decoy_dir = memex_dir.join("memex").join("agents").join("grok");
    fs::create_dir_all(&decoy_dir).expect("decoy memex child");
    let decoy = decoy_dir.join("decoy.majestic");
    fs::write(&decoy, b"").expect("touch decoy under memex child");

    let listed = list_memex_archives(&memex_dir).expect("list the given data directory");
    assert!(
        listed.contains(&wanted),
        "must list archives under memex_dir, got {listed:?}"
    );
    assert!(
        !listed.contains(&decoy),
        "must not treat memex_dir/memex as the data directory, got {listed:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn list_memex_archives_finds_archive_and_majestic_suffixes() {
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    let home = std::env::temp_dir().join(format!(
        "majestic-fake-home-list-archives-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&home);
    let grok_dir = home.join("memex").join("agents").join("grok");
    let social_dir = home.join("memex").join("social").join("x");
    fs::create_dir_all(&grok_dir).expect("grok dir");
    fs::create_dir_all(&social_dir).expect("social dir");
    let archive = grok_dir.join("a.archive");
    let majestic = social_dir.join("b.majestic");
    fs::write(&archive, b"").expect("touch leftover .archive with no sibling .majestic");
    fs::write(&majestic, b"").expect("touch .majestic");
    fs::write(home.join("memex").join("README.md"), b"# memex\n").expect("root README");
    fs::write(grok_dir.join("README.md"), b"skip\n").expect("nested README");
    fs::create_dir_all(home.join("memex").join("skip.archive"))
        .expect("directory with archive suffix");

    let listed = list_memex_archives(&memex_dir(&home)).expect("list archives under fake home");
    assert!(
        listed.contains(&archive),
        "must list agents/grok/a.archive, got {listed:?}"
    );
    assert!(
        listed.contains(&majestic),
        "must list social/x/b.majestic, got {listed:?}"
    );
    assert_eq!(
        listed.len(),
        2,
        "README.md and directories must not be listed, got {listed:?}"
    );
    assert!(
        listed
            .iter()
            .all(|path| !path.starts_with(Path::new("/home/hunter/memex"))),
        "list must not walk the operator memex directory"
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn list_prefers_majestic_over_legacy_archive() {
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    let home = std::env::temp_dir().join(format!(
        "majestic-fake-home-list-prefers-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&home);
    let grok_dir = home.join("memex").join("agents").join("grok");
    fs::create_dir_all(&grok_dir).expect("grok dir");
    let leftover = grok_dir.join("a.archive");
    let majestic = grok_dir.join("a.majestic");
    let zst = grok_dir.join("a.majestic.zst");
    let orphan = grok_dir.join("b.archive");
    fs::write(&leftover, b"").expect("touch sibling leftover .archive");
    fs::write(&majestic, b"").expect("touch .majestic");
    fs::write(&zst, b"").expect("touch .zst that must not be listed");
    fs::write(&orphan, b"").expect("touch leftover .archive with no sibling");

    let listed = list_memex_archives(&memex_dir(&home)).expect("list archives under fake home");
    assert!(
        listed.contains(&majestic),
        "must list a.majestic, got {listed:?}"
    );
    assert!(
        listed.contains(&orphan),
        "must list leftover b.archive with no sibling .majestic, got {listed:?}"
    );
    assert!(
        !listed.contains(&leftover),
        "must not list a.archive when a.majestic exists, got {listed:?}"
    );
    assert!(
        !listed.contains(&zst),
        "must never list or mmap .zst, got {listed:?}"
    );
    assert_eq!(
        listed.len(),
        2,
        "only a.majestic and leftover b.archive, got {listed:?}"
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn scoped_archive_path_uses_service_folder_and_account_file() {
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    let home = std::env::temp_dir().join("majestic-fake-home-scope");
    let memex = memex_dir(&home);
    let got = scoped_archive_path(&memex, "agents/grok", "personal").expect("scope");
    assert_eq!(
        got,
        memex.join("agents").join("grok").join("personal.majestic")
    );
}

#[test]
fn scoped_archive_path_grok_uses_majestic_suffix() {
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    let home = std::env::temp_dir().join("majestic-fake-home-agents-suffix");
    for (service, account, rel) in [
        ("agents/grok", "personal", "agents/grok/personal.majestic"),
        (
            "agents/chatgpt",
            "fixture",
            "agents/chatgpt/fixture.majestic",
        ),
        (
            "agents/reports",
            "fixture",
            "agents/reports/fixture.majestic",
        ),
        (
            "agents/grok-oss",
            "fixture",
            "agents/grok-oss/fixture.majestic",
        ),
    ] {
        let got = scoped_archive_path(&memex_dir(&home), service, account).expect("scope");
        assert!(
            got.ends_with(rel),
            "{service} must use .majestic, got {}",
            got.display()
        );
        let name = got
            .file_name()
            .and_then(|name| name.to_str())
            .expect("archive file name");
        assert!(
            name.ends_with(".majestic"),
            "{service} must use .majestic, got {name}"
        );
        assert!(
            !name.ends_with(".archive"),
            "{service} must not write .archive, got {name}"
        );
    }
}

#[test]
fn scoped_archive_path_grok_business_uses_majestic_suffix() {
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    let home = std::env::temp_dir().join("majestic-fake-home-grok-business");
    let memex = memex_dir(&home);
    let got = scoped_archive_path(&memex, "agents/grok", "business").expect("scope");
    assert_eq!(
        got,
        memex.join("agents").join("grok").join("business.majestic")
    );
}

#[test]
fn scoped_archive_path_keeps_service_slashes() {
    let home = std::env::temp_dir().join("majestic-fake-home-slashes");
    let memex = memex_dir(&home);
    let got = scoped_archive_path(&memex, "social/x", "cryptoquick").expect("scope");
    assert_eq!(
        got,
        memex.join("social").join("x").join("cryptoquick.majestic")
    );
}

#[test]
fn scoped_archive_path_grok_oss_uses_majestic_suffix() {
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    let home = std::env::temp_dir().join("majestic-fake-home-grok-oss");
    let memex = memex_dir(&home);
    let got = scoped_archive_path(&memex, "agents/grok-oss", "fixture").expect("scope");
    assert_eq!(
        got,
        memex
            .join("agents")
            .join("grok-oss")
            .join("fixture.majestic")
    );
}

#[test]
fn scoped_archive_path_reports_uses_majestic_suffix() {
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    let home = std::env::temp_dir().join("majestic-fake-home-reports");
    let memex = memex_dir(&home);
    let got = scoped_archive_path(&memex, "agents/reports", "fixture").expect("scope");
    assert_eq!(
        got,
        memex
            .join("agents")
            .join("reports")
            .join("fixture.majestic")
    );
}

#[test]
fn scoped_archive_path_obsidian_uses_majestic_suffix_and_allows_space() {
    let home = std::env::temp_dir().join("majestic-fake-home-obsidian");
    let memex = memex_dir(&home);
    let got = scoped_archive_path(&memex, "notes/obsidian", "Obsidian Vault").expect("scope");
    assert_eq!(
        got,
        memex
            .join("notes")
            .join("obsidian")
            .join("Obsidian Vault.majestic")
    );
}

#[test]
fn scoped_archive_path_markdown_uses_majestic_suffix() {
    let home = std::env::temp_dir().join("majestic-fake-home-markdown");
    let memex = memex_dir(&home);
    let got = scoped_archive_path(&memex, "notes/markdown", "tiny-markdown").expect("scope");
    assert_eq!(
        got,
        memex
            .join("notes")
            .join("markdown")
            .join("tiny-markdown.majestic")
    );
}

#[test]
fn scoped_archive_path_rejects_dotdot() {
    let home = std::env::temp_dir().join("majestic-fake-home-dotdot");
    for (service, account) in [
        ("..", "personal"),
        ("../agents", "personal"),
        ("agents/..", "personal"),
        ("agents/../grok", "personal"),
        ("agents/grok", ".."),
        ("agents/grok", "../personal"),
        ("agents/grok", "personal/.."),
    ] {
        let err = scoped_archive_path(&memex_dir(&home), service, account).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains(".."),
            "service={service:?} account={account:?} must reject '..', got {text}"
        );
    }
}

#[test]
fn scoped_archive_path_rejects_empty_service() {
    let home = std::env::temp_dir().join("majestic-fake-home-empty-service");
    let err = scoped_archive_path(&memex_dir(&home), "", "personal").unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("empty"),
        "empty service must error in plain English, got {text}"
    );
}

#[test]
fn scoped_archive_path_rejects_empty_account() {
    let home = std::env::temp_dir().join("majestic-fake-home-empty-account");
    let err = scoped_archive_path(&memex_dir(&home), "agents/grok", "").unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("empty"),
        "empty account must error in plain English, got {text}"
    );
}

#[test]
fn ingest_creates_service_account_parent_dirs() {
    let home = std::env::temp_dir().join(format!(
        "majestic-fake-home-scoped-ingest-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).expect("fake home");
    let archive = scoped_archive_path(&memex_dir(&home), "agents/grok", "personal").expect("scope");
    let operator_memex = Path::new("/home/hunter/memex");
    assert!(
        !archive.starts_with(operator_memex),
        "tests must not ingest into the operator memex directory"
    );
    let operator_before = operator_archive_fingerprint();
    assert!(!home.join("memex").exists());

    ingest(&archive, &[fixture_path()]).expect("ingest into scoped archive");

    assert!(
        archive.is_file(),
        "ingest must write the scoped service/account archive"
    );
    assert!(
        home.join("memex").join("agents").join("grok").is_dir(),
        "ingest must create the service folder"
    );
    match operator_before {
        Some((modified, len)) => {
            let after = fs::metadata(operator_archive())
                .expect("leftover operator archive.majestic must stay untouched");
            assert_eq!(after.len(), len);
            assert_eq!(after.modified().ok(), Some(modified));
        }
        None => {
            assert!(
                !operator_archive().exists(),
                "ingest must not write /home/hunter/memex/archive.majestic"
            );
        }
    }
    let _ = fs::remove_dir_all(&home);
}

fn grok_export_fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-grok-export")
}

fn write_synth_grok_export(root: &Path, x_username: Option<&str>) {
    let export_dir = root.join("ttl/30d/export_data/synth-user");
    fs::create_dir_all(&export_dir).expect("synth export dir");
    fs::copy(fixture_path(), export_dir.join("prod-grok-backend.json"))
        .expect("copy synthetic backend");
    let user = match x_username {
        Some(name) => {
            format!(r#"{{"xUsername":"{name}","sessionTierId":"3","grokDb":"legacy-copy"}}"#)
        }
        None => r#"{"sessionTierId":"3","grokDb":"legacy-copy"}"#.to_owned(),
    };
    let auth = format!(r#"{{"user":{user},"teams":[]}}"#);
    fs::write(export_dir.join("prod-mc-auth-mgmt-api.json"), auth).expect("write synth auth");
}

#[test]
fn infer_grok_export_service_is_agents_grok() {
    let scope = infer_grok_export_scope(&grok_export_fixture_dir()).expect("infer service");
    assert_eq!(scope.service, "agents/grok");
}

#[test]
fn infer_grok_export_account_is_x_username() {
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    let scope = infer_grok_export_scope(&grok_export_fixture_dir()).expect("infer account");
    assert_eq!(scope.account, "synthhandle");
    let home = Path::new("/tmp/majestic-infer-x-username-home");
    let path =
        scoped_archive_path(&memex_dir(home), &scope.service, &scope.account).expect("scope path");
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    assert!(
        path.ends_with("agents/grok/synthhandle.majestic"),
        "inferred path must end with agents/grok/synthhandle.majestic, got {}",
        path.display()
    );
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("archive file name");
    assert_eq!(name, "synthhandle.majestic");
    assert_ne!(
        name, "personal.majestic",
        "account stem is user.xUsername, not personal, unless that is the username"
    );
    assert!(
        name.ends_with(".majestic"),
        "agents/grok must use .majestic, not leftover .archive"
    );
}

#[test]
fn infer_account_override_wins() {
    let home = std::env::temp_dir().join(format!(
        "majestic-infer-account-override-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).expect("fake home");
    let fixture = grok_export_fixture_dir();
    let archive = resolve_ingest_archive(
        &home,
        None,
        Some("personal"),
        std::slice::from_ref(&fixture),
    )
    .expect("account flag overrides xUsername");
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    assert_eq!(
        archive,
        home.join("memex")
            .join("agents")
            .join("grok")
            .join("personal.majestic")
    );
    let operator_memex = Path::new("/home/hunter/memex");
    assert!(
        !archive.starts_with(operator_memex),
        "tests must not ingest into the operator memex directory"
    );
    let operator_before = operator_archive_fingerprint();
    ingest(&archive, std::slice::from_ref(&fixture)).expect("ingest override stem");
    assert!(
        archive.is_file(),
        "--account personal must write personal.majestic"
    );
    match operator_before {
        Some((modified, len)) => {
            let after = fs::metadata(operator_archive())
                .expect("leftover operator archive.majestic must stay untouched");
            assert_eq!(after.len(), len);
            assert_eq!(after.modified().ok(), Some(modified));
        }
        None => {
            assert!(
                !operator_archive().exists(),
                "ingest must not write /home/hunter/memex/archive.majestic"
            );
        }
    }
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn infer_fails_without_username_or_flag() {
    let dir = std::env::temp_dir().join(format!(
        "majestic-infer-missing-username-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp export");
    write_synth_grok_export(&dir, None);
    let err = infer_grok_export_scope(&dir).expect_err("missing xUsername");
    let text = err.to_string();
    assert!(
        text.contains("xUsername"),
        "missing user.xUsername must name that field in plain English, got {text}"
    );
    assert!(
        text.contains("--account"),
        "missing user.xUsername must tell the operator to pass --account, got {text}"
    );
    let home = dir.join("fake-home");
    let resolve_err = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&dir))
        .expect_err("no --account and no xUsername");
    let resolve_text = resolve_err.to_string();
    assert!(
        resolve_text.contains("xUsername"),
        "ingest without --account and without xUsername must error in plain English, got {resolve_text}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn infer_fails_for_session_jsonl_without_flags() {
    let home = std::env::temp_dir().join(format!("majestic-infer-session-{}", std::process::id()));
    let session = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-session");
    let err = resolve_ingest_archive(&home, None, None, std::slice::from_ref(&session))
        .expect_err("session JSONL is not a Grok export dump");
    let text = err.to_string();
    assert!(
        text.contains("--service") || text.contains("--account") || text.contains("-o"),
        "non-Grok input without flags must ask for --service/--account or -o, got {text}"
    );
}

#[test]
fn infer_fails_when_accounts_disagree() {
    let dir = std::env::temp_dir().join(format!("majestic-infer-disagree-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let first = dir.join("first");
    let second = dir.join("second");
    fs::create_dir_all(&first).expect("first dump");
    fs::create_dir_all(&second).expect("second dump");
    write_synth_grok_export(&first, Some("synthhandle"));
    write_synth_grok_export(&second, Some("otherhandle"));
    let home = dir.join("fake-home");
    let err = resolve_ingest_archive(&home, None, None, &[first, second])
        .expect_err("disagreeing xUsername");
    let text = err.to_string();
    assert!(
        text.contains("account") || text.contains("--account"),
        "disagreeing inferred accounts must error in plain English, got {text}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn infer_matching_usernames_use_one_archive() {
    let dir = std::env::temp_dir().join(format!("majestic-infer-matching-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let first = dir.join("first");
    let second = dir.join("second");
    fs::create_dir_all(&first).expect("first dump");
    fs::create_dir_all(&second).expect("second dump");
    write_synth_grok_export(&first, Some("synthhandle"));
    write_synth_grok_export(&second, Some("synthhandle"));
    let home = dir.join("fake-home");
    fs::create_dir_all(&home).expect("fake home");
    let archive = resolve_ingest_archive(&home, None, None, &[first, second]).expect("matching");
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    assert!(
        archive.ends_with("agents/grok/synthhandle.majestic"),
        "matching xUsername values land on one .majestic, got {}",
        archive.display()
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn infer_account_override_does_not_need_x_username() {
    let dir = std::env::temp_dir().join(format!(
        "majestic-infer-override-missing-username-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp export");
    write_synth_grok_export(&dir, None);
    let home = dir.join("fake-home");
    let archive = resolve_ingest_archive(&home, None, Some("personal"), std::slice::from_ref(&dir))
        .expect("--account supplies the stem when user.xUsername is missing");
    // Suffix rule is now always `.majestic`; `.archive` is legacy read-only.
    assert!(
        archive.ends_with("agents/grok/personal.majestic"),
        "Grok-shaped dump with --account personal writes personal.majestic, got {}",
        archive.display()
    );
    let _ = fs::remove_dir_all(&dir);
}
