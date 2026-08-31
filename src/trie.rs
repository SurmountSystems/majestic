//! Compact on-disk tries (FST maps) plus packed posting lists.
//!
//! The tries region is a slice of the same `.majestic` file and is packed at
//! ingest. Search runs PCRE2 (grep-pcre2, same engine as `rg -P`) on each
//! packed span of the mmap UTF-8 text (one title or one message). AND any
//! order uses PCRE2 lookaheads on that span. It does not search the whole
//! blob as one haystack. Search does not use rust-regex or PCRE1. It does
//! not parse JSON and does not spawn the `rg` binary.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use fst::MapBuilder;
use grep_matcher::Matcher;
use grep_pcre2::{RegexMatcher as Pcre2Matcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkMatch};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Error;
use crate::archive::{
    Archive, FIELD_MESSAGE, FIELD_SUMMARY, FIELD_TITLE, TextSpan, slice_at, u32_fit_usize,
    u32_from, u64_from, write_u64,
};
use crate::query::{CompiledQuery, compile_query};
use crate::wire::{WireFormat, encode_rpc};

/// Inner tries-region magic. Eight bytes, no NUL.
pub const TRIES_MAGIC: [u8; 8] = *b"MAJTRIES";
/// Tries-region layout version. Archive file version stays 1.
pub const TRIES_VERSION: u32 = 1;
/// Inner header size, including magic and version.
pub const TRIES_HEADER_LEN: usize = 80;
const HIT_LEN: usize = 8;
const POSTING_DIR_ENTRY_LEN: usize = 16;
const SNIPPET_RADIUS: usize = 56;
/// Snippet groups printed. `0` means no print cap.
///
/// 100 is a print default, not a measured quota. It is meant to stop a
/// terminal flood (a live search printed 250830 duplicate lines on
/// 2026-08-28). Override with `-m` / `--max-count` or `search.max_count`.
pub const DEFAULT_SEARCH_MAX_COUNT: usize = 100;
/// Line-count floor from that 250830-hit flood. Default print must stay
/// below this. Not a product quota.
#[cfg(test)]
const SEARCH_PRINT_FLOOD_LINES: usize = 250_000;

/// One search hit. Snippet is a short window of mmap text, not the whole blob.
///
/// Unique hits key on archive (the search group), conversation id, field, and
/// [`Self::span_text`] (the entire packed message or title body). They do not
/// key on the snippet window and they do not key on [`Self::span_off`]. Many
/// PCRE2 matches in one packed message stay one hit. Duplicate packed copies
/// of the same body (overlapping dumps, two spans, identical bytes) stay one
/// hit. If conversation id is missing, unique on field plus that full span
/// text inside the archive.
///
/// Display and RPC then group those unique hits by [`Self::span_text`]. The
/// same packed body in two conversation ids (or two archives) is one snippet
/// and two occurrence rows. It is not two copies of the paragraph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub conversation_id: Option<String>,
    pub field: &'static str,
    pub snippet: String,
    /// Packed UTF-8 span start in the archive text blob.
    pub span_off: u64,
    /// Entire packed span body. Unique identity, not the snippet window.
    pub span_text: String,
}

/// One place a grouped snippet appeared.
///
/// Archive path, conversation id, and field. No timestamp is invented; this
/// crate does not attach times to search hits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchOccurrence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive: Option<PathBuf>,
    pub conversation_id: Option<String>,
    pub field: &'static str,
}

impl fmt::Display for SearchOccurrence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let id = self.conversation_id.as_deref().unwrap_or("-");
        match &self.archive {
            Some(archive) => write!(
                f,
                "archive {}  conversation {id}  field {}",
                archive.display(),
                self.field
            ),
            None => write!(f, "conversation {id}  field {}", self.field),
        }
    }
}

/// One printed or RPC snippet with every place it appeared.
///
/// Grouping key is the full packed span text, not the snippet window, so a
/// short window cannot split one body into two groups. [`Self::snippet`] is
/// the window from the first unique hit in the group. [`Self::field`] is that
/// first hit's field. Each occurrence still carries its own field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchSnippetGroup {
    pub snippet: String,
    pub field: &'static str,
    pub occurrences: Vec<SearchOccurrence>,
}

/// Flags for `memex search`. Patterns compile with [`crate::compile_query`].
///
/// `-i` / [`Self::ignore_case`] is case insensitive. `-w` /
/// [`Self::word_regexp`] is whole word. `-F` / [`Self::fixed_strings`] is a
/// phrase or literal and skips human / slash compile. Human `AND` compiles to
/// lookaheads on one packed span (one title or one message). `|` and `OR`
/// are OR.
///
/// ```
/// use majestic::SearchFlags;
/// let flags = SearchFlags {
///     ignore_case: true,
///     fixed_strings: false,
///     word_regexp: false,
/// };
/// assert!(flags.ignore_case);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchFlags {
    /// Case insensitive (`-i` / `--ignore-case`). Unicode.
    pub ignore_case: bool,
    /// Phrase or literal (`-F` / `--fixed-strings`).
    pub fixed_strings: bool,
    /// Whole word (`-w` / `--word-regexp`).
    pub word_regexp: bool,
}

/// Whether a default-search section is a memex archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchOrigin {
    /// `*.majestic` under `memex_dir`, plus leftover `*.archive` with no sibling `.majestic`.
    Archive,
}

/// One labeled section in a default (systemwide) search report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchGroup {
    /// Archive file under `memex_dir`.
    pub path: PathBuf,
    /// How to label `path` in the report (relative to `memex/`).
    pub origin: SearchOrigin,
    /// Unique hits for this archive, grouped by conversation id.
    pub hits: Vec<SearchHit>,
    /// Extra PCRE2 matches in a packed message already counted as a unique hit.
    pub duplicate_omitted: usize,
}

/// How `memex search` writes the report. Status stays on stderr.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchFormat {
    /// Grouped CLI snippets and occurrence rows. Default.
    #[default]
    Human,
    /// JSON object `{ "hits": [ ... occurrence objects ... ] }`.
    Json,
    /// TOON encoding of the same hit objects.
    Toon,
}

impl SearchFormat {
    /// Parse `human`, `json`, or `toon` (case-insensitive).
    pub fn parse_name(name: &str) -> Result<Self, Error> {
        match name.trim().to_ascii_lowercase().as_str() {
            "human" => Ok(Self::Human),
            "json" => Ok(Self::Json),
            "toon" => Ok(Self::Toon),
            other => Err(Error::InvalidParams(format!(
                "unknown search format {other}; use human, json, or toon"
            ))),
        }
    }

    fn wire(self) -> Option<WireFormat> {
        match self {
            Self::Human => None,
            Self::Json => Some(WireFormat::Json),
            Self::Toon => Some(WireFormat::Toon),
        }
    }
}

