//! Layered user configuration for the `memex` CLI.
//!
//! Later layers win: crate defaults, then the TOML file, then `MEMEX_*`
//! environment variables, then CLI flags the user actually passed. A missing
//! file is not an error. `MEMEX_CONFIG` selects another TOML path (tilde
//! expanded). There is no `--config` flag. The default file is
//! `$XDG_CONFIG_HOME/majestic/memex.toml` (usually
//! `~/.config/majestic/memex.toml`), via the `directories` crate
//! [`directories::BaseDirs::config_dir`]. This is not `~/.config/memex/` and
//! not `~/majestic/memex.toml`. Nested env keys use a double underscore after
//! the `MEMEX_` prefix (`MEMEX_SEARCH__IGNORE_CASE`). `MEMEX_CONFIG` is the
//! file path only. `MEMEX_LOG` and `RUST_LOG` set the terminal tracing filter;
//! they are not this `[log]` table.
//!
//! Layers use figment plus serde plus toml. Figment overlays defaults, a file,
//! env, and CLI. confy cannot overlay layers. Clap has no env feature; overlay
//! uses `ValueSource::CommandLine` so clap defaults do not wipe the file.
//! Committed example: `docs/memex.toml.example`.
//!
//! Home scan skips system trash when [`ScanConfig::skip_system_trash`] (default
//! true). Extra skips are [`ScanConfig::skip_directories`] (crate default
//! empty). Crate defaults do not skip `~/.agents/trash`.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::ingest::HOME_SCAN_MAX_DEPTH;
use crate::serve::DEFAULT_BIND;
use crate::trie::SearchFormat;
use crate::zip::MAX_UNCOMPRESSED;
use crate::zstd_file::COMPRESS_LEVEL;

/// Default memex data directory before tilde expansion.
const DEFAULT_MEMEX_DIR: &str = "~/memex";

/// Loaded memex options. Path-valued keys are tilde-expanded after load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Archive directory. Default `~/memex`.
    #[serde(default = "default_memex_dir")]
    pub memex_dir: PathBuf,
    /// Home scan and skip lists.
    #[serde(default)]
    pub scan: ScanConfig,
    /// `memex search` flags.
    #[serde(default)]
    pub search: SearchConfig,
    /// `memex ingest` paths.
    #[serde(default)]
    pub ingest: IngestConfig,
    /// `--service` and `--account`.
    #[serde(default)]
    pub scope: ScopeConfig,
    /// `memex stats` archive path.
    #[serde(default)]
    pub stats: StatsConfig,
    /// `memex serve --bind`.
    #[serde(default)]
    pub serve: ServeConfig,
    /// `memex compress` zstd level.
    #[serde(default)]
    pub compress: CompressConfig,
    /// Zip ingest limits.
    #[serde(default)]
    pub zip: ZipConfig,
    /// Console tracing filter when `MEMEX_LOG` and `RUST_LOG` are unset.
    #[serde(default)]
    pub log: LogConfig,
    /// Rayon worker count. `None` is the rayon default.
    #[serde(default)]
    pub jobs: JobsConfig,
    /// `memex bench-zstd` paths and flags.
    #[serde(default)]
    pub bench_zstd: BenchZstdConfig,
}

/// Home scan depth and directory skip lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanConfig {
    /// Directory levels below `$HOME` for `memex ingest` with no paths.
    #[serde(default = "default_home_scan_max_depth")]
    pub home_scan_max_depth: u32,
    /// Extra paths to skip (prefix or consecutive path-component match).
    /// Crate default is empty. Do not put `~/.agents/trash` here for everyone.
    #[serde(default)]
    pub skip_directories: Vec<PathBuf>,
    /// Skip XDG Trash, directory name `.Trash`, and names starting with `.Trash-`.
    /// Default true. Does not skip all of `/tmp`.
    #[serde(default = "default_true")]
    pub skip_system_trash: bool,
}

/// Durable `memex search` flags. The pattern itself is not stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchConfig {
    /// Case insensitive (`-i`). Default false.
    #[serde(default)]
    pub ignore_case: bool,
    /// Phrase or literal (`-F`). Default false.
    #[serde(default)]
    pub fixed_strings: bool,
    /// Whole word (`-w`). Default false.
    #[serde(default)]
    pub word_regexp: bool,
    /// One archive path. `None` means every archive under `memex_dir`.
    #[serde(default)]
    pub archive: Option<PathBuf>,
    /// Snippet groups printed. `0` means no cap. Default 100.
    #[serde(default = "default_search_max_count")]
    pub max_count: usize,
    /// Search report encoding. Default `human`. Stdout is the report.
    #[serde(default)]
    pub format: SearchFormat,
}

