//! Compact on-disk tries (FST maps) plus packed posting lists.
//!
//! The tries region is a slice of the same `.majestic` file and is packed at
//! ingest. Search runs PCRE2 (grep-pcre2, same engine as `rg -P`) on the mmap
//! UTF-8 text blob. AND any order uses PCRE2 lookaheads. Search does not use
//! rust-regex or PCRE1. It does not parse JSON and does not spawn the `rg`
//! binary.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use fst::MapBuilder;
use grep_matcher::Matcher;
use grep_pcre2::{RegexMatcher as Pcre2Matcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkMatch};
use rayon::prelude::*;

use crate::Error;
use crate::archive::{
    Archive, FIELD_MESSAGE, FIELD_SUMMARY, FIELD_TITLE, TextSpan, slice_at, u32_fit_usize,
    u32_from, u64_from, write_u64,
};

/// Inner tries-region magic. Eight bytes, no NUL.
pub const TRIES_MAGIC: [u8; 8] = *b"MAJTRIES";
/// Tries-region layout version. Archive file version stays 1.
pub const TRIES_VERSION: u32 = 1;
/// Inner header size, including magic and version.
pub const TRIES_HEADER_LEN: usize = 80;
const HIT_LEN: usize = 8;
const POSTING_DIR_ENTRY_LEN: usize = 16;
const SNIPPET_RADIUS: usize = 56;
/// Unique hits printed per archive. `0` means no print cap.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub conversation_id: Option<String>,
    pub field: &'static str,
    pub snippet: String,
}

/// Flags for `memex search`. Patterns are PCRE2 (`grep-pcre2`).
///
/// `-i` / [`Self::ignore_case`] is case insensitive. `-w` /
/// [`Self::word_regexp`] is whole word. `-F` / [`Self::fixed_strings`] is a
/// phrase or literal. `|` is OR. AND any order uses lookaheads:
/// `(?=.*lizard)(?=.*the)`.
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
    /// Extra PCRE2 hits that matched the same conversation, field, and snippet.
    pub duplicate_omitted: usize,
}

/// Matcher flags plus print cap.
///
/// [`Self::flags`] compile the PCRE2 matcher. [`Self::max_count`] is unique
/// hits printed per archive (`0` means no cap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchExec {
    pub flags: SearchFlags,
    pub max_count: usize,
}

