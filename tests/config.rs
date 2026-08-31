//! Layered memex.toml: crate defaults, file, env path, CLI overlay.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use majestic::config::{CliOverlay, Config, config_file_path_from, expand_tilde};
use majestic::{HOME_SCAN_MAX_DEPTH, serve::DEFAULT_BIND, zstd_file::COMPRESS_LEVEL};

fn unique_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "majestic-config-{label}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("temp config dir");
    dir
}

#[test]
fn missing_config_file_uses_defaults() {
    let missing = PathBuf::from("/no/such/majestic-memex-config/memex.toml");
    assert!(!missing.exists(), "must not exist");
    let home = Path::new("/tmp/fake-home-missing-config");
    let cfg = Config::load_from_path_with_home(&missing, Some(home))
        .expect("missing file is not an error");
    assert_eq!(cfg.memex_dir, home.join("memex"));
    assert!(
        cfg.scan.skip_directories.is_empty(),
        "crate defaults must not list extra skip directories"
    );
    assert!(
        cfg.scan.skip_system_trash,
        "crate default skips system trash"
    );
    assert_eq!(cfg.scan.home_scan_max_depth, HOME_SCAN_MAX_DEPTH);
    assert!(!cfg.search.ignore_case);
    assert!(!cfg.search.fixed_strings);
    assert!(!cfg.search.word_regexp);
    assert_eq!(cfg.search.max_count, majestic::DEFAULT_SEARCH_MAX_COUNT);
    assert_eq!(cfg.search.format, majestic::SearchFormat::Human);
    assert_eq!(cfg.serve.bind, DEFAULT_BIND.to_string());
    assert_eq!(cfg.compress.level, COMPRESS_LEVEL);
    assert_eq!(cfg.zip.max_uncompressed_bytes, 8 * 1024 * 1024 * 1024);
    assert_eq!(cfg.log.filter, "info");
    assert!(cfg.jobs.rayon_threads.is_none());
    assert!(!cfg.bench_zstd.re_ingest);
    let agents_trash = home.join(".agents").join("trash").join("secret.json");
    assert!(
        !cfg.skips_directory(&agents_trash, home, None),
        "crate defaults must not skip ~/.agents/trash"
    );
}

#[test]
fn xdg_config_path_is_majestic_memex_toml() {
    let home = Path::new("/tmp/fake-home-xdg-config");
    let xdg = Path::new("/tmp/fake-xdg-config-home");
    let path = config_file_path_from(None, Some(xdg.as_os_str()), Some(home.as_os_str()));
    assert_eq!(path, xdg.join("majestic").join("memex.toml"));
    assert_ne!(
        path,
        xdg.join("memex").join("memex.toml"),
        "must not use $XDG_CONFIG_HOME/memex/"
    );
    assert_ne!(
        path,
        home.join("majestic").join("memex.toml"),
        "must not use ~/majestic/memex.toml"
    );
    assert_ne!(
        path,
        home.join(".config").join("memex").join("memex.toml"),
        "must not use ~/.config/memex/"
    );

    let without_xdg = config_file_path_from(None, None, Some(home.as_os_str()));
    assert_eq!(
        without_xdg,
        home.join(".config").join("majestic").join("memex.toml")
    );
}