/// Matcher flags plus print cap and report format.
///
/// [`Self::flags`] compile the PCRE2 matcher. [`Self::max_count`] is snippet
/// groups printed after uniqueness and packed-body grouping (`0` means no cap).
/// [`Self::format`] is stdout (`human` default). Status goes to stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchExec {
    pub flags: SearchFlags,
    pub max_count: usize,
    pub format: SearchFormat,
}

impl Default for SearchExec {
    fn default() -> Self {
        Self {
            flags: SearchFlags::default(),
            max_count: DEFAULT_SEARCH_MAX_COUNT,
            format: SearchFormat::Human,
        }
    }
}

impl From<SearchFlags> for SearchExec {
    fn from(flags: SearchFlags) -> Self {
        Self {
            flags,
            ..Self::default()
        }
    }
}

/// One compact search-status line (stderr / tracing INFO).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchStatus {
    /// After archives are mapped, before PCRE2 workers start.
    Starting { archives: usize },
    /// After a worker finishes one already-mapped archive.
    Archive {
        completed: usize,
        total: usize,
        archive: PathBuf,
        unique_hits: usize,
    },
}

/// Records [`SearchStatus`] for tests. Tracing INFO still goes to stderr.
#[derive(Clone, Default)]
pub struct SearchStatusSink {
    events: Arc<Mutex<Vec<SearchStatus>>>,
}