/// Durable `memex ingest` paths. Empty `inputs` means home scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IngestConfig {
    /// Output archive (`-o`). `None` means infer or home scan.
    #[serde(default)]
    pub output: Option<PathBuf>,
    /// Input dumps. Empty means scan `$HOME`.
    #[serde(default)]
    pub inputs: Vec<PathBuf>,
}

/// Folder and username file stem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ScopeConfig {
    /// Folder under the memex directory (`agents/grok`).
    #[serde(default)]
    pub service: Option<String>,
    /// Username file stem.
    #[serde(default)]
    pub account: Option<String>,
}

/// Durable `memex stats` archive path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StatsConfig {
    /// `.majestic` path. `None` means `--service` and `--account`, or error.
    #[serde(default)]
    pub archive: Option<PathBuf>,
}

/// HTTP MCP listen address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServeConfig {
    /// Listen address. Default `127.0.0.1:8741`.
    #[serde(default = "default_bind")]
    pub bind: String,
}

/// zstd compress options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressConfig {
    /// zstd level. Default 3. No dictionary.
    #[serde(default = "default_compress_level")]
    pub level: i32,
}

/// Zip ingest skip limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZipConfig {
    /// Skip zip entries larger than this uncompressed size. Default 8 GiB.
    #[serde(default = "default_max_uncompressed")]
    pub max_uncompressed_bytes: u64,
}

/// Console tracing filter. The journal is always verbose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogConfig {
    /// Used when `MEMEX_LOG` and `RUST_LOG` are unset. Default `info` means
    /// error and info on the terminal, not warn. The systemd journal still
    /// keeps the full log.
    #[serde(default = "default_log_filter")]
    pub filter: String,
}

/// Rayon pool size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct JobsConfig {
    /// Worker threads for ingest hashing. `None` leaves rayon's default.
    /// Search does not use this. Search worker count is
    /// `min(file count, available parallelism)`.
    #[serde(default)]
    pub rayon_threads: Option<usize>,
}

/// `memex bench-zstd` options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchZstdConfig {
    /// Directory of uncompressed archives. Default `~/memex`.
    #[serde(default = "default_memex_dir")]
    pub dir: PathBuf,
    /// JSONL report path. Default `~/memex/bench-zstd/report.jsonl`.
    #[serde(default = "default_bench_jsonl")]
    pub jsonl: PathBuf,
    /// Markdown report path. Default `~/memex/bench-zstd/report.md`.
    #[serde(default = "default_bench_report")]
    pub report: PathBuf,
    /// Time first vs second ingest. Default false.
    #[serde(default)]
    pub re_ingest: bool,
}

/// CLI values that were actually passed. `None` keeps the file or default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOverlay {
    /// `--memex-dir`.
    pub memex_dir: Option<PathBuf>,
    /// `search -i`.
    pub ignore_case: Option<bool>,
    /// `search -F`.
    pub fixed_strings: Option<bool>,
    /// `search -w`.
    pub word_regexp: Option<bool>,
    /// `--service`.
    pub service: Option<String>,
    /// `--account`.
    pub account: Option<String>,
    /// `ingest -o`.
    pub ingest_output: Option<PathBuf>,
    /// `ingest` positional inputs when the user passed at least one.
    pub ingest_inputs: Option<Vec<PathBuf>>,
    /// `search` archive path.
    pub search_archive: Option<PathBuf>,
    /// `search --max-count`.
    pub max_count: Option<usize>,
    /// `search --format`.
    pub search_format: Option<SearchFormat>,
    /// `stats` archive path.
    pub stats_archive: Option<PathBuf>,
    /// `serve --bind`.
    pub serve_bind: Option<String>,
    /// `bench-zstd` directory.
    pub bench_zstd_dir: Option<PathBuf>,
    /// `bench-zstd --jsonl`.
    pub bench_zstd_jsonl: Option<PathBuf>,
    /// `bench-zstd --report`.
    pub bench_zstd_report: Option<PathBuf>,
    /// `bench-zstd --re-ingest`.
    pub bench_zstd_re_ingest: Option<bool>,
}

fn default_memex_dir() -> PathBuf {
    PathBuf::from(DEFAULT_MEMEX_DIR)
}

fn default_true() -> bool {
    true
}

fn default_home_scan_max_depth() -> u32 {
    HOME_SCAN_MAX_DEPTH
}

fn default_bind() -> String {
    DEFAULT_BIND.to_string()
}

fn default_compress_level() -> i32 {
    COMPRESS_LEVEL
}

fn default_max_uncompressed() -> u64 {
    MAX_UNCOMPRESSED
}

fn default_log_filter() -> String {
    "info".into()
}

fn default_search_max_count() -> usize {
    crate::trie::DEFAULT_SEARCH_MAX_COUNT
}

fn default_bench_jsonl() -> PathBuf {
    PathBuf::from("~/memex/bench-zstd/report.jsonl")
}