impl Default for SearchExec {
    fn default() -> Self {
        Self {
            flags: SearchFlags::default(),
            max_count: DEFAULT_SEARCH_MAX_COUNT,
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

/// Search the mmap UTF-8 text blob with PCRE2 (same engine as `rg -P`).
///
/// Default pattern is PCRE2. AND any order is `(?=.*lizard)(?=.*the)`.
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
    let matcher = compile_matcher(pattern, flags)?;
    Ok(search_blob(archive, &matcher)?.hits)
}

/// Open `path`, search, print unique hits grouped by conversation.
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

/// Open `path` and print unique hits grouped by conversation.
pub fn write_search_exec(
    path: impl AsRef<Path>,
    pattern: &str,
    exec: SearchExec,
    mut out: impl Write,
) -> Result<(), Error> {
    let path = path.as_ref();
    let found = search_one_archive_pattern(path, pattern, exec.flags)?;
    let printed = write_archive_section(
        &mut out,
        path,
        &found.hits,
        exec.max_count,
        found.duplicate_omitted,
    )?;
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
/// Each search reads [`Archive::text`] (a mapped slice). It does not copy the
/// text blob into a `Vec`. A systemwide search lists paths, maps every listed
/// archive and holds those maps, compiles the PCRE2 pattern once, then runs
/// PCRE2 on already-mapped text with `min(file count, available parallelism)`
/// workers. That is parallelism on mapped slices. It does not `par_iter` the
/// path list as the architecture (open and search each path as a job). There
/// is no four-map cap. Search does not call `MADV_DONTNEED` after each archive
/// while the user is still searching. Unreadable or corrupt files are skipped
/// so one junk file does not abort the rest. Permission denied is an error on
/// the terminal. Other unreadable files warn in the journal. An empty memex
/// directory is [`Error::NoArchives`].
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
    Ok(search_listed_archives(memex_dir, pattern, flags)?
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
    Ok(search_listed_archives(memex_dir, pattern, exec.flags)?
        .into_iter()
        .map(|(path, found)| SearchGroup {
            path,
            origin: SearchOrigin::Archive,
            hits: found.hits,
            duplicate_omitted: found.duplicate_omitted,
        })
        .collect())
}

fn search_listed_archives(
    memex_dir: &Path,
    pattern: &str,
    flags: SearchFlags,
) -> Result<Vec<(PathBuf, UniqueHits)>, Error> {
    let paths = crate::list_memex_archives(memex_dir)?;
    if paths.is_empty() {
        return Err(Error::NoArchives(memex_dir.to_path_buf()));
    }
    // Compile once so a bad pattern fails before any archive is mapped.
    let matcher = compile_matcher(pattern, flags)?;
    let mut opened = Vec::with_capacity(paths.len());
    for path in paths {
        match Archive::open(&path) {
            Ok(archive) => opened.push((path, archive)),
            Err(error) => {
                crate::logging::log_unreadable_archive(&path, &error);
            }
        }
    }
    let mut groups = search_opened_archives(&opened, &matcher);
    groups.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(groups)
}

fn search_opened_archives(
    opened: &[(PathBuf, Archive)],
    matcher: &Pcre2Matcher,
) -> Vec<(PathBuf, UniqueHits)> {
    let workers = search_worker_count(opened.len());
    if workers <= 1 || opened.len() <= 1 {
        return opened
            .iter()
            .filter_map(|(path, archive)| match search_blob(archive, matcher) {
                Ok(found) => Some((path.clone(), found)),
                Err(error) => {
                    crate::logging::log_unreadable_archive(path, &error);
                    None
                }
            })
            .collect();
    }
    let search_one = |(path, archive): &(PathBuf, Archive)| match search_blob(archive, matcher) {
        Ok(found) => Some((path.clone(), found)),
        Err(error) => {
            crate::logging::log_unreadable_archive(path, &error);
            None
        }
    };
    match rayon::ThreadPoolBuilder::new().num_threads(workers).build() {
        Ok(pool) => pool.install(|| opened.par_iter().filter_map(search_one).collect()),
        Err(_) => opened.iter().filter_map(search_one).collect(),
    }
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
) -> Result<UniqueHits, Error> {
    let archive = Archive::open(path)?;
    let matcher = compile_matcher(pattern, flags)?;
    search_blob(&archive, &matcher)
}

fn search_blob(archive: &Archive, matcher: &Pcre2Matcher) -> Result<UniqueHits, Error> {
    let text = archive.text()?;
    let spans = load_spans(archive)?;
    let mut sink = OffsetSink {
        matcher,
        matches: Vec::new(),
    };
    let mut searcher = text_blob_searcher();
    searcher
        .search_slice(matcher, text.as_bytes(), &mut sink)
        .map_err(Error::from)?;
    let mut raw_hits = Vec::new();
    let mut seen = HashSet::new();
    for (abs, match_len) in sink.matches {
        let Some((span_index, span)) = span_containing(&spans, abs) else {
            continue;
        };
        let rel = abs.saturating_sub(span.text_off);
        let remaining = span.len.saturating_sub(rel);
        // Lookahead AND is zero-width; still a hit when the span has text.
        if remaining == 0 {
            continue;
        }
        let match_len = (match_len as u64).min(remaining);
        let Ok(span_index) = u32::try_from(span_index) else {
            continue;
        };
        let Ok(byte_off) = u32::try_from(rel) else {
            continue;
        };
        let Ok(match_len) = u32::try_from(match_len) else {
            continue;
        };
        if seen.insert((span_index, byte_off)) {
            raw_hits.push(TextMatch {
                span_index,
                byte_off,
                match_len,
            });
        }
    }
    raw_hits.sort_by_key(|hit| (hit.span_index, hit.byte_off));
    let hits = materialize(archive, &raw_hits)?;
    Ok(unique_hits(hits))
}

fn compile_matcher(pattern: &str, flags: SearchFlags) -> Result<Pcre2Matcher, Error> {
    if pattern.is_empty() {
        return Err(Error::EmptyPattern);
    }
    // PCRE2 always (same engine as `rg -P`). AND any order uses lookaheads.
    // `caseless` is the grep-pcre2 builder name for case insensitive (`-i`).
    RegexMatcherBuilder::new()
        .caseless(flags.ignore_case)
        .fixed_strings(flags.fixed_strings)
        .word(flags.word_regexp)
        .utf(true)
        .ucp(true)
        .jit_if_available(true)
        .build(pattern)
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

fn span_containing(spans: &[TextSpan], abs: u64) -> Option<(usize, TextSpan)> {
    let mut index = spans.partition_point(|span| span.text_off <= abs);
    if index == 0 {
        return None;
    }
    index -= 1;
    let span = spans[index];
    let end = span.text_off.checked_add(span.len)?;
    if abs >= span.text_off && abs < end {
        Some((index, span))
    } else {
        None
    }
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
/// Grouped report: archive path relative to `memex_dir`, then conversation id,
/// then unique messages. Duplicate packed snippets print once. Does not walk
/// `$HOME` for live exports.
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
    let mut printed = 0usize;
    let mut duplicate_omitted = 0usize;
    let mut first_section = true;
    for group in &groups {
        if group.hits.is_empty() {
            duplicate_omitted = duplicate_omitted.saturating_add(group.duplicate_omitted);
            continue;
        }
        unique = unique.saturating_add(group.hits.len());
        duplicate_omitted = duplicate_omitted.saturating_add(group.duplicate_omitted);
        let label = section_label(memex_dir, &group.path, group.origin);
        if !first_section {
            writeln!(out)?;
        }
        first_section = false;
        printed = printed.saturating_add(write_archive_section(
            &mut out,
            &label,
            &group.hits,
            exec.max_count,
            group.duplicate_omitted,
        )?);
    }
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
        let key = (hit.conversation_id.clone(), hit.field, hit.snippet.clone());
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

fn write_archive_section(
    out: &mut impl Write,
    label: &Path,
    hits: &[SearchHit],
    max_count: usize,
    duplicate_omitted: usize,
) -> Result<usize, Error> {
    writeln!(out, "{}", label.display())?;
    write_hits_grouped(out, hits, max_count, duplicate_omitted)
}

fn write_hits_grouped(
    out: &mut impl Write,
    hits: &[SearchHit],
    max_count: usize,
    duplicate_omitted: usize,
) -> Result<usize, Error> {
    let cap = if max_count == 0 {
        hits.len()
    } else {
        max_count.min(hits.len())
    };
    let printed = &hits[..cap];
    let mut current_id: Option<&str> = None;
    for hit in printed {
        let id = hit.conversation_id.as_deref().unwrap_or("-");
        if current_id != Some(id) {
            writeln!(out, "  {id}")?;
            current_id = Some(id);
        }
        writeln!(out, "    {}", hit.field)?;
        writeln!(out, "      {}", hit.snippet)?;
    }
    let not_shown = hits.len().saturating_sub(cap);
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
        (1, 0) => "1 more unique message not shown".into(),
        (n, 0) => format!("{n} more unique messages not shown"),
        (1, d) => format!("1 more unique message not shown, {d} duplicate hits omitted"),
        (n, d) => format!("{n} more unique messages not shown, {d} duplicate hits omitted"),
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
        DEFAULT_SEARCH_MAX_COUNT, SEARCH_PRINT_FLOOD_LINES, SearchHit, unique_hits,
        write_hits_grouped,
    };

    fn hit(id: &str, snippet: &str) -> SearchHit {
        SearchHit {
            conversation_id: Some(id.to_owned()),
            field: "message",
            snippet: snippet.to_owned(),
        }
    }

    #[test]
    fn same_snippet_twice_is_one_unique_hit() {
        let found = unique_hits(vec![
            hit("same-convo", "packed twice"),
            hit("same-convo", "packed twice"),
        ]);
        assert_eq!(found.hits.len(), 1);
        assert_eq!(found.duplicate_omitted, 1);
        assert_eq!(found.hits[0].conversation_id.as_deref(), Some("same-convo"));
    }

    #[test]
    fn two_conversations_stay_two_unique_hits() {
        let found = unique_hits(vec![
            hit("convo-a", "alpha unique text"),
            hit("convo-b", "beta unique text"),
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
            .map(|i| hit(&format!("c{i}"), &format!("snippet {i}")))
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
            text.contains("more unique messages not shown"),
            "suppressed unique count must appear, got {text:?}"
        );
        assert_eq!(
            text.matches("    message\n").count(),
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
    }
}