impl SearchStatusSink {
    /// Empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of recorded events, in emit order.
    pub fn events(&self) -> Vec<SearchStatus> {
        self.events
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    fn record(&self, event: SearchStatus) {
        if let Ok(mut guard) = self.events.lock() {
            guard.push(event);
        }
    }
}

fn emit_status(sink: Option<&SearchStatusSink>, event: SearchStatus) {
    match &event {
        SearchStatus::Starting { archives } => {
            tracing::info!(archives, "searching {archives} archives");
        }
        SearchStatus::Archive {
            completed,
            total,
            archive,
            unique_hits,
        } => {
            tracing::info!(
                unique = unique_hits,
                "searching {completed}/{total} {}",
                archive.display()
            );
        }
    }
    if let Some(sink) = sink {
        sink.record(event);
    }
}

struct UniqueHits {
    hits: Vec<SearchHit>,
    duplicate_omitted: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Hit {
    span_index: u32,
    byte_off: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TextMatch {
    span_index: u32,
    byte_off: u32,
    match_len: u32,
}

struct OffsetSink<'a> {
    matcher: &'a Pcre2Matcher,
    matches: Vec<(u64, usize)>,
}

struct TriesHeader {
    cs_fst_off: usize,
    cs_fst_len: usize,
    ci_fst_off: usize,
    ci_fst_len: usize,
    cs_post_off: usize,
    cs_post_len: usize,
    ci_post_off: usize,
    ci_post_len: usize,
}

/// Build the tries region from the UTF-8 text blob and span table.
pub(crate) fn pack_tries(text: &str, spans: &[TextSpan]) -> Result<Vec<u8>, Error> {
    let mut case_sensitive: BTreeMap<String, Vec<Hit>> = BTreeMap::new();
    let mut case_insensitive: BTreeMap<String, Vec<Hit>> = BTreeMap::new();

    for (span_index, span) in spans.iter().enumerate() {
        let span_index = u32_fit_usize(span_index, "text span index")?;
        let span_text = span_str(text, span).map_err(|_| Error::ingest("span is outside text"))?;
        for_each_token(span_text, |rel, token| {
            if token.is_empty() {
                return Ok::<(), Error>(());
            }
            let byte_off = u32_fit_usize(rel, "token byte offset in a text span")?;
            let hit = Hit {
                span_index,
                byte_off,
            };
            case_sensitive
                .entry(token.to_owned())
                .or_default()
                .push(hit);
            let folded = token.to_lowercase();
            if !folded.is_empty() {
                case_insensitive.entry(folded).or_default().push(hit);
            }
            Ok::<(), Error>(())
        })?;
    }

    let (cs_fst, cs_post) = finish_index(case_sensitive)?;
    let (ci_fst, ci_post) = finish_index(case_insensitive)?;

    let mut out = vec![0u8; TRIES_HEADER_LEN];
    let (cs_fst_off, cs_fst_len) = append_aligned(&mut out, &cs_fst);
    let (ci_fst_off, ci_fst_len) = append_aligned(&mut out, &ci_fst);
    let (cs_post_off, cs_post_len) = append_aligned(&mut out, &cs_post);
    let (ci_post_off, ci_post_len) = append_aligned(&mut out, &ci_post);

    out[0..8].copy_from_slice(&TRIES_MAGIC);
    out[8..12].copy_from_slice(&TRIES_VERSION.to_le_bytes());
    out[12..16].copy_from_slice(&(TRIES_HEADER_LEN as u32).to_le_bytes());
    write_u64(&mut out, 16, cs_fst_off);
    write_u64(&mut out, 24, cs_fst_len);
    write_u64(&mut out, 32, ci_fst_off);
    write_u64(&mut out, 40, ci_fst_len);
    write_u64(&mut out, 48, cs_post_off);
    write_u64(&mut out, 56, cs_post_len);
    write_u64(&mut out, 64, ci_post_off);
    write_u64(&mut out, 72, ci_post_len);
    Ok(out)
}

pub(crate) fn validate_tries_header(tries: &[u8]) -> Result<(), Error> {
    let _ = TriesHeader::parse(tries)?;
    Ok(())
}

/// Search packed spans of the mmap UTF-8 text with PCRE2 (same engine as `rg -P`).
///
/// This entry point takes a raw PCRE2 pattern. CLI and MCP compile human
/// queries first (`lizard AND the` becomes lookaheads). Each lookahead AND
/// runs on one packed span (one title or one message), not the whole blob.
/// `ignore_case` is case insensitive (`-i`). A match may sit inside a stored
/// word unless `word_regexp` (whole word, `-w`).
pub fn search(
    archive: &Archive,
    pattern: &str,
    ignore_case: bool,
) -> Result<Vec<SearchHit>, Error> {
    search_with(
        archive,
        pattern,
        SearchFlags {
            ignore_case,
            ..SearchFlags::default()
        },
    )
}

/// Search the mmap UTF-8 text blob with [`SearchFlags`]. Pattern is PCRE2.
///
/// `-i` is case insensitive. `-F` is a phrase or literal. `-w` is whole word.
pub fn search_with(
    archive: &Archive,
    pattern: &str,
    flags: SearchFlags,
) -> Result<Vec<SearchHit>, Error> {
    search_compiled(archive, &CompiledQuery::pcre2(pattern, flags))
}

/// Compile a human or `/regex/flags` query, then search the mmap text.
pub fn search_query(
    archive: &Archive,
    pattern: &str,
    flags: SearchFlags,
) -> Result<Vec<SearchHit>, Error> {
    search_compiled(archive, &compile_query(pattern, flags)?)
}

fn search_compiled(archive: &Archive, query: &CompiledQuery) -> Result<Vec<SearchHit>, Error> {
    let matcher = compile_matcher(query)?;
    Ok(search_blob(archive, &matcher)?.hits)
}

/// Open `path`, search, print unique hits grouped by packed body.
pub fn write_search(
    path: impl AsRef<Path>,
    pattern: &str,
    ignore_case: bool,
    out: impl Write,
) -> Result<(), Error> {
    write_search_with(
        path,
        pattern,
        SearchFlags {
            ignore_case,
            ..SearchFlags::default()
        },
        out,
    )
}

/// Open `path` and search with [`SearchFlags`]. Pattern is PCRE2.
///
/// `-i` is case insensitive. `-F` is a phrase or literal. `-w` is whole word.
/// Print cap and grouping use [`SearchExec::default`].
pub fn write_search_with(
    path: impl AsRef<Path>,
    pattern: &str,
    flags: SearchFlags,
    out: impl Write,
) -> Result<(), Error> {
    write_search_exec(path, pattern, SearchExec::from(flags), out)
}

/// Open `path` and print unique hits grouped by packed body.
pub fn write_search_exec(
    path: impl AsRef<Path>,
    pattern: &str,
    exec: SearchExec,
    mut out: impl Write,
) -> Result<(), Error> {
    let path = path.as_ref();
    let found = search_one_archive_pattern(path, pattern, exec.flags, None)?;
    let printed =
        write_one_archive_report(&mut out, path, &found.hits, exec, found.duplicate_omitted)?;
    tracing::info!(
        archive = %path.display(),
        unique = found.hits.len(),
        printed,
        duplicate_omitted = found.duplicate_omitted,
        "search finished"
    );
    Ok(())
}

/// Search every listed mmap archive under `memex_dir`.
///
/// Each search reads [`Archive::text`] (a mapped slice) and runs PCRE2 on
/// each packed span in that slice. It does not copy the text blob into a
/// `Vec`. A systemwide search lists paths, maps every listed archive and
/// holds those maps, compiles the PCRE2 pattern once, then searches
/// already-mapped spans with `min(file count, available parallelism)`
/// workers. That is parallelism on mapped slices (one worker per archive).
/// It does not `par_iter` the path list as the architecture (open and search
/// each path as a job). There is no four-map cap. Search does not call
/// `MADV_DONTNEED` after each archive while the user is still searching.
/// Unreadable or corrupt files are skipped so one junk file does not abort
/// the rest. Permission denied is an error on the terminal. Other unreadable
/// files warn in the journal. An empty memex directory is
/// [`Error::NoArchives`].
pub fn search_all_archives(
    memex_dir: &Path,
    pattern: &str,
    ignore_case: bool,
) -> Result<Vec<(PathBuf, Vec<SearchHit>)>, Error> {
    search_all_archives_with(
        memex_dir,
        pattern,
        SearchFlags {
            ignore_case,
            ..SearchFlags::default()
        },
    )
}

/// Search every archive under `memex_dir` with [`SearchFlags`].
pub fn search_all_archives_with(
    memex_dir: &Path,
    pattern: &str,
    flags: SearchFlags,
) -> Result<Vec<(PathBuf, Vec<SearchHit>)>, Error> {
    Ok(search_listed_archives(memex_dir, pattern, flags, None)?
        .into_iter()
        .map(|(path, found)| (path, found.hits))
        .collect())
}

/// Systemwide default search: every mmap archive under `memex_dir`.
///
/// Home live-scan is ingest's job. `--service`/`--account` or an explicit
/// archive path still search one file.
pub fn search_default(
    memex_dir: &Path,
    pattern: &str,
    ignore_case: bool,
) -> Result<Vec<SearchGroup>, Error> {
    search_default_with(
        memex_dir,
        pattern,
        SearchFlags {
            ignore_case,
            ..SearchFlags::default()
        },
    )
}

/// Systemwide default search with [`SearchFlags`]. Pattern is PCRE2.
pub fn search_default_with(
    memex_dir: &Path,
    pattern: &str,
    flags: SearchFlags,
) -> Result<Vec<SearchGroup>, Error> {
    search_default_exec(memex_dir, pattern, SearchExec::from(flags))
}

/// Systemwide default search with print cap.
pub fn search_default_exec(
    memex_dir: &Path,
    pattern: &str,
    exec: SearchExec,
) -> Result<Vec<SearchGroup>, Error> {
    search_default_exec_with_status(memex_dir, pattern, exec, None)
}

/// Systemwide default search that records compact status events.
pub fn search_default_exec_with_status(
    memex_dir: &Path,
    pattern: &str,
    exec: SearchExec,
    status: Option<&SearchStatusSink>,
) -> Result<Vec<SearchGroup>, Error> {
    Ok(
        search_listed_archives(memex_dir, pattern, exec.flags, status)?
            .into_iter()
            .map(|(path, found)| SearchGroup {
                path,
                origin: SearchOrigin::Archive,
                hits: found.hits,
                duplicate_omitted: found.duplicate_omitted,
            })
            .collect(),
    )
}

/// Search every archive and record compact status (for tests, no tty).
pub fn search_all_archives_with_status(
    memex_dir: &Path,
    pattern: &str,
    flags: SearchFlags,
    status: &SearchStatusSink,
) -> Result<Vec<(PathBuf, Vec<SearchHit>)>, Error> {
    Ok(
        search_listed_archives(memex_dir, pattern, flags, Some(status))?
            .into_iter()
            .map(|(path, found)| (path, found.hits))
            .collect(),
    )
}

fn search_listed_archives(
    memex_dir: &Path,
    pattern: &str,
    flags: SearchFlags,
    status: Option<&SearchStatusSink>,
) -> Result<Vec<(PathBuf, UniqueHits)>, Error> {
    let paths = crate::list_memex_archives(memex_dir)?;
    if paths.is_empty() {
        return Err(Error::NoArchives(memex_dir.to_path_buf()));
    }
    // Compile once so a bad pattern fails before any archive is mapped.
    let query = compile_query(pattern, flags)?;
    let matcher = compile_matcher(&query)?;
    let mut opened = Vec::with_capacity(paths.len());
    for path in paths {
        match Archive::open(&path) {
            Ok(archive) => opened.push((path, archive)),
            Err(error) => {
                crate::logging::log_unreadable_archive(&path, &error);
            }
        }
    }
    emit_status(
        status,
        SearchStatus::Starting {
            archives: opened.len(),
        },
    );
    let mut groups = search_opened_archives(memex_dir, &opened, &matcher, status);
    groups.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(groups)
}

fn search_opened_archives(
    memex_dir: &Path,
    opened: &[(PathBuf, Archive)],
    matcher: &Pcre2Matcher,
    status: Option<&SearchStatusSink>,
) -> Vec<(PathBuf, UniqueHits)> {
    let total = opened.len();
    let workers = search_worker_count(total);
    let completed = AtomicUsize::new(0);
    let unique_hits = AtomicUsize::new(0);
    let search_one = |item: &(PathBuf, Archive)| {
        let (path, archive) = item;
        // Clone so each PCRE2 worker has its own match-data pool.
        let matcher = matcher.clone();
        let result = search_blob(archive, &matcher);
        match result {
            Ok(found) => {
                let running =
                    unique_hits.fetch_add(found.hits.len(), Ordering::Relaxed) + found.hits.len();
                let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                emit_status(
                    status,
                    SearchStatus::Archive {
                        completed: done,
                        total,
                        archive: status_label(memex_dir, path),
                        unique_hits: running,
                    },
                );
                Some((path.clone(), found))
            }
            Err(error) => {
                let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                emit_status(
                    status,
                    SearchStatus::Archive {
                        completed: done,
                        total,
                        archive: status_label(memex_dir, path),
                        unique_hits: unique_hits.load(Ordering::Relaxed),
                    },
                );
                crate::logging::log_unreadable_archive(path, &error);
                None
            }
        }
    };
    if workers <= 1 || total <= 1 {
        return opened.iter().filter_map(search_one).collect();
    }
    match rayon::ThreadPoolBuilder::new().num_threads(workers).build() {
        Ok(pool) => pool.install(|| opened.par_iter().filter_map(search_one).collect()),
        Err(_) => opened.iter().filter_map(search_one).collect(),
    }
}

fn status_label(memex_dir: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(memex_dir)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

fn search_worker_count(nfiles: usize) -> usize {
    if nfiles == 0 {
        return 1;
    }
    let cpus = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    nfiles.min(cpus).max(1)
}

fn search_one_archive_pattern(
    path: &Path,
    pattern: &str,
    flags: SearchFlags,
    status: Option<&SearchStatusSink>,
) -> Result<UniqueHits, Error> {
    let query = compile_query(pattern, flags)?;
    let matcher = compile_matcher(&query)?;
    emit_status(status, SearchStatus::Starting { archives: 1 });
    let archive = Archive::open(path)?;
    let found = search_blob(&archive, &matcher)?;
    emit_status(
        status,
        SearchStatus::Archive {
            completed: 1,
            total: 1,
            archive: path.to_path_buf(),
            unique_hits: found.hits.len(),
        },
    );
    Ok(found)
}

fn search_blob(archive: &Archive, matcher: &Pcre2Matcher) -> Result<UniqueHits, Error> {
    let text = archive.text()?;
    let spans = load_spans(archive)?;
    let mut sink = OffsetSink {
        matcher,
        matches: Vec::new(),
    };
    let mut searcher = text_blob_searcher();
    let mut raw_hits = Vec::new();
    let mut match_count = 0usize;
    // Packed spans are adjacent in the UTF-8 blob. A blob-wide `(?=.*a)(?=.*b)`
    // is a zero-width hit at any offset from which both words exist later in
    // the archive. AND means both terms in this span's text.
    for (span_index, span) in spans.iter().enumerate() {
        let span_text = span_str(text, span)?;
        if span_text.is_empty() {
            continue;
        }
        let Ok(span_index) = u32::try_from(span_index) else {
            continue;
        };
        sink.matches.clear();
        searcher
            .search_slice(matcher, span_text.as_bytes(), &mut sink)
            .map_err(Error::from)?;
        let mut best: Option<TextMatch> = None;
        for (rel, match_len) in &sink.matches {
            let remaining = span.len.saturating_sub(*rel);
            // Lookahead AND is zero-width; still a hit when the span has text.
            if remaining == 0 {
                continue;
            }
            let Ok(byte_off) = u32::try_from(*rel) else {
                continue;
            };
            let Ok(match_len) = u32::try_from(*match_len) else {
                continue;
            };
            match_count = match_count.saturating_add(1);
            let candidate = TextMatch {
                span_index,
                byte_off,
                match_len,
            };
            // One packed span is one hit. Keep the first match in that record
            // (smallest offset; longest match if the offset ties).
            match &mut best {
                Some(prev) => {
                    if candidate.byte_off < prev.byte_off
                        || (candidate.byte_off == prev.byte_off
                            && candidate.match_len > prev.match_len)
                    {
                        *prev = candidate;
                    }
                }
                None => best = Some(candidate),
            }
        }
        if let Some(hit) = best {
            raw_hits.push(hit);
        }
    }
    raw_hits.sort_by_key(|hit| (hit.span_index, hit.byte_off));
    let hits = materialize(archive, &raw_hits)?;
    let mut found = unique_hits(hits);
    found.duplicate_omitted = match_count.saturating_sub(found.hits.len());
    Ok(found)
}

fn compile_matcher(query: &CompiledQuery) -> Result<Pcre2Matcher, Error> {
    if query.pattern.is_empty() {
        return Err(Error::EmptyPattern);
    }
    // PCRE2 always (same engine as `rg -P`). AND any order uses lookaheads
    // on one packed span, not the whole archive blob.
    // `caseless` is the grep-pcre2 builder name for case insensitive (`-i`).
    RegexMatcherBuilder::new()
        .caseless(query.flags.ignore_case)
        .fixed_strings(query.flags.fixed_strings)
        .word(query.flags.word_regexp)
        .multi_line(query.multi_line)
        .dotall(query.dotall)
        .extended(query.extended)
        .utf(true)
        .ucp(true)
        .jit_if_available(true)
        .build(&query.pattern)
        .map_err(|error| Error::InvalidPattern(error.to_string()))
}

fn text_blob_searcher() -> Searcher {
    SearcherBuilder::new()
        .line_number(false)
        .bom_sniffing(false)
        .multi_line(false)
        .build()
}

fn load_spans(archive: &Archive) -> Result<Vec<TextSpan>, Error> {
    let count = archive.span_count()?;
    let mut spans = Vec::with_capacity(count);
    for index in 0..count {
        spans.push(archive.span(index)?);
    }
    Ok(spans)
}

impl Sink for OffsetSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        let line = mat.bytes();
        let line_abs = mat.absolute_byte_offset();
        self.matcher
            .find_iter(line, |found| {
                // Lookahead AND is zero-width; still record so the line is a hit.
                let match_len = found.end().saturating_sub(found.start());
                let abs = line_abs.saturating_add(found.start() as u64);
                self.matches.push((abs, match_len));
                true
            })
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(true)
    }
}

/// Systemwide default search report.
///
/// Unique hits first (conversation, field, full packed body). Display then
/// groups identical packed bodies: the snippet prints once, then occurrence
/// rows list archive path, conversation id, and field. Two conversation ids
/// or two archives with the same body are one snippet and two occurrence
/// rows. Many PCRE2 matches in one packed message print once. Duplicate
/// packed copies of the same body in one conversation print once. Does not
/// walk `$HOME` for live exports.
pub fn write_search_all(
    memex_dir: &Path,
    pattern: &str,
    ignore_case: bool,
    out: impl Write,
) -> Result<(), Error> {
    write_search_all_with(
        memex_dir,
        pattern,
        SearchFlags {
            ignore_case,
            ..SearchFlags::default()
        },
        out,
    )
}

/// Systemwide default search with [`SearchFlags`]. Pattern is PCRE2.
pub fn write_search_all_with(
    memex_dir: &Path,
    pattern: &str,
    flags: SearchFlags,
    out: impl Write,
) -> Result<(), Error> {
    write_search_all_exec(memex_dir, pattern, SearchExec::from(flags), out)
}

/// Systemwide default search with grouping, unique hits, and a print cap.
pub fn write_search_all_exec(
    memex_dir: &Path,
    pattern: &str,
    exec: SearchExec,
    mut out: impl Write,
) -> Result<(), Error> {
    let mut groups = search_default_exec(memex_dir, pattern, exec)?;
    groups.sort_by(|left, right| {
        section_label(memex_dir, &left.path, left.origin).cmp(&section_label(
            memex_dir,
            &right.path,
            right.origin,
        ))
    });
    let mut unique = 0usize;
    let mut duplicate_omitted = 0usize;
    let mut printed_omitted = 0usize;
    let mut labeled: Vec<(PathBuf, SearchHit)> = Vec::new();
    for group in groups {
        duplicate_omitted = duplicate_omitted.saturating_add(group.duplicate_omitted);
        if group.hits.is_empty() {
            continue;
        }
        unique = unique.saturating_add(group.hits.len());
        printed_omitted = printed_omitted.saturating_add(group.duplicate_omitted);
        let label = match exec.format {
            SearchFormat::Human => section_label(memex_dir, &group.path, group.origin),
            SearchFormat::Json | SearchFormat::Toon => group.path.clone(),
        };
        for hit in group.hits {
            labeled.push((label.clone(), hit));
        }
    }
    let snippet_groups = group_snippet_occurrences(
        labeled
            .iter()
            .map(|(path, hit)| (Some(path.as_path()), hit)),
    );
    let printed = write_search_report(&mut out, &snippet_groups, exec, printed_omitted)?;
    tracing::info!(unique, printed, duplicate_omitted, "search finished");
    Ok(())
}

/// Alias of [`write_search_all`].
pub fn write_default_search(
    memex_dir: &Path,
    pattern: &str,
    ignore_case: bool,
    out: impl Write,
) -> Result<(), Error> {
    write_search_all(memex_dir, pattern, ignore_case, out)
}

fn section_label(memex_dir: &Path, path: &Path, origin: SearchOrigin) -> PathBuf {
    match origin {
        SearchOrigin::Archive => path.strip_prefix(memex_dir).unwrap_or(path).to_path_buf(),
    }
}

fn unique_hits(hits: Vec<SearchHit>) -> UniqueHits {
    let raw = hits.len();
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for hit in hits {
        // One logical message is one hit: conversation, field, full span
        // bytes. Not the snippet window and not the packed start offset.
        let key = (
            hit.conversation_id.clone(),
            hit.field,
            hit.span_text.clone(),
        );
        if seen.insert(key) {
            unique.push(hit);
        }
    }
    let unique = group_by_conversation(unique);
    UniqueHits {
        duplicate_omitted: raw.saturating_sub(unique.len()),
        hits: unique,
    }
}

/// Group unique hits for display and RPC.
///
/// Key is the entire packed span text, not the snippet window. Two unique
/// hits with the same packed body become one snippet group and two
/// occurrence rows. Different packed bodies stay two snippet groups. Does
/// not drop an occurrence.
pub fn group_snippet_occurrences<'a>(
    hits: impl IntoIterator<Item = (Option<&'a Path>, &'a SearchHit)>,
) -> Vec<SearchSnippetGroup> {
    let mut groups: Vec<SearchSnippetGroup> = Vec::new();
    let mut index_by_text: HashMap<String, usize> = HashMap::new();
    for (archive, hit) in hits {
        let occurrence = SearchOccurrence {
            archive: archive.map(Path::to_path_buf),
            conversation_id: hit.conversation_id.clone(),
            field: hit.field,
        };
        match index_by_text.entry(hit.span_text.clone()) {
            Entry::Occupied(occupied) => {
                let index = *occupied.get();
                groups[index].occurrences.push(occurrence);
            }
            Entry::Vacant(vacant) => {
                vacant.insert(groups.len());
                groups.push(SearchSnippetGroup {
                    snippet: hit.snippet.clone(),
                    field: hit.field,
                    occurrences: vec![occurrence],
                });
            }
        }
    }
    groups
}