fn default_bench_report() -> PathBuf {
    PathBuf::from("~/memex/bench-zstd/report.md")
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            home_scan_max_depth: HOME_SCAN_MAX_DEPTH,
            skip_directories: Vec::new(),
            skip_system_trash: true,
        }
    }
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
        }
    }
}

impl Default for CompressConfig {
    fn default() -> Self {
        Self {
            level: COMPRESS_LEVEL,
        }
    }
}

impl Default for ZipConfig {
    fn default() -> Self {
        Self {
            max_uncompressed_bytes: MAX_UNCOMPRESSED,
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: default_log_filter(),
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            ignore_case: false,
            fixed_strings: false,
            word_regexp: false,
            archive: None,
            max_count: default_search_max_count(),
            format: SearchFormat::Human,
        }
    }
}

impl Default for BenchZstdConfig {
    fn default() -> Self {
        Self {
            dir: default_memex_dir(),
            jsonl: default_bench_jsonl(),
            report: default_bench_report(),
            re_ingest: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::crate_defaults()
    }
}

impl Config {
    /// Crate defaults. Extra `skip_directories` is empty. System trash is skipped.
    /// `~/.agents/trash` is not skipped here.
    pub fn crate_defaults() -> Self {
        Self {
            memex_dir: default_memex_dir(),
            scan: ScanConfig::default(),
            search: SearchConfig::default(),
            ingest: IngestConfig::default(),
            scope: ScopeConfig::default(),
            stats: StatsConfig::default(),
            serve: ServeConfig::default(),
            compress: CompressConfig::default(),
            zip: ZipConfig::default(),
            log: LogConfig::default(),
            jobs: JobsConfig::default(),
            bench_zstd: BenchZstdConfig::default(),
        }
    }

    /// Load from `MEMEX_CONFIG` or the XDG file, then `MEMEX_*` env (not `MEMEX_CONFIG`).
    pub fn load() -> Result<Self, Error> {
        let path = config_file_path();
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from);
        Self::load_layers(&path, home.as_deref(), true)
    }

    /// Load crate defaults, then this file (missing is OK). Does not read `MEMEX_*` env.
    pub fn load_from_path_with_home(path: &Path, home: Option<&Path>) -> Result<Self, Error> {
        Self::load_layers(path, home, false)
    }

    fn load_layers(path: &Path, home: Option<&Path>, env: bool) -> Result<Self, Error> {
        let mut figment =
            Figment::from(Serialized::defaults(Self::crate_defaults())).merge(Toml::file(path));
        if env {
            figment = figment.merge(Env::prefixed("MEMEX_").ignore(&["CONFIG"]).split("__"));
        }
        let mut config: Self = figment
            .extract()
            .map_err(|error| Error::InvalidParams(format!("config: {error}")))?;
        if let Some(home) = home {
            config.expand_tildes(home);
        }
        Ok(config)
    }

    /// Expand `~` and `~/` in path-valued keys using `home`.
    pub fn expand_tildes(&mut self, home: &Path) {
        self.memex_dir = expand_tilde(&self.memex_dir, home);
        for skip in &mut self.scan.skip_directories {
            *skip = expand_tilde(skip, home);
        }
        if let Some(output) = &self.ingest.output {
            self.ingest.output = Some(expand_tilde(output, home));
        }
        for input in &mut self.ingest.inputs {
            *input = expand_tilde(input, home);
        }
        if let Some(archive) = &self.search.archive {
            self.search.archive = Some(expand_tilde(archive, home));
        }
        if let Some(archive) = &self.stats.archive {
            self.stats.archive = Some(expand_tilde(archive, home));
        }
        self.bench_zstd.dir = expand_tilde(&self.bench_zstd.dir, home);
        self.bench_zstd.jsonl = expand_tilde(&self.bench_zstd.jsonl, home);
        self.bench_zstd.report = expand_tilde(&self.bench_zstd.report, home);
    }

