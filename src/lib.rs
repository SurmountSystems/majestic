//! A personal memex with agentic support for lossless ingest, efficient encoding, and ripgrep-style search of account data exports.
//!
//! Crate **majestic**, command **memex**, on-disk format **majestic v1**.
//! Magic is ASCII `MAJESTIC` plus byte `0x01`. The crate lives at `~/majestic`.
//! The data directory is `memex_dir` (default `$HOME/memex`).
//! Optional settings live at `$XDG_CONFIG_HOME/majestic/memex.toml` (usually
//! `~/.config/majestic/memex.toml`). A missing file uses crate defaults. See
//! [`config`]. Number 0012 is Majestic Memex and is specified. This crate is
//! still a proof of concept. It is not that numbered specification. See the
//! crate README for every command.
//!
//! Library entry points: [`scoped_archive_path`], [`list_memex_archives`],
//! [`default_memex_dir`], [`Error`], [`call_local`], [`SearchFlags`],
//! [`search_with`], [`Config`], [`wire`].
//!
//! MCP, ACP, and HTTP `/mcp` accept JSON (default, for humans) and
//! [TOON](https://github.com/toon-format/spec) (accessed: 2026-08-27) for
//! language-model tools. See [`wire`].
//!
//! # Search patterns
//!
//! Search uses mmap text, grep-searcher, and grep-pcre2 only. Open maps each
//! `.majestic` file with `PROT_READ` and `MAP_SHARED`. That is a virtual map,
//! not a heap copy of the file size. RSS is pages PCRE2 has faulted, not the
//! file size at `mmap()`. Patterns are PCRE2 (the same engine as `rg -P`).
//! There is no PCRE1 and no rust-regex fallback. `-i` is case insensitive.
//! `-w` is whole word. `-F` is a phrase or literal. `|` is OR. AND any order
//! uses lookaheads on one packed span (one title or one message), not the
//! whole archive blob.
//!
//! | Want | Pattern |
//! | --- | --- |
//! | OR | `lizard OR catfooding` or `lizard\|catfooding` or `/lizard\|catfooding/` |
//! | AND, any order | `lizard AND the` or `(?=.*lizard)(?=.*the)` inside `/.../` |
//! | Phrase | `"hello world"` or `-F 'hello world'` |
//! | Case insensitive | `-i` or `/Catfooding/i` |
//! | Whole word | `-w` |
//! | Regex | `/<regex>/` flags: `i` case insensitive, `g` all matches (already unique by message), `m` multiline, `s` dotall, `x` extended |
//!
//! A pattern that is not slash-wrapped is a human query. Bare words join with
//! implicit AND. AND requires both words in the same title or the same
//! message. CLI equivalents:
//!
//! ```text
//! memex search 'lizard OR catfooding'
//! memex search 'lizard AND the'
//! memex search '"hello world"'
//! memex search '/Catfooding/i'
//! memex search -F 'hello world'
//! memex search -i catfooding
//! memex search -w food
//! memex search --format json 'lizard AND the'
//! memex search --format toon 'lizard AND the'
//! ```

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

pub mod acp;
pub mod archive;
pub mod bench_zstd;
pub mod config;
pub mod hash;
pub mod ingest;
mod ingest_characters;
mod ingest_chatgpt;
mod ingest_facebook;
mod ingest_sqlite;
mod ingest_story_cards;
mod ingest_telegram;
mod ingest_x;
pub mod logging;
pub mod mcp;
pub mod query;
pub mod rpc;
pub mod schema;
pub mod serve;
pub mod trie;
pub mod wire;
pub(crate) mod zip;
pub mod zstd_file;

/// Service folder inferred from an official Grok account export dump.
pub const GROK_SERVICE: &str = "agents/grok";
/// grok-oss session JSONL and `session_docs` sqlite.
pub const GROK_OSS_SERVICE: &str = "agents/grok-oss";
/// Agent report markdown under `.agents/reports`.
pub const REPORTS_SERVICE: &str = "agents/reports";
/// Obsidian vault (directory that contains `.obsidian/`).
pub const OBSIDIAN_SERVICE: &str = "notes/obsidian";
/// Markdown tree with no `.obsidian/` directory.
pub const MARKDOWN_SERVICE: &str = "notes/markdown";
/// Telegram Desktop JSON export (`result.json`).
pub const TELEGRAM_SERVICE: &str = "social/telegram";
/// ChatGPT account export (`conversations-*.json` plus `user.json`).
pub const CHATGPT_SERVICE: &str = "agents/chatgpt";
/// Meta Download Your Information (Facebook and Instagram activity folders).
pub const META_SERVICE: &str = "social/meta";
/// X account archive (`data/account.js` plus `data/tweet.js` or `data/tweets.js`).
pub const X_SERVICE: &str = "social/x";