fn group_by_conversation(hits: Vec<SearchHit>) -> Vec<SearchHit> {
    let mut order = Vec::new();
    let mut buckets: HashMap<Option<String>, Vec<SearchHit>> = HashMap::new();
    for hit in hits {
        let key = hit.conversation_id.clone();
        if !buckets.contains_key(&key) {
            order.push(key.clone());
        }
        buckets.entry(key).or_default().push(hit);
    }
    let mut out = Vec::new();
    for key in order {
        if let Some(group) = buckets.remove(&key) {
            out.extend(group);
        }
    }
    out
}

fn write_one_archive_report(
    out: &mut impl Write,
    path: &Path,
    hits: &[SearchHit],
    exec: SearchExec,
    duplicate_omitted: usize,
) -> Result<usize, Error> {
    match exec.format {
        SearchFormat::Human => {
            writeln!(out, "{}", path.display())?;
            write_hits_grouped(out, hits, exec.max_count, duplicate_omitted)
        }
        SearchFormat::Json | SearchFormat::Toon => {
            let groups = group_snippet_occurrences(hits.iter().map(|hit| (Some(path), hit)));
            write_search_report(out, &groups, exec, duplicate_omitted)
        }
    }
}

fn write_search_report(
    out: &mut impl Write,
    groups: &[SearchSnippetGroup],
    exec: SearchExec,
    duplicate_omitted: usize,
) -> Result<usize, Error> {
    match exec.format.wire() {
        None => write_snippet_groups(out, groups, exec.max_count, duplicate_omitted),
        Some(wire) => write_encoded_hits(out, groups, exec.max_count, wire),
    }
}