    /// Apply CLI flags the user passed. `None` overlay fields keep the current value.
    pub fn overlay_cli(&mut self, overlay: &CliOverlay) {
        if let Some(dir) = &overlay.memex_dir {
            self.memex_dir = dir.clone();
        }
        if let Some(value) = overlay.ignore_case {
            self.search.ignore_case = value;
        }
        if let Some(value) = overlay.fixed_strings {
            self.search.fixed_strings = value;
        }
        if let Some(value) = overlay.word_regexp {
            self.search.word_regexp = value;
        }
        if let Some(service) = &overlay.service {
            self.scope.service = Some(service.clone());
        }
        if let Some(account) = &overlay.account {
            self.scope.account = Some(account.clone());
        }
        if let Some(output) = &overlay.ingest_output {
            self.ingest.output = Some(output.clone());
        }
        if let Some(inputs) = &overlay.ingest_inputs {
            self.ingest.inputs = inputs.clone();
        }
        if let Some(archive) = &overlay.search_archive {
            self.search.archive = Some(archive.clone());
        }
        if let Some(value) = overlay.max_count {
            self.search.max_count = value;
        }
        if let Some(value) = overlay.search_format {
            self.search.format = value;
        }
        if let Some(archive) = &overlay.stats_archive {
            self.stats.archive = Some(archive.clone());
        }
        if let Some(bind) = &overlay.serve_bind {
            self.serve.bind = bind.clone();
        }
        if let Some(dir) = &overlay.bench_zstd_dir {
            self.bench_zstd.dir = dir.clone();
        }
        if let Some(jsonl) = &overlay.bench_zstd_jsonl {
            self.bench_zstd.jsonl = jsonl.clone();
        }
        if let Some(report) = &overlay.bench_zstd_report {
            self.bench_zstd.report = report.clone();
        }
        if let Some(re_ingest) = overlay.bench_zstd_re_ingest {
            self.bench_zstd.re_ingest = re_ingest;
        }
    }

    /// Extra `skip_directories` plus system trash when [`ScanConfig::skip_system_trash`].
    ///
    /// This does not apply ingest's other skip names (`.git`, `sandbox-blocked-dir*`).
    /// Home scan calls this. `~/.agents/trash` is skipped only when listed.
    ///
    /// `xdg_data_home` is `$XDG_DATA_HOME` when set. `None` uses `home/.local/share`.
    pub fn skips_directory(&self, path: &Path, home: &Path, xdg_data_home: Option<&Path>) -> bool {
        if self.scan.skip_system_trash && is_system_trash_path(path, home, xdg_data_home) {
            return true;
        }
        self.scan
            .skip_directories
            .iter()
            .any(|skip| path_matches_skip(path, skip))
    }
}

/// Default config file: `MEMEX_CONFIG`, else `$XDG_CONFIG_HOME/majestic/memex.toml`.
///
/// Uses [`directories::BaseDirs`] when `MEMEX_CONFIG` is unset so the XDG
/// config directory is the real base directory (not `~/majestic`).
pub fn config_file_path() -> PathBuf {
    if let Some(path) = std::env::var_os("MEMEX_CONFIG").filter(|path| !path.is_empty()) {
        let path = PathBuf::from(path);
        return match std::env::var_os("HOME").filter(|home| !home.is_empty()) {
            Some(home) => expand_tilde(&path, Path::new(&home)),
            None => path,
        };
    }
    let config_home = directories::BaseDirs::new()
        .map(|dirs| dirs.config_dir().to_path_buf())
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
                .unwrap_or_else(|| PathBuf::from(".config"))
        });
    config_home.join("majestic").join("memex.toml")
}

/// Resolve the config file from explicit env values (tests pass fake HOME / XDG).
pub fn config_file_path_from(
    memex_config: Option<&OsStr>,
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> PathBuf {
    if let Some(path) = memex_config.filter(|path| !path.is_empty()) {
        let path = PathBuf::from(path);
        return match home.filter(|home| !home.is_empty()) {
            Some(home) => expand_tilde(&path, Path::new(home)),
            None => path,
        };
    }
    let config_home = match xdg_config_home.filter(|path| !path.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => match home.filter(|home| !home.is_empty()) {
            Some(home) => PathBuf::from(home).join(".config"),
            None => PathBuf::from(".config"),
        },
    };
    config_home.join("majestic").join("memex.toml")
}

/// Expand `~` and `~/...` to `home`. Other paths are unchanged. No `~user`.
pub fn expand_tilde(path: &Path, home: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    if text == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home.join(rest);
    }
    path.to_path_buf()
}

fn is_system_trash_path(path: &Path, home: &Path, xdg_data_home: Option<&Path>) -> bool {
    let data_home = xdg_data_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".local").join("share"));
    let xdg_trash = data_home.join("Trash");
    if path == xdg_trash || path.starts_with(&xdg_trash) {
        return true;
    }
    let user_trash = home.join(".Trash");
    if path == user_trash || path.starts_with(&user_trash) {
        return true;
    }
    path.components().any(|component| match component {
        Component::Normal(name) => {
            let name = name.to_string_lossy();
            name == ".Trash" || name.starts_with(".Trash-")
        }
        _ => false,
    })
}

fn path_matches_skip(path: &Path, skip: &Path) -> bool {
    if skip.as_os_str().is_empty() {
        return false;
    }
    if skip.is_absolute() {
        return path == skip || path.starts_with(skip);
    }
    let skip_names: Vec<&OsStr> = skip.iter().collect();
    let path_names: Vec<&OsStr> = path.iter().collect();
    if skip_names.is_empty() || path_names.len() < skip_names.len() {
        return false;
    }
    path_names
        .windows(skip_names.len())
        .any(|window| window == skip_names.as_slice())
}