/// Process `$HOME`. Empty or unset is [`Error::HomeUnset`].
pub fn home_dir() -> Result<PathBuf, Error> {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => Ok(PathBuf::from(home)),
        _ => Err(Error::HomeUnset),
    }
}

/// Default data directory: `home/memex`.
pub fn default_memex_dir(home: &Path) -> PathBuf {
    home.join("memex")
}

/// Scoped mmap archive under `memex_dir/{service}/`.
///
/// `memex_dir` is the archive directory (`memex_dir` in memex.toml, default
/// `$HOME/memex`). Do not pass `$HOME` and expect another `memex` folder.
/// `service` is a folder and may contain slashes (`agents/grok`, `social/x`).
/// `account` is the username file stem (Grok export `user.xUsername`, or `--account`).
/// New files are always `{account}.majestic`. Leftover `{account}.archive` is still
/// listed when there is no sibling same-stem `.majestic`.
/// Empty service or account is an error. `..` in either is an error (path escape).
pub fn scoped_archive_path(
    memex_dir: &Path,
    service: &str,
    account: &str,
) -> Result<PathBuf, Error> {
    reject_scope_part("service", service, "a folder such as agents/grok")?;
    reject_scope_part("account", account, "a username file stem such as personal")?;
    Ok(memex_dir.join(service).join(scoped_account_file(account)))
}

/// File name for an account under a service folder. Always `{account}.majestic`.
fn scoped_account_file(account: &str) -> String {
    format!("{account}.majestic")
}

/// Recursively list mmap archives under `memex_dir`.
///
/// `memex_dir` is the archive directory (`memex_dir` in memex.toml, default
/// `$HOME/memex`). Do not treat a nested `memex` folder as the data directory.
/// Collects `*.majestic` (never `*.zst`) in the scoped layout
/// (`{memex_dir}/{service}/{account}.majestic`). Also collects leftover
/// `*.archive` when there is no sibling `same-stem.majestic`. Skips `README` /
/// `README.md` and directories. Missing `memex_dir` is an empty list.
pub fn list_memex_archives(memex_dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut out = Vec::new();
    collect_archive_files(memex_dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_archive_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            crate::logging::log_io_on_walk(dir, &err);
            return Ok(());
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                crate::logging::log_io_on_walk(dir, &err);
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                crate::logging::log_io_on_walk(&entry.path(), &err);
                continue;
            }
        };
        let name = entry.file_name();
        if is_readme_name(&name) {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            // Nested extra data directory, not a service folder.
            if name == "memex" {
                continue;
            }
            collect_archive_files(&path, out)?;
            continue;
        }
        if file_type.is_file() && is_listed_archive_path(&path, &name) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_readme_name(name: &OsStr) -> bool {
    name.eq_ignore_ascii_case("README") || name.eq_ignore_ascii_case("README.md")
}

fn is_listed_archive_path(path: &Path, name: &OsStr) -> bool {
    let Some(ext) = Path::new(name).extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    if ext.eq_ignore_ascii_case("zst") {
        return false;
    }
    if ext.eq_ignore_ascii_case("majestic") {
        return true;
    }
    if ext.eq_ignore_ascii_case("archive") {
        return !path.with_extension("majestic").is_file();
    }
    false
}