fn write_encoded_hits(
    out: &mut impl Write,
    groups: &[SearchSnippetGroup],
    max_count: usize,
    format: WireFormat,
) -> Result<usize, Error> {
    let cap = if max_count == 0 {
        groups.len()
    } else {
        max_count.min(groups.len())
    };
    let value = json!({ "hits": &groups[..cap] });
    let encoded = encode_rpc(&value, format).map_err(Error::InvalidParams)?;
    writeln!(out, "{encoded}")?;
    Ok(cap)
}

fn write_hits_grouped(
    out: &mut impl Write,
    hits: &[SearchHit],
    max_count: usize,
    duplicate_omitted: usize,
) -> Result<usize, Error> {
    let groups = group_snippet_occurrences(hits.iter().map(|hit| (None, hit)));
    write_snippet_groups(out, &groups, max_count, duplicate_omitted)
}

fn write_snippet_groups(
    out: &mut impl Write,
    groups: &[SearchSnippetGroup],
    max_count: usize,
    duplicate_omitted: usize,
) -> Result<usize, Error> {
    let cap = if max_count == 0 {
        groups.len()
    } else {
        max_count.min(groups.len())
    };
    for group in &groups[..cap] {
        writeln!(out, "  {}", group.field)?;
        writeln!(out, "    {}", group.snippet)?;
        writeln!(out, "    present in:")?;
        for occurrence in &group.occurrences {
            writeln!(out, "      {occurrence}")?;
        }
    }
    let not_shown = groups.len().saturating_sub(cap);
    if not_shown > 0 || duplicate_omitted > 0 {
        writeln!(
            out,
            "  ({})",
            suppressed_clause(not_shown, duplicate_omitted)
        )?;
    }
    Ok(cap)
}