#[test]
fn memex_config_env_selects_file() {
    let dir = unique_dir("memex-config-env");
    let custom = dir.join("custom.toml");
    fs::write(&custom, "memex_dir = \"/from-memex-config\"\n").expect("write custom toml");
    let path = config_file_path_from(
        Some(custom.as_os_str()),
        Some(OsStr::new("/tmp/xdg-unused")),
        Some(OsStr::new("/tmp/home-unused")),
    );
    assert_eq!(path, custom);
    let cfg = Config::load_from_path_with_home(&path, Some(Path::new("/unused")))
        .expect("load MEMEX_CONFIG file");
    assert_eq!(cfg.memex_dir, PathBuf::from("/from-memex-config"));

    let tilde_path = config_file_path_from(
        Some(OsStr::new("~/custom.toml")),
        None,
        Some(OsStr::new("/tmp/fake-home-memex-config")),
    );
    assert_eq!(
        tilde_path,
        PathBuf::from("/tmp/fake-home-memex-config/custom.toml")
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_overrides_config_file() {
    let dir = unique_dir("cli-overlay");
    let file = dir.join("memex.toml");
    fs::write(
        &file,
        "memex_dir = \"/from-file\"\n[search]\nignore_case = true\n",
    )
    .expect("write toml");
    let mut cfg =
        Config::load_from_path_with_home(&file, Some(Path::new("/home/fake"))).expect("load file");
    assert_eq!(cfg.memex_dir, PathBuf::from("/from-file"));
    assert!(cfg.search.ignore_case);
    assert_eq!(
        cfg.search.max_count,
        majestic::DEFAULT_SEARCH_MAX_COUNT,
        "omitted search.max_count must keep the default print cap"
    );
    cfg.overlay_cli(&CliOverlay {
        memex_dir: Some(PathBuf::from("/from-cli")),
        ..CliOverlay::default()
    });
    assert_eq!(
        cfg.memex_dir,
        PathBuf::from("/from-cli"),
        "CLI memex_dir must win over the file"
    );
    assert!(
        cfg.search.ignore_case,
        "unpassed search flags must keep the file value"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn default_config_skips_system_trash_not_agents_trash() {
    let cfg = Config::crate_defaults();
    let home = Path::new("/tmp/fake-home-scan-skip");
    let xdg_trash = home
        .join(".local")
        .join("share")
        .join("Trash")
        .join("files")
        .join("secret.json");
    let mount_trash = Path::new("/mnt/disk/.Trash-1000/files/secret.json");
    let user_trash = home.join(".Trash").join("files").join("secret.json");
    let agents_trash = home.join(".agents").join("trash").join("secret.json");
    let tmp = Path::new("/tmp");
    assert!(
        cfg.skips_directory(&xdg_trash, home, None),
        "XDG Trash must skip under crate defaults"
    );
    assert!(
        cfg.skips_directory(mount_trash, home, None),
        "mount-point .Trash- must skip under crate defaults"
    );
    assert!(
        cfg.skips_directory(&user_trash, home, None),
        "~/.Trash must skip under crate defaults"
    );
    assert!(
        !cfg.skips_directory(agents_trash.as_path(), home, None),
        "crate defaults must not skip ~/.agents/trash"
    );
    assert!(
        !cfg.skips_directory(tmp, home, None),
        "must not skip all of /tmp"
    );
}

#[test]
fn config_file_skip_directories_skips_agents_trash() {
    let dir = unique_dir("skip-agents");
    let file = dir.join("memex.toml");
    fs::write(&file, "[scan]\nskip_directories = [\"~/.agents/trash\"]\n")
        .expect("write skip toml");
    let home = Path::new("/tmp/fake-home-agents-skip");
    let cfg = Config::load_from_path_with_home(&file, Some(home)).expect("load skip file");
    let agents_trash = home.join(".agents").join("trash").join("secret.json");
    assert!(
        cfg.skips_directory(&agents_trash, home, None),
        "listed skip_directories must skip ~/.agents/trash"
    );
    assert_eq!(
        cfg.scan.skip_directories,
        vec![home.join(".agents").join("trash")]
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn expand_tilde_in_memex_dir_and_skip_directories() {
    let dir = unique_dir("tilde");
    let file = dir.join("memex.toml");
    fs::write(
        &file,
        "memex_dir = \"~/memex\"\n[scan]\nskip_directories = [\"~/.agents/trash\"]\n",
    )
    .expect("write tilde toml");
    let home = Path::new("/tmp/tilde-home");
    let cfg = Config::load_from_path_with_home(&file, Some(home)).expect("load");
    assert_eq!(cfg.memex_dir, home.join("memex"));
    assert_eq!(
        cfg.scan.skip_directories,
        vec![home.join(".agents").join("trash")]
    );
    assert_eq!(
        expand_tilde(Path::new("~/custom.toml"), home),
        home.join("custom.toml")
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn skip_system_trash_false_does_not_skip_xdg_trash() {
    let mut cfg = Config::crate_defaults();
    cfg.scan.skip_system_trash = false;
    let home = Path::new("/tmp/fake-home-no-system-trash");
    let xdg_trash = home
        .join(".local")
        .join("share")
        .join("Trash")
        .join("files")
        .join("secret.json");
    assert!(!cfg.skips_directory(&xdg_trash, home, None));
}