fn reject_scope_part(kind: &str, value: &str, example: &str) -> Result<(), Error> {
    if value.is_empty() {
        return Err(Error::InvalidScope(format!(
            "{kind} is empty; pass {example}"
        )));
    }
    for component in Path::new(value).components() {
        match component {
            Component::ParentDir => {
                return Err(Error::InvalidScope(format!("{kind} must not contain '..'")));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::InvalidScope(format!(
                    "{kind} must not be an absolute path"
                )));
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

pub use archive::{
    Archive, ArchiveRoot, ArchiveStats, AssetEntry, ConversationRecord, ExportManifest,
    FIELD_MESSAGE, FIELD_SUMMARY, FIELD_TITLE, MAGIC, RECORD_CONVERSATION, RECORD_RESPONSE,
    TaggedAuth, TaggedBilling, TaggedJson, TextSpan, VERSION,
};
pub use config::{CliOverlay, Config, config_file_path, config_file_path_from, expand_tilde};
pub use ingest::{
    HOME_SCAN_MAX_DEPTH, InferredScope, IngestReport, infer_grok_export_scope, infer_ingest_scope,
    ingest, ingest_from_flags, ingest_from_flags_with_config, ingest_home, ingest_home_with_config,
    ingest_with_config, resolve_ingest_archive, resolve_ingest_archive_in,
};
pub use query::{CompiledQuery, QueryKind, compile_query, escape_pcre2};
pub use rpc::{LOCAL_FUNCTION_METHODS, RpcContext, call_local};
pub use schema::{
    AuthFile, BackendExport, BillingFile, Conversation, ConversationItem, ExtraMap, JsonAtom,
    Response, ResponseItem, Timestamp,
};
pub use trie::{
    DEFAULT_SEARCH_MAX_COUNT, SearchExec, SearchFlags, SearchFormat, SearchGroup, SearchHit,
    SearchOccurrence, SearchOrigin, SearchSnippetGroup, SearchStatus, SearchStatusSink,
    group_snippet_occurrences, search, search_all_archives, search_all_archives_with,
    search_all_archives_with_status, search_default, search_default_exec,
    search_default_exec_with_status, search_default_with, search_query, search_with,
    write_default_search, write_search, write_search_all, write_search_all_exec,
    write_search_all_with, write_search_exec, write_search_with,
};
pub use wire::{WireFormat, encode_rpc, parse_rpc_value};

/// Product errors for ingest, archive open, search, and RPC.
///
/// Display text is the CLI and MCP message. Variants do not carry auth key values.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid majestic magic")]
    InvalidMagic,
    #[error("unsupported majestic version {0}")]
    UnsupportedVersion(u32),
    #[error("archive is truncated or offsets are invalid")]
    InvalidArchive,
    #[error("archive bytecheck failed: {0}")]
    Bytecheck(String),
    #[error("tries region is missing; re-ingest the archive")]
    TriesMissing,
    #[error("invalid tries region")]
    InvalidTries,
    #[error("search pattern is empty")]
    EmptyPattern,
    #[error("invalid search pattern: {0}")]
    InvalidPattern(String),
    #[error("fst: {0}")]
    Fst(String),
    #[error("HOME is not set; pass -o or an explicit archive path")]
    HomeUnset,
    #[error("pass --service and --account, or an explicit archive path (-o)")]
    ArchiveUnspecified,
    #[error("no archives found under {0}")]
    NoArchives(PathBuf),
    #[error("this input is not an official Grok export; pass --service and --account, or -o")]
    NotGrokExport,
    #[error("the Grok export auth file has no user.xUsername; pass --account")]
    MissingXUsername,
    #[error("inputs infer different accounts; pass --account")]
    AccountDisagree,
    #[error("{0}")]
    InvalidScope(String),
    #[error("{0}")]
    InvalidParams(String),
    #[error("unknown RPC method {0}")]
    UnknownMethod(String),
    #[error("{0}")]
    Ingest(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    pub(crate) fn ingest(message: impl Into<String>) -> Self {
        Self::Ingest(message.into())
    }

    pub(crate) fn bytecheck(error: impl ToString) -> Self {
        Self::Bytecheck(error.to_string())
    }
}

impl From<fst::Error> for Error {
    fn from(error: fst::Error) -> Self {
        Self::Fst(error.to_string())
    }
}

/// rkyv high-level serialize. Maps rancor `Error` to [`Error::Ingest`].
///
/// rkyv 0.8 `RelPtr::emplace` still uses rancor `Panic` internally. This crate
/// enables `pointer_width_64` so in-memory offsets fit in that pointer.
/// Do not `unwrap` / `expect` a rancor panic.
pub(crate) fn rkyv_to_bytes<T>(value: &T) -> Result<rkyv::util::AlignedVec, Error>
where
    T: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                rkyv::rancor::Error,
            >,
        >,
{
    rkyv::to_bytes::<rkyv::rancor::Error>(value).map_err(|error| {
        Error::ingest(format!(
            "rkyv serialize failed: {error}. A relative pointer or length did not fit in the archive."
        ))
    })
}