fn suppressed_clause(not_shown: usize, duplicate_omitted: usize) -> String {
    match (not_shown, duplicate_omitted) {
        (0, n) => format!("{n} duplicate hits omitted"),
        (1, 0) => "1 more snippet group not shown".into(),
        (n, 0) => format!("{n} more snippet groups not shown"),
        (1, d) => format!("1 more snippet group not shown, {d} duplicate hits omitted"),
        (n, d) => format!("{n} more snippet groups not shown, {d} duplicate hits omitted"),
    }
}

impl TriesHeader {
    fn parse(tries: &[u8]) -> Result<Self, Error> {
        if tries.len() < TRIES_HEADER_LEN {
            return Err(Error::InvalidTries);
        }
        if tries[0..8] != TRIES_MAGIC {
            return Err(Error::InvalidTries);
        }
        let version = u32_from(tries, 8).map_err(|_| Error::InvalidTries)?;
        if version != TRIES_VERSION {
            return Err(Error::InvalidTries);
        }
        let header_len = u32_from(tries, 12).map_err(|_| Error::InvalidTries)? as usize;
        if header_len != TRIES_HEADER_LEN {
            return Err(Error::InvalidTries);
        }
        let header = Self {
            cs_fst_off: read_usize(tries, 16)?,
            cs_fst_len: read_usize(tries, 24)?,
            ci_fst_off: read_usize(tries, 32)?,
            ci_fst_len: read_usize(tries, 40)?,
            cs_post_off: read_usize(tries, 48)?,
            cs_post_len: read_usize(tries, 56)?,
            ci_post_off: read_usize(tries, 64)?,
            ci_post_len: read_usize(tries, 72)?,
        };
        slice_tries(tries, header.cs_fst_off, header.cs_fst_len)?;
        slice_tries(tries, header.ci_fst_off, header.ci_fst_len)?;
        slice_tries(tries, header.cs_post_off, header.cs_post_len)?;
        slice_tries(tries, header.ci_post_off, header.ci_post_len)?;
        Ok(header)
    }
}

fn slice_tries(tries: &[u8], off: usize, len: usize) -> Result<&[u8], Error> {
    slice_at(tries, off, len).map_err(|_| Error::InvalidTries)
}

fn finish_index(map: BTreeMap<String, Vec<Hit>>) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let mut builder = MapBuilder::memory();
    let mut lists = Vec::with_capacity(map.len());
    for (key, mut hits) in map {
        if key.is_empty() {
            continue;
        }
        hits.sort_by_key(|hit| (hit.span_index, hit.byte_off));
        hits.dedup();
        let id = lists.len() as u64;
        builder.insert(key.as_bytes(), id)?;
        lists.push(hits);
    }
    let fst_bytes = builder.into_inner()?;
    Ok((fst_bytes, pack_postings(&lists)?))
}

fn pack_postings(lists: &[Vec<Hit>]) -> Result<Vec<u8>, Error> {
    let list_count = u32_fit_usize(lists.len(), "posting list count")?;
    let dir_off = 8usize;
    let dir_bytes = lists
        .len()
        .checked_mul(POSTING_DIR_ENTRY_LEN)
        .ok_or_else(|| Error::ingest("posting directory overflow"))?;
    let hits_start = dir_off
        .checked_add(dir_bytes)
        .ok_or_else(|| Error::ingest("posting directory overflow"))?;
    let mut out = vec![0u8; hits_start];
    out[0..4].copy_from_slice(&list_count.to_le_bytes());
    let mut hits_off = hits_start as u64;
    for (index, list) in lists.iter().enumerate() {
        let hit_count = u32_fit_usize(list.len(), "posting list length")?;
        let entry = dir_off + index * POSTING_DIR_ENTRY_LEN;
        out[entry..entry + 8].copy_from_slice(&hits_off.to_le_bytes());
        out[entry + 8..entry + 12].copy_from_slice(&hit_count.to_le_bytes());
        hits_off = hits_off
            .checked_add((list.len() * HIT_LEN) as u64)
            .ok_or_else(|| Error::ingest("posting hits overflow"))?;
    }
    for list in lists {
        for hit in list {
            out.extend_from_slice(&hit.span_index.to_le_bytes());
            out.extend_from_slice(&hit.byte_off.to_le_bytes());
        }
    }
    Ok(out)
}

fn materialize(archive: &Archive, hits: &[TextMatch]) -> Result<Vec<SearchHit>, Error> {
    let text = archive.text()?;
    let root = archive.root()?;
    let mut out = Vec::with_capacity(hits.len());
    for hit in hits {
        let span = archive.span(hit.span_index as usize)?;
        let span_text = span_str(text, &span)?;
        let off = hit.byte_off as usize;
        if off >= span_text.len() || !span_text.is_char_boundary(off) {
            return Err(Error::InvalidArchive);
        }
        let match_len = hit.match_len as usize;
        let snippet = make_snippet(span_text, off, match_len);
        let Some(record) = root.conversations.get(span.record_index as usize) else {
            return Err(Error::InvalidArchive);
        };
        let conversation_id = record
            .item
            .conversation
            .id
            .as_ref()
            .map(|id| id.as_str().to_owned());
        out.push(SearchHit {
            conversation_id,
            field: field_name(span.field_id),
            snippet,
            span_off: span.text_off,
            span_text: span_text.to_owned(),
        });
    }
    Ok(out)
}

fn field_name(field_id: u32) -> &'static str {
    match field_id {
        FIELD_TITLE => "title",
        FIELD_SUMMARY => "summary",
        FIELD_MESSAGE => "message",
        _ => "unknown",
    }
}

fn make_snippet(span_text: &str, token_off: usize, token_len: usize) -> String {
    let token_end = token_off.saturating_add(token_len).min(span_text.len());
    let mut start = token_off.saturating_sub(SNIPPET_RADIUS);
    while start > 0 && !span_text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = token_end
        .saturating_add(SNIPPET_RADIUS)
        .min(span_text.len());
    while end < span_text.len() && !span_text.is_char_boundary(end) {
        end += 1;
    }
    let mut snippet = String::new();
    if start > 0 {
        snippet.push_str("...");
    }
    for ch in span_text[start..end].chars() {
        if matches!(ch, '\n' | '\r' | '\t') {
            snippet.push(' ');
        } else {
            snippet.push(ch);
        }
    }
    if end < span_text.len() {
        snippet.push_str("...");
    }
    snippet
}

fn span_str<'a>(text: &'a str, span: &TextSpan) -> Result<&'a str, Error> {
    let start = usize::try_from(span.text_off).map_err(|_| Error::InvalidArchive)?;
    let len = usize::try_from(span.len).map_err(|_| Error::InvalidArchive)?;
    let end = start.checked_add(len).ok_or(Error::InvalidArchive)?;
    if end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return Err(Error::InvalidArchive);
    }
    Ok(&text[start..end])
}

fn for_each_token<E>(
    text: &str,
    mut visit: impl FnMut(usize, &str) -> Result<(), E>,
) -> Result<(), E> {
    let mut start = None;
    for (index, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            if start.is_none() {
                start = Some(index);
            }
        } else if let Some(token_start) = start.take() {
            let token = &text[token_start..index];
            if !token.is_empty() {
                visit(token_start, token)?;
            }
        }
    }
    if let Some(token_start) = start {
        let token = &text[token_start..];
        if !token.is_empty() {
            visit(token_start, token)?;
        }
    }
    Ok(())
}

fn append_aligned(buf: &mut Vec<u8>, bytes: &[u8]) -> (u64, u64) {
    let pad = (8 - (buf.len() % 8)) % 8;
    buf.resize(buf.len() + pad, 0);
    let off = buf.len() as u64;
    buf.extend_from_slice(bytes);
    (off, bytes.len() as u64)
}

fn read_usize(bytes: &[u8], off: usize) -> Result<usize, Error> {
    usize::try_from(u64_from(bytes, off).map_err(|_| Error::InvalidTries)?)
        .map_err(|_| Error::InvalidTries)
}

#[cfg(test)]
mod unique_print_tests {
    use super::{
        DEFAULT_SEARCH_MAX_COUNT, SEARCH_PRINT_FLOOD_LINES, SearchHit, SearchSnippetGroup,
        group_snippet_occurrences, unique_hits, write_hits_grouped,
    };

    fn groups_for(hits: &[SearchHit]) -> Vec<SearchSnippetGroup> {
        group_snippet_occurrences(hits.iter().map(|hit| (None, hit)))
    }

    fn hit(id: &str, span_off: u64, span_text: &str) -> SearchHit {
        hit_with_snippet(id, span_off, span_text, span_text)
    }

    fn hit_with_snippet(id: &str, span_off: u64, span_text: &str, snippet: &str) -> SearchHit {
        SearchHit {
            conversation_id: Some(id.to_owned()),
            field: "message",
            snippet: snippet.to_owned(),
            span_off,
            span_text: span_text.to_owned(),
        }
    }

    #[test]
    fn same_span_twice_is_one_unique_hit() {
        let found = unique_hits(vec![
            hit("same-convo", 40, "packed twice"),
            hit("same-convo", 40, "packed twice"),
        ]);
        assert_eq!(found.hits.len(), 1);
        assert_eq!(found.duplicate_omitted, 1);
        assert_eq!(found.hits[0].conversation_id.as_deref(), Some("same-convo"));
    }

    #[test]
    fn same_span_text_two_offsets_is_one_unique_hit() {
        let found = unique_hits(vec![
            hit_with_snippet("same-convo", 40, "packed twice", "first snippet"),
            hit_with_snippet("same-convo", 4000, "packed twice", "second snippet"),
        ]);
        assert_eq!(found.hits.len(), 1);
        assert_eq!(found.duplicate_omitted, 1);
        assert_eq!(
            found.hits[0].snippet, "first snippet",
            "snippet must come from the first kept packed copy"
        );
        assert_eq!(found.hits[0].span_off, 40);
    }

    #[test]
    fn sliding_snippets_same_span_are_one_unique_hit() {
        let body = "packed message to you, I did tell myself extra";
        let found = unique_hits(vec![
            hit_with_snippet("same-convo", 100, body, "... to you, I did tell myself"),
            hit_with_snippet("same-convo", 100, body, "...to you, I did tell myself"),
            hit_with_snippet("same-convo", 100, body, "...o you, I did tell myself"),
        ]);
        assert_eq!(found.hits.len(), 1);
        assert_eq!(found.duplicate_omitted, 2);
        assert_eq!(
            found.hits[0].snippet, "... to you, I did tell myself",
            "snippet must come from the first match in that packed message"
        );
    }

    #[test]
    fn different_message_bodies_stay_two_unique_hits() {
        let found = unique_hits(vec![
            hit("same-convo", 100, "alpha-unique-msg"),
            hit("same-convo", 200, "beta-unique-msg"),
        ]);
        assert_eq!(found.hits.len(), 2);
        assert_eq!(found.duplicate_omitted, 0);
    }

    #[test]
    fn same_snippet_window_different_bodies_stay_two_unique_hits() {
        let found = unique_hits(vec![
            hit_with_snippet(
                "same-convo",
                100,
                "aaa in the first packed message body",
                "aaa",
            ),
            hit_with_snippet(
                "same-convo",
                200,
                "aaa in the second packed message body",
                "aaa",
            ),
        ]);
        assert_eq!(
            found.hits.len(),
            2,
            "unique hits must not key on the snippet window"
        );
        assert_eq!(found.duplicate_omitted, 0);
    }

    #[test]
    fn title_and_message_stay_two_unique_hits() {
        let found = unique_hits(vec![
            SearchHit {
                conversation_id: Some("same-convo".into()),
                field: "title",
                snippet: "aaa".into(),
                span_off: 0,
                span_text: "aaa".into(),
            },
            SearchHit {
                conversation_id: Some("same-convo".into()),
                field: "message",
                snippet: "aaa".into(),
                span_off: 40,
                span_text: "aaa".into(),
            },
        ]);
        assert_eq!(found.hits.len(), 2);
        assert_eq!(found.duplicate_omitted, 0);
        let groups = groups_for(&found.hits);
        assert_eq!(
            groups.len(),
            1,
            "the grouping key is full span text, so the same packed bytes are one snippet, got {groups:?}"
        );
        assert_eq!(groups[0].occurrences.len(), 2);
        let mut fields: Vec<_> = groups[0]
            .occurrences
            .iter()
            .map(|occurrence| occurrence.field)
            .collect();
        fields.sort();
        assert_eq!(fields, ["message", "title"]);
    }

    #[test]
    fn two_conversations_stay_two_unique_hits() {
        let found = unique_hits(vec![
            hit("convo-a", 0, "alpha unique text"),
            hit("convo-b", 40, "beta unique text"),
        ]);
        assert_eq!(found.hits.len(), 2);
        assert_eq!(found.duplicate_omitted, 0);
    }

    #[test]
    fn two_conversations_same_body_are_one_snippet_two_occurrences() {
        let found = unique_hits(vec![
            hit("convo-a", 0, "same sentence"),
            hit("convo-b", 40, "same sentence"),
        ]);
        assert_eq!(
            found.hits.len(),
            2,
            "uniqueness still keeps both conversation ids; grouping must not drop an occurrence"
        );
        assert_eq!(found.duplicate_omitted, 0);
        let groups = groups_for(&found.hits);
        assert_eq!(
            groups.len(),
            1,
            "presentation is one snippet for the same packed body, got {groups:?}"
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
        assert_eq!(ids, ["convo-a", "convo-b"]);
        assert_eq!(groups[0].snippet, "same sentence");
    }

    #[test]
    fn two_different_bodies_are_two_snippet_groups() {
        let found = unique_hits(vec![
            hit("convo-a", 0, "alpha unique text"),
            hit("convo-b", 40, "beta unique text"),
        ]);
        let groups = groups_for(&found.hits);
        assert_eq!(
            groups.len(),
            2,
            "different packed bodies stay two snippet blocks, got {groups:?}"
        );
        assert!(groups.iter().all(|group| group.occurrences.len() == 1));
    }

    #[test]
    fn overlapping_same_id_same_bytes_are_one_snippet_one_occurrence() {
        let found = unique_hits(vec![
            hit("dump-convo", 40, "packed-copy-token"),
            hit("dump-convo", 4000, "packed-copy-token"),
        ]);
        assert_eq!(found.hits.len(), 1);
        let groups = groups_for(&found.hits);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].occurrences.len(),
            1,
            "overlapping dumps of the same id and bytes stay one occurrence, got {groups:?}"
        );
    }

    #[test]
    fn write_hits_grouped_prints_same_body_once_then_occurrences() {
        let hits = unique_hits(vec![
            hit("convo-a", 0, "same sentence"),
            hit("convo-b", 40, "same sentence"),
        ])
        .hits;
        let mut out = Vec::new();
        write_hits_grouped(&mut out, &hits, 0, 0).expect("print grouped hits");
        let text = String::from_utf8(out).expect("utf-8");
        assert_eq!(
            text.matches("same sentence").count(),
            1,
            "the paragraph must print once, got {text:?}"
        );
        assert!(text.contains("present in:"), "got {text:?}");
        assert!(
            text.contains("conversation convo-a  field message"),
            "got {text:?}"
        );
        assert!(
            text.contains("conversation convo-b  field message"),
            "got {text:?}"
        );
    }

    #[test]
    fn missing_conversation_id_same_body_is_one_unique_hit() {
        let found = unique_hits(vec![
            SearchHit {
                conversation_id: None,
                field: "message",
                snippet: "first snippet".into(),
                span_off: 0,
                span_text: "packed body".into(),
            },
            SearchHit {
                conversation_id: None,
                field: "message",
                snippet: "second snippet".into(),
                span_off: 80,
                span_text: "packed body".into(),
            },
        ]);
        assert_eq!(found.hits.len(), 1);
        assert_eq!(found.duplicate_omitted, 1);
        assert_eq!(found.hits[0].snippet, "first snippet");
    }

    #[test]
    fn missing_conversation_id_different_bodies_stay_two_unique_hits() {
        let found = unique_hits(vec![
            SearchHit {
                conversation_id: None,
                field: "message",
                snippet: "alpha".into(),
                span_off: 0,
                span_text: "alpha body".into(),
            },
            SearchHit {
                conversation_id: None,
                field: "message",
                snippet: "beta".into(),
                span_off: 80,
                span_text: "beta body".into(),
            },
        ]);
        assert_eq!(found.hits.len(), 2);
        assert_eq!(found.duplicate_omitted, 0);
    }

    #[test]
    fn default_print_cap_does_not_print_250k_lines() {
        const {
            assert!(DEFAULT_SEARCH_MAX_COUNT > 0);
            assert!(DEFAULT_SEARCH_MAX_COUNT < SEARCH_PRINT_FLOOD_LINES);
        }
        let hits: Vec<SearchHit> = (0..1_000)
            .map(|i| hit(&format!("c{i}"), i as u64, &format!("snippet {i}")))
            .collect();
        let mut out = Vec::new();
        let printed = write_hits_grouped(&mut out, &hits, DEFAULT_SEARCH_MAX_COUNT, 0)
            .expect("print grouped hits");
        assert_eq!(printed, DEFAULT_SEARCH_MAX_COUNT);
        let text = String::from_utf8(out).expect("utf-8");
        let lines = text.lines().count();
        assert!(
            lines < SEARCH_PRINT_FLOOD_LINES,
            "default cap must not print 250k lines, got {lines}"
        );
        assert!(
            text.contains("more snippet groups not shown"),
            "suppressed snippet-group count must appear, got {text:?}"
        );
        assert_eq!(
            text.matches("present in:").count(),
            DEFAULT_SEARCH_MAX_COUNT
        );
    }
}

#[cfg(test)]
mod search_workers_tests {
    use super::search_worker_count;

    #[test]
    fn search_worker_count_is_min_of_files_and_available_parallelism() {
        assert_eq!(search_worker_count(0), 1);
        assert_eq!(search_worker_count(1), 1);
        let cpus = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);
        assert_eq!(search_worker_count(cpus), cpus);
        assert_eq!(search_worker_count(cpus.saturating_add(50)), cpus);
        if cpus > 4 {
            assert!(
                search_worker_count(8) > 4,
                "there is no four-map cap; eight files on more than four CPUs use more than four workers"
            );
        }
        assert_eq!(
            search_worker_count(2),
            2.min(cpus),
            "two archives use min(2, available parallelism) workers"
        );
    }
}
