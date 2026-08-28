//! Stream Grok export JSON, Telegram Desktop `result.json`, ChatGPT
//! `conversations-*.json`, Facebook DYI (`your_facebook_activity/`), X
//! account archives (`data/account.js` plus tweets), zip sources (no extract),
//! markdown, and grok-oss sqlite into an archive.
//!
//! The backend file is not parsed as a `serde_json::Value` document. Top-level
//! keys are visited in order. Each `{conversation, responses}` object is
//! deserialized on its own and merged by identity (id, byte length, mtime,
//! BaoTree root, responses sub-hash). Telegram chats use the same identity
//! hash: the same chat id with different bytes stays two encodings. grok-oss
//! session JSONL (`chat_history.jsonl` as chat turns; other session `*.jsonl`
//! as leftover JSON on the export) maps into the same conversation records;
//! unknown keys stay in leftover maps. Markdown files and `session_docs`
//! sqlite rows use the same conversation records. Asset `content` files are
//! hashed with the same BaoTree hasher. Two files are the same only when those
//! roots match. Matching bytes keep one catalog row, one stored body, and
//! provenance. File bodies live in the bao blob (`blob_off` / `blob_len`). Do
//! not store a hash field on leftover maps. `memex ingest` with no paths walks
//! `$HOME` for known export shapes only (max [`HOME_SCAN_MAX_DEPTH`] directory
//! levels, or `scan.home_scan_max_depth`, no symlinks). Home scan skips system
//! trash when [`crate::config::ScanConfig::skip_system_trash`] and extra
//! [`crate::config::ScanConfig::skip_directories`]. Crate defaults do not skip
//! `~/.agents/trash`.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::CHATGPT_SERVICE;
use crate::Error;
use crate::GROK_OSS_SERVICE;
use crate::GROK_SERVICE;
use crate::MARKDOWN_SERVICE;
use crate::META_SERVICE;
use crate::OBSIDIAN_SERVICE;
use crate::REPORTS_SERVICE;
use crate::TELEGRAM_SERVICE;
use crate::X_SERVICE;
use crate::archive::{
    Archive, ArchiveRoot, AssetEntry, ConversationRecord, ExportManifest, FIELD_MESSAGE,
    FIELD_SUMMARY, FIELD_TITLE, RECORD_CONVERSATION, RECORD_RESPONSE, TaggedAuth, TaggedBilling,
    TaggedJson, TextSpan, u32_fit_usize, write_archive,
};
use crate::config::Config;
use crate::hash::{self, Digest};
use crate::ingest_chatgpt;
use crate::ingest_facebook;
use crate::ingest_sqlite;
use crate::ingest_telegram;
use crate::ingest_x;
use crate::schema::{
    AuthFile, BillingFile, Conversation, ConversationItem, ExtraMap, JsonAtom, Response,
    ResponseItem, Timestamp,
};
use crate::scoped_archive_path;
use crate::zip::{self, MAX_UNCOMPRESSED, ZipKind};

const SPILL_MEDIA: u8 = 2;
const SPILL_PROJECT: u8 = 3;
const SPILL_TASK: u8 = 4;
const BACKEND_FILE_NAME: &str = "prod-grok-backend.json";
const AUTH_FILE_NAME: &str = "prod-mc-auth-mgmt-api.json";
const BILLING_FILE_NAME: &str = "prod-mc-billing.json";
const ASSET_SERVER_DIR_NAME: &str = "prod-mc-asset-server";
const CHAT_HISTORY_FILE_NAME: &str = "chat_history.jsonl";
const SESSION_SEARCH_SQLITE_NAME: &str = "session_search.sqlite";
const GROK_OSS_DB_NAME: &str = "grok_oss.db";
const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";
const WALK_DEPTH: u32 = 8;
/// Directory levels below `$HOME` for `memex ingest` with no input paths.
pub const HOME_SCAN_MAX_DEPTH: u32 = 8;
/// Conversations parsed from JSON, then identity-hashed with Rayon.
const CONVERSATION_IDENTITY_BATCH: usize = 128;

enum IngestSource {
    Backend(PathBuf),
    SessionDir(PathBuf),
    SessionJsonl(PathBuf),
    Markdown(PathBuf),
    Sqlite(PathBuf),
    Telegram(PathBuf),
    ChatGpt(PathBuf),
    Facebook(PathBuf),
    Twitter(PathBuf),
    Zip(PathBuf),
}

impl IngestSource {
    fn path(&self) -> &Path {
        match self {
            Self::Backend(path)
            | Self::SessionDir(path)
            | Self::SessionJsonl(path)
            | Self::Markdown(path)
            | Self::Sqlite(path)
            | Self::Telegram(path)
            | Self::ChatGpt(path)
            | Self::Facebook(path)
            | Self::Twitter(path)
            | Self::Zip(path) => path,
        }
    }
}

/// Counts written by [`ingest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestReport {
    pub output: PathBuf,
    pub exports: usize,
    pub conversations: usize,
}

/// Folder and username file stem inferred from an ingest input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredScope {
    pub service: String,
    pub account: String,
}

#[derive(Default)]
struct Accumulator {
    exports: Vec<ExportManifest>,
    conversations: Vec<ConversationRecord>,
    by_id: HashMap<String, usize>,
    media_posts: Vec<TaggedJson>,
    projects: Vec<TaggedJson>,
    tasks: Vec<TaggedJson>,
    auth: Vec<TaggedAuth>,
    billing: Vec<TaggedBilling>,
    assets: Vec<AssetEntry>,
    /// BaoTree roots of hashed `content` bytes, parallel to `assets`.
    asset_digests: Vec<Digest>,
    /// Uploaded file bodies, parallel to `assets`. Same digest keeps one body.
    asset_bodies: Vec<Vec<u8>>,
    /// Identity hashes, parallel to `conversations`.
    idents: Vec<ItemIdentity>,
    /// Zip entry skip limit from [`crate::config::ZipConfig::max_uncompressed_bytes`].
    max_uncompressed_bytes: u64,
}

/// Stream `inputs` into `output`. Existing output is the base set.
pub fn ingest(output: impl AsRef<Path>, inputs: &[PathBuf]) -> Result<IngestReport, Error> {
    let home = crate::home_dir().ok();
    ingest_with_config(output, inputs, home.as_deref(), &Config::crate_defaults())
}

/// Same as [`ingest`], with skip lists and zip limits from `config`.
///
/// `home` is the process home (or a fake home in tests). Extra
/// [`crate::config::ScanConfig::skip_directories`] apply to explicit dump
/// walks and markdown collection, not only a home scan.
pub fn ingest_with_config(
    output: impl AsRef<Path>,
    inputs: &[PathBuf],
    home: Option<&Path>,
    config: &Config,
) -> Result<IngestReport, Error> {
    let output = output.as_ref();
    if inputs.is_empty() {
        return Err(Error::ingest("no input paths"));
    }

    tracing::info!(
        output = %output.display(),
        input_count = inputs.len(),
        "ingest started"
    );
    for input in inputs {
        tracing::debug!(path = %input.display(), "ingest input path");
    }

    create_output_parent(output)?;

    let home_buf = home
        .map(Path::to_path_buf)
        .or_else(|| crate::home_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let home = home_buf.as_path();

    let mut acc = if output.is_file() {
        Accumulator::from_archive(output)?
    } else {
        Accumulator::default()
    };
    acc.max_uncompressed_bytes = config.zip.max_uncompressed_bytes;

    let mut sources = Vec::new();
    for input in inputs {
        let found = discover_sources(input, home, config)?;
        if found.is_empty() {
            return Err(Error::ingest(format!(
                "no Grok dump, ChatGPT conversations export, Telegram result.json, Facebook your_facebook_activity, X account archive, markdown, or session_docs sqlite under {}",
                input.display()
            )));
        }
        sources.extend(found);
    }
    let sources = filter_duplicate_facebook_zip_sources(sources)?;
    let sources = filter_duplicate_zip_and_unpacked_sources(sources)?;
    for source in sources {
        match source {
            IngestSource::Backend(path) => acc.ingest_backend(&path)?,
            IngestSource::SessionDir(path) => acc.ingest_session_dir(&path)?,
            IngestSource::SessionJsonl(path) => acc.ingest_session_jsonl(&path)?,
            IngestSource::Markdown(path) => acc.ingest_markdown(&path, home, config)?,
            IngestSource::Sqlite(path) => acc.ingest_sqlite(&path)?,
            IngestSource::Telegram(path) => acc.ingest_telegram(&path)?,
            IngestSource::ChatGpt(path) => acc.ingest_chatgpt(&path)?,
            IngestSource::Facebook(path) => acc.ingest_facebook_dir(&path)?,
            IngestSource::Twitter(path) => acc.ingest_twitter_dir(&path)?,
            IngestSource::Zip(path) => acc.ingest_zip(&path)?,
        }
    }

    let report = acc.pack(output)?;
    tracing::info!(
        output = %report.output.display(),
        exports = report.exports,
        conversations = report.conversations,
        "wrote archive"
    );
    Ok(report)
}

struct SeenFacebookZip {
    path: PathBuf,
    size: u64,
    digest: Option<Digest>,
}

impl SeenFacebookZip {
    fn digest(&mut self) -> Result<Digest, Error> {
        if let Some(digest) = self.digest {
            return Ok(digest);
        }
        let digest = hash::hash_path(&self.path)?;
        self.digest = Some(digest);
        Ok(digest)
    }
}

fn is_facebook_zip(path: &Path) -> bool {
    zip::is_zip_path(path) && matches!(zip::classify_zip(path), Ok(Some(ZipKind::Facebook)))
}

fn is_twitter_zip(path: &Path) -> bool {
    zip::is_zip_path(path) && matches!(zip::classify_zip(path), Ok(Some(ZipKind::Twitter)))
}

/// Skip a Facebook zip whose full-file hash matches another already in this group.
///
/// Same size first, then hash. Does not ingest the duplicate. Logs path and
/// `reason=duplicate of` the kept path. Not message bodies.
fn filter_duplicate_facebook_zip_sources(
    sources: Vec<IngestSource>,
) -> Result<Vec<IngestSource>, Error> {
    let mut out = Vec::with_capacity(sources.len());
    let mut seen: Vec<SeenFacebookZip> = Vec::new();
    for source in sources {
        let path = match &source {
            IngestSource::Zip(path) if is_facebook_zip(path) || is_twitter_zip(path) => {
                path.clone()
            }
            _ => {
                out.push(source);
                continue;
            }
        };
        let size = fs::metadata(&path)?.len();
        let mut duplicate_of = None;
        for kept in &mut seen {
            if kept.size != size {
                continue;
            }
            let kept_digest = kept.digest()?;
            let incoming = hash::hash_path(&path)?;
            if incoming == kept_digest {
                duplicate_of = Some(kept.path.clone());
                break;
            }
        }
        if let Some(other) = duplicate_of {
            tracing::debug!(
                path = %path.display(),
                reason = %format!("duplicate of {}", other.display()),
                "skipped a duplicate zip"
            );
            continue;
        }
        seen.push(SeenFacebookZip {
            path: path.clone(),
            size,
            digest: None,
        });
        out.push(source);
    }
    Ok(out)
}

/// When a zip and an unpacked directory (or file) carry the same export
/// payload, keep one. Does not delete source dumps.
fn filter_duplicate_zip_and_unpacked_sources(
    sources: Vec<IngestSource>,
) -> Result<Vec<IngestSource>, Error> {
    let mut out = Vec::with_capacity(sources.len());
    let mut seen: Vec<(zip::ZipKind, Digest, PathBuf, bool)> = Vec::new();
    for source in sources {
        let is_zip = matches!(source, IngestSource::Zip(_));
        let Some((kind, digest, path)) = source_payload_identity(&source)? else {
            out.push(source);
            continue;
        };
        if let Some((_, _, kept, kept_zip)) = seen
            .iter()
            .find(|(kept_kind, kept_digest, _, _)| *kept_kind == kind && *kept_digest == digest)
        {
            // Two unpacked dumps with the same bytes are overlapping exports.
            // Skip only when a zip and an unpacked tree (or two zips) match.
            if is_zip || *kept_zip {
                tracing::debug!(
                    path = %path.display(),
                    reason = %format!(
                        "same export payload as {}; keeping one copy",
                        kept.display()
                    ),
                    "skipped a duplicate export"
                );
                continue;
            }
        }
        seen.push((kind, digest, path, is_zip));
        out.push(source);
    }
    Ok(out)
}

fn source_payload_identity(
    source: &IngestSource,
) -> Result<Option<(zip::ZipKind, Digest, PathBuf)>, Error> {
    match source {
        IngestSource::ChatGpt(path) => Ok(Some((
            ZipKind::ChatGpt,
            chatgpt_payload_digest(path)?,
            path.clone(),
        ))),
        IngestSource::Backend(path) => {
            Ok(Some((ZipKind::Grok, hash::hash_path(path)?, path.clone())))
        }
        IngestSource::Telegram(path) => Ok(Some((
            ZipKind::Telegram,
            hash::hash_path(path)?,
            path.clone(),
        ))),
        IngestSource::Zip(path) => match zip::classify_zip(path)? {
            Some(ZipKind::ChatGpt) => Ok(Some((
                ZipKind::ChatGpt,
                chatgpt_zip_digest(path)?,
                path.clone(),
            ))),
            Some(ZipKind::Grok) => Ok(hash_zip_named_entry(path, BACKEND_FILE_NAME)?
                .map(|digest| (ZipKind::Grok, digest, path.clone()))),
            Some(ZipKind::Telegram) => Ok(hash_zip_named_entry(
                path,
                ingest_telegram::RESULT_FILE_NAME,
            )?
            .map(|digest| (ZipKind::Telegram, digest, path.clone()))),
            Some(ZipKind::Facebook | ZipKind::Twitter) | None => Ok(None),
        },
        IngestSource::SessionDir(_)
        | IngestSource::SessionJsonl(_)
        | IngestSource::Markdown(_)
        | IngestSource::Sqlite(_)
        | IngestSource::Facebook(_)
        | IngestSource::Twitter(_) => Ok(None),
    }
}

fn chatgpt_payload_digest(path: &Path) -> Result<Digest, Error> {
    if path.is_file() {
        return hash::hash_path(path);
    }
    let mut files = ingest_chatgpt::conversation_files_in_dir(path)?;
    files.sort();
    let mut acc = Vec::new();
    for file in files {
        let name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("conversations.json");
        acc.extend_from_slice(name.as_bytes());
        acc.push(0);
        acc.extend_from_slice(&hash::hash_path(&file)?.root);
    }
    Ok(hash::hash_bytes(&acc))
}

fn chatgpt_zip_digest(path: &Path) -> Result<Digest, Error> {
    let mut parts: Vec<(String, Digest)> = Vec::new();
    zip::for_each_matching_entry(
        path,
        MAX_UNCOMPRESSED,
        ingest_chatgpt::is_conversations_json_name,
        |name, reader| {
            parts.push((zip::inner_file_name(name).to_owned(), hash_reader(reader)?));
            Ok(())
        },
    )?;
    parts.sort_by(|left, right| left.0.cmp(&right.0));
    let mut acc = Vec::new();
    for (name, digest) in parts {
        acc.extend_from_slice(name.as_bytes());
        acc.push(0);
        acc.extend_from_slice(&digest.root);
    }
    Ok(hash::hash_bytes(&acc))
}

fn hash_zip_named_entry(path: &Path, file_name: &str) -> Result<Option<Digest>, Error> {
    zip::with_named_entry(path, file_name, MAX_UNCOMPRESSED, hash_reader)
}

fn hash_reader(reader: &mut dyn Read) -> Result<Digest, Error> {
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf)?;
    Ok(hash::hash_bytes(&buf))
}

/// Ingest explicit paths, or scan `home` when `inputs` is empty.
///
/// Home scan always infers per source. `-o` with a scan is an error (many
/// archives). `--service` or `--account` with a scan is an error; pass
/// explicit paths to force those flags. Home scan uses crate defaults for
/// skip lists; [`ingest_from_flags_with_config`] applies loaded config.
pub fn ingest_from_flags(
    home: &Path,
    output: Option<&Path>,
    service: Option<&str>,
    account: Option<&str>,
    inputs: &[PathBuf],
) -> Result<Vec<IngestReport>, Error> {
    ingest_from_flags_with_config(
        home,
        output,
        service,
        account,
        inputs,
        &Config::crate_defaults(),
    )
}

/// Same as [`ingest_from_flags`], with skip lists and scan depth from `config`.
pub fn ingest_from_flags_with_config(
    home: &Path,
    output: Option<&Path>,
    service: Option<&str>,
    account: Option<&str>,
    inputs: &[PathBuf],
    config: &Config,
) -> Result<Vec<IngestReport>, Error> {
    if inputs.is_empty() {
        if output.is_some() {
            return Err(Error::ingest(
                "home scan writes many archives; do not pass -o",
            ));
        }
        if service.is_some() || account.is_some() {
            return Err(Error::ingest(
                "home scan always infers per source; pass explicit paths to force those flags",
            ));
        }
        return ingest_home_with_config(home, config);
    }
    let output = match output {
        Some(path) => path.to_path_buf(),
        None => {
            let memex_dir = crate::config::expand_tilde(&config.memex_dir, home);
            resolve_ingest_archive_with(&memex_dir, service, account, inputs, home, config)?
        }
    };
    Ok(vec![ingest_with_config(
        &output,
        inputs,
        Some(home),
        config,
    )?])
}

/// Walk `home` for known export shapes and ingest each into its inferred archive.
///
/// Discovers official Grok dumps, ChatGPT `conversations-*.json` dirs and
/// zips, Facebook DYI zips and `your_facebook_activity/` trees, X account
/// archive zips and `data/account.js` trees, Telegram `result.json`,
/// Obsidian vaults, `.agents/reports` directories, and
/// `session_search.sqlite` with `session_docs`. Does not ingest arbitrary
/// markdown trees. Skips skip-list names (including `sandbox-blocked-dir*`),
/// system trash when [`crate::config::ScanConfig::skip_system_trash`], extra
/// [`crate::config::ScanConfig::skip_directories`], `$HOME/memex` as a source,
/// and symlinks. Does not skip `~/.agents/trash` unless that path is listed in
/// config. Does not walk `.grok` except `.grok/sessions`. Walks at most
/// [`HOME_SCAN_MAX_DEPTH`] directory levels below
/// `home` (or `scan.home_scan_max_depth`). Zips are classified from the central
/// directory. Facebook and X archive zips in one group with identical bytes are
/// skipped (same size, then full-file hash). A zip and an unpacked tree of the
/// same ChatGPT, Grok, or Telegram payload keep one copy.
pub fn ingest_home(home: &Path) -> Result<Vec<IngestReport>, Error> {
    ingest_home_with_config(home, &Config::crate_defaults())
}

/// Home scan using this config for skip lists and scan depth.
pub fn ingest_home_with_config(home: &Path, config: &Config) -> Result<Vec<IngestReport>, Error> {
    let discovered = discover_home_export_paths(home, config)?;
    let mut groups: BTreeMap<(String, String), Vec<PathBuf>> = BTreeMap::new();
    for path in discovered {
        match infer_ingest_scope(&path) {
            Ok(scope) => {
                tracing::debug!(
                    path = %path.display(),
                    service = %scope.service,
                    account = %scope.account,
                    "home scan ingest"
                );
                groups
                    .entry((scope.service, scope.account))
                    .or_default()
                    .push(path);
            }
            Err(error) => {
                tracing::debug!(
                    path = %path.display(),
                    reason = %error,
                    "skipped a path that is not a memex source"
                );
            }
        }
    }
    let mut reports = Vec::new();
    for ((service, account), paths) in groups {
        let dest = crate::config::expand_tilde(&config.memex_dir, home);
        let output = scoped_archive_path(&dest, &service, &account)?;
        reports.push(ingest_with_config(&output, &paths, Some(home), config)?);
    }
    Ok(reports)
}

impl Accumulator {
    fn from_archive(path: &Path) -> Result<Self, Error> {
        let archive = Archive::open(path)?;
        let root = archive.deserialize_root()?;
        let blob = archive.bao_blob()?;
        let mut by_id = HashMap::new();
        for (index, record) in root.conversations.iter().enumerate() {
            if let Some(id) = record
                .id
                .clone()
                .or_else(|| record.item.conversation.id.clone())
            {
                by_id.insert(id, index);
            }
        }
        let mut asset_bodies = Vec::with_capacity(root.assets.len());
        for entry in &root.assets {
            let start = usize::try_from(entry.blob_off).map_err(|_| Error::InvalidArchive)?;
            let extra = usize::try_from(entry.blob_len).map_err(|_| Error::InvalidArchive)?;
            let end = start.checked_add(extra).ok_or(Error::InvalidArchive)?;
            let body = blob.get(start..end).ok_or(Error::InvalidArchive)?.to_vec();
            asset_bodies.push(body);
        }
        let asset_digests = asset_bodies
            .par_iter()
            .map(|body| hash::hash_bytes(body))
            .collect();
        let idents = root
            .conversations
            .par_iter()
            .map(|record| identity_of(&record.item))
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(Self {
            exports: root.exports,
            conversations: root.conversations,
            by_id,
            media_posts: root.media_posts,
            projects: root.projects,
            tasks: root.tasks,
            auth: root.auth,
            billing: root.billing,
            assets: root.assets,
            asset_digests,
            asset_bodies,
            idents,
            max_uncompressed_bytes: MAX_UNCOMPRESSED,
        })
    }

    fn zip_max(&self) -> u64 {
        if self.max_uncompressed_bytes == 0 {
            MAX_UNCOMPRESSED
        } else {
            self.max_uncompressed_bytes
        }
    }

    fn ingest_backend(&mut self, backend: &Path) -> Result<(), Error> {
        let export_index = u32_fit_usize(self.exports.len(), "export count")?;
        let file = File::open(backend)?;
        let mut extra = ExtraMap::new();
        stream_backend(file, self, &mut extra, export_index)?;

        let export_dir = backend.parent().unwrap_or(Path::new("."));
        let auth_path = export_dir.join(AUTH_FILE_NAME);
        if auth_path.is_file() {
            self.auth.push(TaggedAuth {
                export_index,
                file: read_typed_file::<AuthFile>(&auth_path)?,
            });
        }
        let billing_path = export_dir.join(BILLING_FILE_NAME);
        if billing_path.is_file() {
            self.billing.push(TaggedBilling {
                export_index,
                file: read_typed_file::<BillingFile>(&billing_path)?,
            });
        }
        catalog_assets(
            export_dir,
            export_index,
            &mut self.assets,
            &mut self.asset_digests,
            &mut self.asset_bodies,
        )?;

        self.exports.push(ExportManifest {
            source_path: backend.display().to_string(),
            id: export_id(backend),
            extra,
        });
        Ok(())
    }

    fn add_conversations(
        &mut self,
        export_index: u32,
        items: Vec<ConversationItem>,
    ) -> Result<(), Error> {
        if items.is_empty() {
            return Ok(());
        }
        let idents = items
            .par_iter()
            .map(identity_of)
            .collect::<Result<Vec<_>, Error>>()?;
        for (item, ident) in items.into_iter().zip(idents) {
            self.add_conversation_with_ident(export_index, item, ident)?;
        }
        Ok(())
    }

    fn add_conversation(&mut self, export_index: u32, item: ConversationItem) -> Result<(), Error> {
        let incoming = identity_of(&item)?;
        self.add_conversation_with_ident(export_index, item, incoming)
    }

    fn add_conversation_with_ident(
        &mut self,
        export_index: u32,
        item: ConversationItem,
        incoming: ItemIdentity,
    ) -> Result<(), Error> {
        match incoming.id.as_ref() {
            None => {
                self.conversations
                    .push(new_record(export_index, &incoming, item));
                self.idents.push(incoming);
                Ok(())
            }
            Some(id) => {
                if let Some(&index) = self.by_id.get(id) {
                    if identities_match(&self.idents[index], &incoming) {
                        add_provenance(&mut self.conversations[index], export_index);
                        return Ok(());
                    }
                    let existing_item = self.conversations[index].item.clone();
                    let existing_modified = self.idents[index].modified.clone();
                    let merged =
                        enrich_item(existing_item, item, &existing_modified, &incoming.modified)?;
                    let merged_ident = identity_of(&merged)?;
                    let record = &mut self.conversations[index];
                    record.item = merged;
                    record.byte_len = merged_ident.byte_len;
                    record.modified = merged_ident.modified.clone();
                    record.id = merged_ident.id.clone();
                    self.idents[index] = merged_ident;
                    add_provenance(record, export_index);
                    Ok(())
                } else {
                    let index = self.conversations.len();
                    let id = id.clone();
                    self.conversations
                        .push(new_record(export_index, &incoming, item));
                    self.idents.push(incoming);
                    self.by_id.insert(id, index);
                    Ok(())
                }
            }
        }
    }

    fn ingest_session_dir(&mut self, dir: &Path) -> Result<(), Error> {
        let Some(chat) = session_chat_history(dir) else {
            return Err(Error::ingest(format!(
                "no grok-oss session JSONL under {}",
                dir.display()
            )));
        };
        self.ingest_session(dir, &[chat])
    }

    fn ingest_session_jsonl(&mut self, path: &Path) -> Result<(), Error> {
        let dir = path.parent().unwrap_or(Path::new("."));
        if is_chat_history_file(path) {
            self.ingest_session(dir, &[path.to_path_buf()])
        } else if let Some(chat) = session_chat_history(dir) {
            self.ingest_session(dir, &[chat])
        } else {
            self.ingest_session(dir, &[path.to_path_buf()])
        }
    }

    fn ingest_session(&mut self, session_dir: &Path, jsonl_files: &[PathBuf]) -> Result<(), Error> {
        let export_index = u32_fit_usize(self.exports.len(), "export count")?;
        let mut conversation = Conversation::default();
        let dir_id = session_dir
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        conversation.id = dir_id.clone();
        let summary_path = session_dir.join("summary.json");
        if summary_path.is_file() {
            apply_session_summary(&mut conversation, &summary_path)?;
        }
        if conversation.id.is_none() {
            conversation.id = dir_id;
        }
        let conversation_id = conversation.id.clone();
        let mut responses = Vec::new();
        for path in jsonl_files {
            read_session_jsonl(path, conversation_id.as_deref(), &mut responses)?;
        }
        let item = ConversationItem {
            conversation,
            responses,
            extra: ExtraMap::new(),
        };
        self.add_conversation(export_index, item)?;
        catalog_assets(
            session_dir,
            export_index,
            &mut self.assets,
            &mut self.asset_digests,
            &mut self.asset_bodies,
        )?;
        let source_path = jsonl_files
            .first()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| session_dir.display().to_string());
        self.exports.push(ExportManifest {
            source_path,
            id: conversation_id.filter(|id| looks_like_uuid(id)),
            extra: sibling_jsonl_leftover(session_dir, jsonl_files)?,
        });
        Ok(())
    }

    fn ingest_markdown(&mut self, root: &Path, home: &Path, config: &Config) -> Result<(), Error> {
        let files = collect_markdown_files(root, home, config)?;
        if files.is_empty() {
            return Err(Error::ingest(format!(
                "no markdown files under {}",
                root.display()
            )));
        }
        let export_index = u32_fit_usize(self.exports.len(), "export count")?;
        let id_root = if root.is_file() {
            root.parent().unwrap_or(root)
        } else {
            root
        };
        for file in &files {
            self.add_conversation(export_index, markdown_item(id_root, file)?)?;
        }
        self.exports.push(ExportManifest {
            source_path: root.display().to_string(),
            id: None,
            extra: ExtraMap::new(),
        });
        Ok(())
    }

    fn ingest_sqlite(&mut self, path: &Path) -> Result<(), Error> {
        let export_index = u32_fit_usize(self.exports.len(), "export count")?;
        ingest_sqlite::for_each_session_doc(path, |item| {
            self.add_conversation(export_index, item)
        })?;
        self.exports.push(ExportManifest {
            source_path: path.display().to_string(),
            id: None,
            extra: ExtraMap::new(),
        });
        Ok(())
    }

    fn ingest_telegram(&mut self, path: &Path) -> Result<(), Error> {
        let export_index = u32_fit_usize(self.exports.len(), "export count")?;
        let extra = ingest_telegram::for_each_telegram_conversation(path, |item| {
            self.add_conversation(export_index, item)
        })?;
        self.exports.push(ExportManifest {
            source_path: path.display().to_string(),
            id: None,
            extra,
        });
        Ok(())
    }

    fn ingest_chatgpt(&mut self, path: &Path) -> Result<(), Error> {
        let export_index = u32_fit_usize(self.exports.len(), "export count")?;
        let mut extra = ExtraMap::new();
        if path.is_dir() {
            if let Some(user) =
                ingest_chatgpt::read_user_json_file(&path.join(ingest_chatgpt::USER_FILE_NAME))?
            {
                extra.insert(ingest_chatgpt::USER_FILE_NAME.to_owned(), user);
            }
            for file in ingest_chatgpt::conversation_files_in_dir(path)? {
                let reader = File::open(&file)?;
                ingest_chatgpt::for_each_chatgpt_conversation(reader, &file, |item| {
                    self.add_conversation(export_index, item)
                })?;
            }
        } else {
            let reader = File::open(path)?;
            ingest_chatgpt::for_each_chatgpt_conversation(reader, path, |item| {
                self.add_conversation(export_index, item)
            })?;
        }
        self.exports.push(ExportManifest {
            source_path: path.display().to_string(),
            id: None,
            extra,
        });
        Ok(())
    }

    fn ingest_zip(&mut self, path: &Path) -> Result<(), Error> {
        match zip::classify_zip(path)? {
            Some(ZipKind::ChatGpt) => self.ingest_chatgpt_zip(path),
            Some(ZipKind::Grok) => self.ingest_grok_zip(path),
            Some(ZipKind::Telegram) => self.ingest_telegram_zip(path),
            Some(ZipKind::Facebook) => self.ingest_facebook_zip(path),
            Some(ZipKind::Twitter) => self.ingest_twitter_zip(path),
            None => Err(Error::ingest(format!(
                "zip is not a known export (ChatGPT conversations+user.json, Grok prod-grok-backend.json, Telegram result.json, Facebook your_facebook_activity, or X account archive): {}",
                path.display()
            ))),
        }
    }

    fn ingest_chatgpt_zip(&mut self, path: &Path) -> Result<(), Error> {
        let export_index = u32_fit_usize(self.exports.len(), "export count")?;
        let mut extra = ExtraMap::new();
        zip::with_named_entry(
            path,
            ingest_chatgpt::USER_FILE_NAME,
            self.zip_max(),
            |reader| {
                let value = ingest_chatgpt::read_user_json_atom(reader, path)?;
                extra.insert(ingest_chatgpt::USER_FILE_NAME.to_owned(), value);
                Ok(())
            },
        )?;
        zip::for_each_matching_entry(
            path,
            self.zip_max(),
            ingest_chatgpt::is_conversations_json_name,
            |name, reader| {
                ingest_chatgpt::for_each_chatgpt_conversation(reader, Path::new(name), |item| {
                    self.add_conversation(export_index, item)
                })
            },
        )?;
        self.exports.push(ExportManifest {
            source_path: path.display().to_string(),
            id: None,
            extra,
        });
        Ok(())
    }

    fn ingest_grok_zip(&mut self, path: &Path) -> Result<(), Error> {
        let export_index = u32_fit_usize(self.exports.len(), "export count")?;
        let mut extra = ExtraMap::new();
        zip::with_named_entry(path, BACKEND_FILE_NAME, self.zip_max(), |reader| {
            stream_backend(reader, self, &mut extra, export_index)
        })?
        .ok_or_else(|| {
            Error::ingest(format!(
                "zip has no {}: {}",
                BACKEND_FILE_NAME,
                path.display()
            ))
        })?;
        if let Some(file) = zip::with_named_entry(path, AUTH_FILE_NAME, self.zip_max(), |reader| {
            read_typed_reader::<AuthFile, _>(reader, path)
        })? {
            self.auth.push(TaggedAuth { export_index, file });
        }
        if let Some(file) =
            zip::with_named_entry(path, BILLING_FILE_NAME, self.zip_max(), |reader| {
                read_typed_reader::<BillingFile, _>(reader, path)
            })?
        {
            self.billing.push(TaggedBilling { export_index, file });
        }
        self.exports.push(ExportManifest {
            source_path: path.display().to_string(),
            id: None,
            extra,
        });
        Ok(())
    }

    fn ingest_facebook_dir(&mut self, path: &Path) -> Result<(), Error> {
        let export_index = u32_fit_usize(self.exports.len(), "export count")?;
        for file in ingest_facebook::json_files_in_tree(path)? {
            let label = ingest_facebook::json_label(path, &file);
            let reader = File::open(&file)?;
            ingest_facebook::for_each_facebook_item(reader, &label, |item| {
                self.add_conversation(export_index, item)
            })?;
        }
        self.catalog_facebook_images_dir(path, export_index)?;
        self.exports.push(ExportManifest {
            source_path: path.display().to_string(),
            id: None,
            extra: ExtraMap::new(),
        });
        Ok(())
    }

    fn ingest_facebook_zip(&mut self, path: &Path) -> Result<(), Error> {
        let export_index = u32_fit_usize(self.exports.len(), "export count")?;
        zip::for_each_matching_entry(
            path,
            self.zip_max(),
            ingest_facebook::is_facebook_activity_json,
            |name, reader| {
                ingest_facebook::for_each_facebook_item(reader, name, |item| {
                    self.add_conversation(export_index, item)
                })
            },
        )?;
        self.catalog_facebook_images_zip(path, export_index)?;
        self.exports.push(ExportManifest {
            source_path: path.display().to_string(),
            id: None,
            extra: ExtraMap::new(),
        });
        Ok(())
    }

    fn ingest_twitter_dir(&mut self, path: &Path) -> Result<(), Error> {
        let export_index = u32_fit_usize(self.exports.len(), "export count")?;
        let extra = twitter_username_extra_from_dir(path)?;
        for file in ingest_x::payload_files_in_tree(path)? {
            let reader = File::open(&file)?;
            ingest_x::for_each_twitter_item(reader, &file, |item| {
                self.add_conversation(export_index, item)
            })?;
        }
        self.exports.push(ExportManifest {
            source_path: path.display().to_string(),
            id: None,
            extra,
        });
        Ok(())
    }

    fn ingest_twitter_zip(&mut self, path: &Path) -> Result<(), Error> {
        let export_index = u32_fit_usize(self.exports.len(), "export count")?;
        let extra = twitter_username_extra_from_zip(path)?;
        zip::for_each_matching_entry(
            path,
            self.zip_max(),
            ingest_x::is_twitter_payload_js_name,
            |name, reader| {
                ingest_x::for_each_twitter_item(reader, Path::new(name), |item| {
                    self.add_conversation(export_index, item)
                })
            },
        )?;
        self.exports.push(ExportManifest {
            source_path: path.display().to_string(),
            id: None,
            extra,
        });
        Ok(())
    }

    fn catalog_facebook_images_zip(&mut self, path: &Path, export_index: u32) -> Result<(), Error> {
        let mut hashed = Vec::new();
        zip::for_each_matching_entry(
            path,
            self.zip_max(),
            ingest_facebook::is_facebook_image_name,
            |name, reader| {
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes)?;
                hashed.push(hashed_facebook_image(name.replace('\\', "/"), bytes, 0));
                Ok(())
            },
        )?;
        merge_hashed_assets(
            export_index,
            hashed,
            &mut self.assets,
            &mut self.asset_digests,
            &mut self.asset_bodies,
        );
        Ok(())
    }

    fn catalog_facebook_images_dir(&mut self, dir: &Path, export_index: u32) -> Result<(), Error> {
        let mut hashed = Vec::new();
        for file in ingest_facebook::image_files_in_tree(dir)? {
            let bytes = fs::read(&file)?;
            let mtime = unix_mtime(&fs::metadata(&file)?);
            hashed.push(hashed_facebook_image(relative_to(dir, &file), bytes, mtime));
        }
        merge_hashed_assets(
            export_index,
            hashed,
            &mut self.assets,
            &mut self.asset_digests,
            &mut self.asset_bodies,
        );
        Ok(())
    }

    fn ingest_telegram_zip(&mut self, path: &Path) -> Result<(), Error> {
        let export_index = u32_fit_usize(self.exports.len(), "export count")?;
        let extra = zip::with_named_entry(
            path,
            ingest_telegram::RESULT_FILE_NAME,
            self.zip_max(),
            |reader| {
                ingest_telegram::for_each_telegram_conversation_reader(reader, path, |item| {
                    self.add_conversation(export_index, item)
                })
            },
        )?
        .ok_or_else(|| {
            Error::ingest(format!(
                "zip has no {}: {}",
                ingest_telegram::RESULT_FILE_NAME,
                path.display()
            ))
        })?;
        self.exports.push(ExportManifest {
            source_path: path.display().to_string(),
            id: None,
            extra,
        });
        Ok(())
    }

    fn pack(mut self, output: &Path) -> Result<IngestReport, Error> {
        u32_fit_usize(self.conversations.len(), "conversation count")?;
        u32_fit_usize(self.assets.len(), "asset count")?;
        if self.conversations.len() != self.idents.len() {
            return Err(Error::ingest("conversation identity list diverged"));
        }
        let encodings = self
            .conversations
            .par_iter()
            .map(|record| hash::encode_item(&record.item))
            .collect::<Result<Vec<_>, Error>>()?;
        let mut blob_cap = 0usize;
        for encoded in &encodings {
            blob_cap = blob_cap.saturating_add(encoded.len());
        }
        for body in &self.asset_bodies {
            blob_cap = blob_cap.saturating_add(body.len());
        }
        let mut blob = Vec::with_capacity(blob_cap);
        for (record, encoded) in self.conversations.iter_mut().zip(encodings) {
            record.byte_len = encoded.len() as u64;
            record.blob_off = blob.len() as u64;
            record.blob_len = record.byte_len;
            record.id = record.item.conversation.id.clone();
            record.modified = record
                .item
                .conversation
                .modify_time
                .clone()
                .or_else(|| record.item.conversation.create_time.clone());
            blob.extend_from_slice(&encoded);
        }
        if self.assets.len() != self.asset_digests.len()
            || self.assets.len() != self.asset_bodies.len()
        {
            return Err(Error::ingest("asset catalog and digest lists diverged"));
        }
        for (entry, body) in self.assets.iter_mut().zip(self.asset_bodies.iter()) {
            entry.blob_off = blob.len() as u64;
            entry.blob_len = body.len() as u64;
            blob.extend_from_slice(body);
        }
        let packed = hash::pack_blob(blob);
        let (text, spans) = build_text(&self.conversations)?;
        let root = ArchiveRoot {
            exports: self.exports,
            conversations: self.conversations,
            media_posts: self.media_posts,
            projects: self.projects,
            tasks: self.tasks,
            auth: self.auth,
            billing: self.billing,
            assets: self.assets,
        };
        let report = IngestReport {
            output: output.to_path_buf(),
            exports: root.exports.len(),
            conversations: root.conversations.len(),
        };
        write_archive(output, &root, text.as_bytes(), &spans, &packed)?;
        Ok(report)
    }
}

#[derive(Clone)]
struct ItemIdentity {
    id: Option<String>,
    byte_len: u64,
    modified: Option<Timestamp>,
    root: Digest,
    sub: Digest,
}

fn identity_of(item: &ConversationItem) -> Result<ItemIdentity, Error> {
    let (encoded, sub) = rayon::join(
        || hash::encode_item(item),
        || hash::hash_responses(&item.responses),
    );
    let encoded = encoded?;
    Ok(ItemIdentity {
        id: item.conversation.id.clone(),
        byte_len: encoded.len() as u64,
        modified: item
            .conversation
            .modify_time
            .clone()
            .or_else(|| item.conversation.create_time.clone()),
        root: hash::hash_bytes(&encoded),
        sub: sub?,
    })
}

fn identities_match(existing: &ItemIdentity, incoming: &ItemIdentity) -> bool {
    existing.byte_len == incoming.byte_len
        && existing.modified == incoming.modified
        && existing.root == incoming.root
        && existing.sub == incoming.sub
}

fn new_record(
    export_index: u32,
    ident: &ItemIdentity,
    item: ConversationItem,
) -> ConversationRecord {
    ConversationRecord {
        export_index,
        export_indices: vec![export_index],
        byte_len: ident.byte_len,
        modified: ident.modified.clone(),
        id: ident.id.clone(),
        blob_off: 0,
        blob_len: 0,
        item,
    }
}

fn add_provenance(record: &mut ConversationRecord, export_index: u32) {
    if !record.export_indices.contains(&export_index) {
        record.export_indices.push(export_index);
    }
}

fn add_asset_provenance(entry: &mut AssetEntry, export_index: u32) {
    if !entry.export_indices.contains(&export_index) {
        entry.export_indices.push(export_index);
    }
}

fn enrich_item(
    mut existing: ConversationItem,
    incoming: ConversationItem,
    existing_modified: &Option<Timestamp>,
    incoming_modified: &Option<Timestamp>,
) -> Result<ConversationItem, Error> {
    let incoming_newer = is_newer(incoming_modified, existing_modified);
    existing.conversation =
        merge_conversation(existing.conversation, incoming.conversation, incoming_newer);
    existing.responses = merge_responses(existing.responses, incoming.responses)?;
    existing.extra = union_extra(existing.extra, incoming.extra, incoming_newer);
    Ok(existing)
}

fn merge_conversation(
    mut dest: Conversation,
    src: Conversation,
    src_wins_if_set: bool,
) -> Conversation {
    dest.title = pick_text(&dest.title, &src.title, src_wins_if_set);
    dest.summary = pick_text(&dest.summary, &src.summary, src_wins_if_set);
    overlay_option(&mut dest.anon_user_id, src.anon_user_id, src_wins_if_set);
    overlay_option(&mut dest.asset_ids, src.asset_ids, src_wins_if_set);
    overlay_option(&mut dest.controller, src.controller, src_wins_if_set);
    overlay_option(&mut dest.create_time, src.create_time, src_wins_if_set);
    overlay_option(&mut dest.id, src.id, src_wins_if_set);
    overlay_option(
        &mut dest.leaf_response_id,
        src.leaf_response_id,
        src_wins_if_set,
    );
    overlay_option(&mut dest.media_types, src.media_types, src_wins_if_set);
    overlay_option(&mut dest.modify_time, src.modify_time, src_wins_if_set);
    overlay_option(&mut dest.root_asset_id, src.root_asset_id, src_wins_if_set);
    overlay_option(
        &mut dest.shared_with_team,
        src.shared_with_team,
        src_wins_if_set,
    );
    overlay_option(
        &mut dest.shared_with_user_ids,
        src.shared_with_user_ids,
        src_wins_if_set,
    );
    overlay_option(&mut dest.starred, src.starred, src_wins_if_set);
    overlay_option(
        &mut dest.system_prompt_id,
        src.system_prompt_id,
        src_wins_if_set,
    );
    overlay_option(
        &mut dest.system_prompt_name,
        src.system_prompt_name,
        src_wins_if_set,
    );
    overlay_option(
        &mut dest.task_result_id,
        src.task_result_id,
        src_wins_if_set,
    );
    overlay_option(&mut dest.team_id, src.team_id, src_wins_if_set);
    overlay_option(&mut dest.temporary, src.temporary, src_wins_if_set);
    overlay_option(&mut dest.user_id, src.user_id, src_wins_if_set);
    overlay_option(&mut dest.x_user_id, src.x_user_id, src_wins_if_set);
    dest.extra = union_extra(dest.extra, src.extra, src_wins_if_set);
    dest
}

fn pick_text(
    existing: &Option<String>,
    incoming: &Option<String>,
    incoming_newer: bool,
) -> Option<String> {
    let incoming_set = nonempty(incoming);
    let existing_set = nonempty(existing);
    if incoming_newer {
        if incoming_set {
            incoming.clone()
        } else if existing_set {
            existing.clone()
        } else {
            incoming.clone().or_else(|| existing.clone())
        }
    } else {
        richer_text(existing, incoming)
    }
}

fn nonempty(value: &Option<String>) -> bool {
    value.as_ref().is_some_and(|text| !text.is_empty())
}

fn richer_text(a: &Option<String>, b: &Option<String>) -> Option<String> {
    match (a, b) {
        (Some(left), Some(right)) => {
            if right.len() > left.len() {
                Some(right.clone())
            } else {
                Some(left.clone())
            }
        }
        (Some(left), None) => Some(left.clone()),
        (None, Some(right)) => Some(right.clone()),
        (None, None) => None,
    }
}

fn overlay_option<T>(dest: &mut Option<T>, src: Option<T>, src_wins_if_set: bool) {
    if src.is_some() && (dest.is_none() || src_wins_if_set) {
        *dest = src;
    }
}

fn union_extra(mut base: ExtraMap, other: ExtraMap, other_wins: bool) -> ExtraMap {
    for (key, value) in other {
        if other_wins {
            base.insert(key, value);
        } else {
            base.entry(key).or_insert(value);
        }
    }
    base
}

fn merge_responses(
    mut dest: Vec<ResponseItem>,
    incoming: Vec<ResponseItem>,
) -> Result<Vec<ResponseItem>, Error> {
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for (index, item) in dest.iter().enumerate() {
        if let Some(id) = item.response._id.as_ref() {
            by_id.entry(id.clone()).or_insert(index);
        }
    }
    for item in incoming {
        match item.response._id.clone() {
            None => dest.push(item),
            Some(id) => {
                if let Some(&index) = by_id.get(&id) {
                    let existing_digest = hash::hash_response(&dest[index])?;
                    let incoming_digest = hash::hash_response(&item)?;
                    if existing_digest == incoming_digest {
                        dest[index].extra =
                            union_extra(dest[index].extra.clone(), item.extra, false);
                        continue;
                    }
                    let existing_len = message_len(&dest[index]);
                    let incoming_len = message_len(&item);
                    if incoming_len > existing_len {
                        let mut kept = item;
                        kept.extra = union_extra(dest[index].extra.clone(), kept.extra, true);
                        dest[index] = kept;
                    } else {
                        dest.push(item);
                    }
                } else {
                    by_id.insert(id, dest.len());
                    dest.push(item);
                }
            }
        }
    }
    Ok(dest)
}

fn message_len(item: &ResponseItem) -> usize {
    match &item.response.message {
        Some(JsonAtom::String(text)) => text.len(),
        Some(atom) => atom_len(atom),
        None => 0,
    }
}

fn atom_len(atom: &JsonAtom) -> usize {
    match atom {
        JsonAtom::Null => 0,
        JsonAtom::Bool(_) | JsonAtom::I64(_) | JsonAtom::U64(_) | JsonAtom::F64(_) => 1,
        JsonAtom::String(text) => text.len(),
        JsonAtom::Array(values) => values.iter().map(atom_len).sum(),
        JsonAtom::Object(values) => values.values().map(atom_len).sum(),
    }
}

fn is_newer(incoming: &Option<Timestamp>, existing: &Option<Timestamp>) -> bool {
    match (incoming, existing) {
        (Some(left), Some(right)) => timestamp_sort_key(left) > timestamp_sort_key(right),
        (Some(_), None) => true,
        _ => false,
    }
}

fn timestamp_sort_key(ts: &Timestamp) -> String {
    match ts {
        Timestamp::Iso(value) => value.clone(),
        Timestamp::BsonDate { date } => format!("bson:{}", atom_sort_key(date)),
        Timestamp::Other(atom) => format!("other:{}", atom_sort_key(atom)),
    }
}

fn atom_sort_key(atom: &JsonAtom) -> String {
    match atom {
        JsonAtom::String(value) => value.clone(),
        JsonAtom::I64(value) => format!("i{value}"),
        JsonAtom::U64(value) => format!("u{value}"),
        JsonAtom::F64(value) => format!("f{value}"),
        JsonAtom::Bool(value) => format!("b{value}"),
        JsonAtom::Null => "null".to_owned(),
        JsonAtom::Array(values) => values
            .iter()
            .map(atom_sort_key)
            .collect::<Vec<_>>()
            .join(","),
        JsonAtom::Object(values) => values
            .iter()
            .map(|(key, value)| format!("{key}={}", atom_sort_key(value)))
            .collect::<Vec<_>>()
            .join(","),
    }
}

fn stream_backend<R: Read>(
    reader: R,
    acc: &mut Accumulator,
    extra: &mut ExtraMap,
    export_index: u32,
) -> Result<(), Error> {
    let reader = BufReader::with_capacity(256 * 1024, reader);
    let mut de = serde_json::Deserializer::from_reader(reader);
    serde::Deserializer::deserialize_map(
        &mut de,
        BackendVisitor {
            acc,
            extra,
            export_index,
        },
    )?;
    de.end()?;
    Ok(())
}

fn read_typed_file<T>(path: &Path) -> Result<T, Error>
where
    T: for<'de> Deserialize<'de>,
{
    let file = File::open(path)?;
    read_typed_reader(file, path)
}

fn read_typed_reader<T, R: Read>(reader: R, label: &Path) -> Result<T, Error>
where
    T: for<'de> Deserialize<'de>,
{
    let mut de = serde_json::Deserializer::from_reader(BufReader::new(reader));
    let value = T::deserialize(&mut de)
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    de.end()
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    Ok(value)
}

struct BackendVisitor<'a> {
    acc: &'a mut Accumulator,
    extra: &'a mut ExtraMap,
    export_index: u32,
}

impl<'de> Visitor<'de> for BackendVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a Grok backend export object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "conversations" => {
                    map.next_value_seed(SeqSink {
                        acc: self.acc,
                        export_index: self.export_index,
                        kind: 1,
                    })?;
                }
                "media_posts" => {
                    map.next_value_seed(SeqSink {
                        acc: self.acc,
                        export_index: self.export_index,
                        kind: SPILL_MEDIA,
                    })?;
                }
                "projects" => {
                    map.next_value_seed(SeqSink {
                        acc: self.acc,
                        export_index: self.export_index,
                        kind: SPILL_PROJECT,
                    })?;
                }
                "tasks" => {
                    map.next_value_seed(SeqSink {
                        acc: self.acc,
                        export_index: self.export_index,
                        kind: SPILL_TASK,
                    })?;
                }
                _ => {
                    let value = map.next_value::<JsonAtom>()?;
                    self.extra.insert(key, value);
                }
            }
        }
        Ok(())
    }
}

struct SeqSink<'a> {
    acc: &'a mut Accumulator,
    export_index: u32,
    kind: u8,
}

impl<'de> DeserializeSeed<'de> for SeqSink<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for SeqSink<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        match self.kind {
            1 => {
                let mut batch = Vec::new();
                if let Some(hint) = seq.size_hint() {
                    batch.reserve(hint.min(CONVERSATION_IDENTITY_BATCH));
                }
                while let Some(item) = seq.next_element::<ConversationItem>()? {
                    batch.push(item);
                    if batch.len() >= CONVERSATION_IDENTITY_BATCH {
                        self.acc
                            .add_conversations(self.export_index, std::mem::take(&mut batch))
                            .map_err(de::Error::custom)?;
                    }
                }
                if !batch.is_empty() {
                    self.acc
                        .add_conversations(self.export_index, batch)
                        .map_err(de::Error::custom)?;
                }
            }
            SPILL_MEDIA => {
                while let Some(item) = seq.next_element::<JsonAtom>()? {
                    self.acc.media_posts.push(TaggedJson {
                        export_index: self.export_index,
                        value: item,
                    });
                }
            }
            SPILL_PROJECT => {
                while let Some(item) = seq.next_element::<JsonAtom>()? {
                    self.acc.projects.push(TaggedJson {
                        export_index: self.export_index,
                        value: item,
                    });
                }
            }
            SPILL_TASK => {
                while let Some(item) = seq.next_element::<JsonAtom>()? {
                    self.acc.tasks.push(TaggedJson {
                        export_index: self.export_index,
                        value: item,
                    });
                }
            }
            _ => {
                return Err(de::Error::custom("unknown array kind"));
            }
        }
        Ok(())
    }
}

fn discover_sources(
    input: &Path,
    home: &Path,
    config: &Config,
) -> Result<Vec<IngestSource>, Error> {
    let meta = fs::metadata(input)?;
    if meta.is_file() {
        if zip::is_zip_path(input) {
            return Ok(vec![IngestSource::Zip(input.to_path_buf())]);
        }
        if is_session_sqlite_path(input) {
            return Ok(vec![IngestSource::Sqlite(input.to_path_buf())]);
        }
        if is_jsonl(input) {
            return Ok(vec![IngestSource::SessionJsonl(input.to_path_buf())]);
        }
        if is_markdown_file(input) {
            return Ok(vec![IngestSource::Markdown(input.to_path_buf())]);
        }
        if ingest_telegram::is_result_json_name(input) {
            if ingest_telegram::telegram_meta(input)?.is_some()
                || ingest_telegram::is_chatexport_result_json(input)
            {
                return Ok(vec![IngestSource::Telegram(input.to_path_buf())]);
            }
            return Err(Error::ingest(format!(
                "{} is named result.json but is not a Telegram Desktop export",
                input.display()
            )));
        }
        if ingest_chatgpt::is_conversations_json_name(
            input
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(""),
        ) || ingest_chatgpt::is_user_json_name(
            input
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(""),
        ) {
            if let Some(parent) = input
                .parent()
                .filter(|parent| ingest_chatgpt::is_chatgpt_dir(parent))
            {
                return Ok(vec![IngestSource::ChatGpt(parent.to_path_buf())]);
            }
            if ingest_chatgpt::is_conversations_json_name(
                input
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(""),
            ) {
                return Ok(vec![IngestSource::ChatGpt(input.to_path_buf())]);
            }
        }
        return Ok(vec![IngestSource::Backend(input.to_path_buf())]);
    }
    if !meta.is_dir() {
        return Err(Error::ingest(format!(
            "not a file or directory: {}",
            input.display()
        )));
    }

    let grok = find_grok_backends(input, home, config)?;
    if !grok.is_empty() {
        return Ok(grok.into_iter().map(IngestSource::Backend).collect());
    }
    if ingest_chatgpt::is_chatgpt_dir(input) {
        return Ok(vec![IngestSource::ChatGpt(input.to_path_buf())]);
    }
    if ingest_facebook::is_facebook_dir(input) {
        return Ok(vec![IngestSource::Facebook(input.to_path_buf())]);
    }
    if ingest_x::is_twitter_dir(input) {
        return Ok(vec![IngestSource::Twitter(input.to_path_buf())]);
    }
    if is_obsidian_vault(input) {
        return Ok(vec![IngestSource::Markdown(input.to_path_buf())]);
    }
    if let Some(telegram) = telegram_result_in_dir(input)? {
        return Ok(vec![IngestSource::Telegram(telegram)]);
    }
    if is_agents_reports_dir(input) {
        return Ok(vec![IngestSource::Markdown(input.to_path_buf())]);
    }
    if dir_has_markdown(input, home, config)? {
        return Ok(vec![IngestSource::Markdown(input.to_path_buf())]);
    }

    let chat = input.join(CHAT_HISTORY_FILE_NAME);
    if chat.is_file() {
        return Ok(vec![IngestSource::SessionDir(input.to_path_buf())]);
    }

    let mut sources = Vec::new();
    walk_sources(input, WALK_DEPTH, home, config, &mut sources)?;
    Ok(sources)
}

fn collect_uuid_backends(export_data: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    for entry in fs::read_dir(export_data)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let backend = entry.path().join(BACKEND_FILE_NAME);
        if backend.is_file() {
            out.push(backend);
        }
    }
    Ok(())
}

fn walk_sources(
    dir: &Path,
    depth: u32,
    home: &Path,
    config: &Config,
    out: &mut Vec<IngestSource>,
) -> Result<(), Error> {
    if depth == 0 {
        return Ok(());
    }
    let backend = dir.join(BACKEND_FILE_NAME);
    if backend.is_file() {
        out.push(IngestSource::Backend(backend));
        return Ok(());
    }
    let chat = dir.join(CHAT_HISTORY_FILE_NAME);
    if chat.is_file() {
        out.push(IngestSource::SessionDir(dir.to_path_buf()));
        return Ok(());
    }
    if ingest_chatgpt::is_chatgpt_dir(dir) {
        out.push(IngestSource::ChatGpt(dir.to_path_buf()));
        return Ok(());
    }
    if ingest_facebook::is_facebook_dir(dir) {
        out.push(IngestSource::Facebook(dir.to_path_buf()));
        return Ok(());
    }
    if ingest_x::is_twitter_dir(dir) {
        out.push(IngestSource::Twitter(dir.to_path_buf()));
        return Ok(());
    }
    if let Some(telegram) = telegram_result_in_dir(dir)? {
        out.push(IngestSource::Telegram(telegram));
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_file() {
            if dump_skips_path(&path, home, config) {
                continue;
            }
            if zip::is_zip_path(&path) && zip::classify_zip(&path)?.is_some() {
                out.push(IngestSource::Zip(path));
            }
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if name == "assets" || name == ASSET_SERVER_DIR_NAME {
            continue;
        }
        if skip_system_trash_walk_name(&name) {
            continue;
        }
        if dump_skips_path(&path, home, config) {
            continue;
        }
        walk_sources(&path, depth - 1, home, config, out)?;
    }
    Ok(())
}

fn is_jsonl(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
}

/// Infer `agents/grok` plus `user.xUsername` from one official Grok export path.
pub fn infer_grok_export_scope(input: &Path) -> Result<InferredScope, Error> {
    let account = infer_account_from_backends(&find_grok_backends(
        input,
        Path::new(""),
        &Config::crate_defaults(),
    )?)?;
    Ok(InferredScope {
        service: GROK_SERVICE.to_owned(),
        account,
    })
}

/// Infer service folder and account stem from one input path.
///
/// Detection order: ChatGPT zip or dir, Facebook DYI zip or
/// `your_facebook_activity/` tree, X account archive zip or dir, official
/// Grok dump (file, dir, or zip), Telegram `result.json` (file, dir, or zip),
/// Obsidian (`.obsidian/`), session_docs sqlite, `.agents/reports`, markdown
/// tree. A dump directory that is not itself a known root still infers from
/// nested known export shapes (the same walk as explicit ingest). Session
/// JSONL still needs flags.
pub fn infer_ingest_scope(input: &Path) -> Result<InferredScope, Error> {
    infer_ingest_scope_in(input, Path::new(""), &Config::crate_defaults())
}

fn infer_ingest_scope_in(
    input: &Path,
    home: &Path,
    config: &Config,
) -> Result<InferredScope, Error> {
    match infer_ingest_scope_at_path(input, home, config) {
        Ok(scope) => Ok(scope),
        Err(Error::NotGrokExport) => infer_ingest_scope_from_discovered(input, home, config),
        Err(error) => Err(error),
    }
}

fn infer_ingest_scope_at_path(
    input: &Path,
    home: &Path,
    config: &Config,
) -> Result<InferredScope, Error> {
    if let Some(scope) = infer_chatgpt_scope(input)? {
        return Ok(scope);
    }
    if input.is_file() && zip::is_zip_path(input) {
        return infer_zip_scope(input);
    }
    if let Some(scope) = infer_facebook_dir_scope(input)? {
        return Ok(scope);
    }
    if let Some(scope) = infer_twitter_dir_scope(input)? {
        return Ok(scope);
    }
    if input.is_file() && is_backend_file_name(input) {
        return infer_grok_export_scope(input);
    }
    if let Some(scope) = infer_telegram_scope(input)? {
        return Ok(scope);
    }
    let grok = find_grok_backends(input, home, config)?;
    if !grok.is_empty() {
        let account = infer_account_from_backends(&grok)?;
        return Ok(InferredScope {
            service: GROK_SERVICE.to_owned(),
            account,
        });
    }
    if input.is_dir() && is_obsidian_vault(input) {
        return Ok(InferredScope {
            service: OBSIDIAN_SERVICE.to_owned(),
            account: dir_account_name(input)?,
        });
    }
    if input.is_file() && is_session_sqlite_path(input) {
        return Ok(InferredScope {
            service: GROK_OSS_SERVICE.to_owned(),
            account: process_user_account()?,
        });
    }
    if input.is_dir() && is_agents_reports_dir(input) {
        return Ok(InferredScope {
            service: REPORTS_SERVICE.to_owned(),
            account: process_user_account()?,
        });
    }
    if is_markdown_input(input, home, config)? {
        return Ok(InferredScope {
            service: MARKDOWN_SERVICE.to_owned(),
            account: dir_account_name(input)?,
        });
    }
    Err(Error::NotGrokExport)
}

/// Infer from nested known export shapes under a dump directory.
///
/// Honors [`crate::config::ScanConfig::skip_directories`]. A skipped Grok
/// tree does not force [`Error::NotGrokExport`] when remaining files are
/// Telegram, markdown, or another known shape. Uninferable nested paths
/// (session JSONL) are skipped so walk order does not fail the dump.
fn infer_ingest_scope_from_discovered(
    input: &Path,
    home: &Path,
    config: &Config,
) -> Result<InferredScope, Error> {
    let sources = discover_sources(input, home, config)?;
    let mut inferred: Option<InferredScope> = None;
    for source in sources {
        let path = source.path();
        if dump_skips_path(path, home, config) {
            continue;
        }
        let next = match infer_ingest_scope_at_path(path, home, config) {
            Ok(scope) => scope,
            Err(Error::NotGrokExport) => continue,
            Err(error) => return Err(error),
        };
        match &inferred {
            None => inferred = Some(next),
            Some(existing)
                if existing.service == next.service && existing.account == next.account => {}
            Some(_) => return Err(Error::AccountDisagree),
        }
    }
    inferred.ok_or(Error::NotGrokExport)
}

/// Ingest archive path under `home/memex`.
///
/// `-o` is applied by the CLI before this function. `--service` and `--account`
/// override inferred values. Omitted flags infer from input shape (Grok dump,
/// Telegram `result.json`, Obsidian, session_docs sqlite, agent reports,
/// markdown). Session JSONL still needs those flags or `-o`. Home scan does
/// not use this function; it infers per source.
pub fn resolve_ingest_archive(
    home: &Path,
    service: Option<&str>,
    account: Option<&str>,
    inputs: &[PathBuf],
) -> Result<PathBuf, Error> {
    resolve_ingest_archive_with(
        &crate::default_memex_dir(home),
        service,
        account,
        inputs,
        home,
        &Config::crate_defaults(),
    )
}

/// Same as [`resolve_ingest_archive`], with an explicit data directory.
pub fn resolve_ingest_archive_in(
    memex_dir: &Path,
    service: Option<&str>,
    account: Option<&str>,
    inputs: &[PathBuf],
) -> Result<PathBuf, Error> {
    resolve_ingest_archive_with(
        memex_dir,
        service,
        account,
        inputs,
        Path::new(""),
        &Config::crate_defaults(),
    )
}

fn resolve_ingest_archive_with(
    memex_dir: &Path,
    service: Option<&str>,
    account: Option<&str>,
    inputs: &[PathBuf],
    home: &Path,
    config: &Config,
) -> Result<PathBuf, Error> {
    match (service, account) {
        (Some(service), Some(account)) => scoped_archive_path(memex_dir, service, account),
        (service, account) => {
            let inferred = infer_ingest_scope_from_inputs(inputs, service, account, home, config)?;
            scoped_archive_path(memex_dir, &inferred.service, &inferred.account)
        }
    }
}

fn infer_ingest_scope_from_inputs(
    inputs: &[PathBuf],
    service: Option<&str>,
    account: Option<&str>,
    home: &Path,
    config: &Config,
) -> Result<InferredScope, Error> {
    if inputs.is_empty() {
        return Err(Error::ingest("no input paths"));
    }
    let mut inferred: Option<InferredScope> = None;
    for input in inputs {
        let next = match infer_ingest_scope_in(input, home, config) {
            Ok(scope) => InferredScope {
                service: service.unwrap_or(&scope.service).to_owned(),
                account: account.unwrap_or(&scope.account).to_owned(),
            },
            Err(error) => {
                let grok_shaped = find_grok_backends(input, home, config)
                    .is_ok_and(|backends| !backends.is_empty());
                match (grok_shaped, account) {
                    (true, Some(account)) => InferredScope {
                        service: service.unwrap_or(GROK_SERVICE).to_owned(),
                        account: account.to_owned(),
                    },
                    _ => return Err(error),
                }
            }
        };
        match &inferred {
            None => inferred = Some(next),
            Some(existing)
                if existing.service == next.service && existing.account == next.account => {}
            Some(_) => return Err(Error::AccountDisagree),
        }
    }
    inferred.ok_or(Error::NotGrokExport)
}

fn infer_chatgpt_scope(input: &Path) -> Result<Option<InferredScope>, Error> {
    if input.is_file() && zip::is_zip_path(input) {
        if zip::classify_zip(input)? == Some(ZipKind::ChatGpt) {
            return Ok(Some(chatgpt_inferred_scope()?));
        }
        return Ok(None);
    }
    if input.is_dir() && ingest_chatgpt::is_chatgpt_dir(input) {
        return Ok(Some(chatgpt_inferred_scope()?));
    }
    if input.is_file() {
        let name = input
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if (ingest_chatgpt::is_conversations_json_name(name)
            || ingest_chatgpt::is_user_json_name(name))
            && (input.parent().is_some_and(ingest_chatgpt::is_chatgpt_dir)
                || ingest_chatgpt::is_conversations_json_name(name))
        {
            return Ok(Some(chatgpt_inferred_scope()?));
        }
    }
    Ok(None)
}

fn chatgpt_inferred_scope() -> Result<InferredScope, Error> {
    Ok(InferredScope {
        service: CHATGPT_SERVICE.to_owned(),
        account: process_user_account()?,
    })
}

fn infer_zip_scope(input: &Path) -> Result<InferredScope, Error> {
    match zip::classify_zip(input)? {
        Some(ZipKind::ChatGpt) => chatgpt_inferred_scope(),
        Some(ZipKind::Facebook) => facebook_inferred_scope(input),
        Some(ZipKind::Twitter) => infer_twitter_zip_scope(input),
        Some(ZipKind::Grok) => infer_grok_zip_scope(input),
        Some(ZipKind::Telegram) => infer_telegram_zip_scope(input)?
            .ok_or_else(|| Error::ingest("zip result.json is not a Telegram Desktop export")),
        None => Err(Error::NotGrokExport),
    }
}

fn infer_facebook_dir_scope(input: &Path) -> Result<Option<InferredScope>, Error> {
    if !ingest_facebook::is_facebook_dir(input) {
        return Ok(None);
    }
    Ok(Some(facebook_inferred_scope(input)?))
}

fn facebook_inferred_scope(input: &Path) -> Result<InferredScope, Error> {
    let account = ingest_facebook::account_from_path(input)
        .filter(|name| !name.is_empty() && !name.contains('@'))
        .map_or_else(process_user_account, Ok)?;
    Ok(InferredScope {
        service: META_SERVICE.to_owned(),
        account,
    })
}

fn infer_twitter_dir_scope(input: &Path) -> Result<Option<InferredScope>, Error> {
    if !ingest_x::is_twitter_dir(input) {
        return Ok(None);
    }
    Ok(Some(twitter_inferred_scope_from_username(
        ingest_x::account_file_in_tree(input)
            .map(|path| ingest_x::username_from_file(&path))
            .transpose()?
            .flatten(),
    )?))
}

fn infer_twitter_zip_scope(input: &Path) -> Result<InferredScope, Error> {
    let username = zip::with_named_entry(
        input,
        ingest_x::ACCOUNT_FILE_NAME,
        MAX_UNCOMPRESSED,
        |reader| ingest_x::username_from_reader(reader, input),
    )?
    .flatten();
    twitter_inferred_scope_from_username(username)
}

fn twitter_inferred_scope_from_username(username: Option<String>) -> Result<InferredScope, Error> {
    let account = match username {
        Some(name) if !name.is_empty() && !name.contains('@') => name,
        _ => process_user_account()?,
    };
    Ok(InferredScope {
        service: X_SERVICE.to_owned(),
        account,
    })
}

fn twitter_username_extra_from_dir(path: &Path) -> Result<ExtraMap, Error> {
    let mut extra = ExtraMap::new();
    if let Some(account) = ingest_x::account_file_in_tree(path)
        && let Some(username) = ingest_x::username_from_file(&account)?
    {
        extra.insert("username".to_owned(), JsonAtom::String(username));
    }
    Ok(extra)
}

fn twitter_username_extra_from_zip(path: &Path) -> Result<ExtraMap, Error> {
    let mut extra = ExtraMap::new();
    if let Some(username) = zip::with_named_entry(
        path,
        ingest_x::ACCOUNT_FILE_NAME,
        MAX_UNCOMPRESSED,
        |reader| ingest_x::username_from_reader(reader, path),
    )?
    .flatten()
    {
        extra.insert("username".to_owned(), JsonAtom::String(username));
    }
    Ok(extra)
}

fn infer_grok_zip_scope(input: &Path) -> Result<InferredScope, Error> {
    let account = zip::with_named_entry(input, AUTH_FILE_NAME, MAX_UNCOMPRESSED, |reader| {
        let file = read_typed_reader::<AuthFile, _>(reader, input)?;
        x_username_from_auth(&file)
            .map(str::to_owned)
            .ok_or(Error::MissingXUsername)
    })?
    .ok_or(Error::MissingXUsername)?;
    Ok(InferredScope {
        service: GROK_SERVICE.to_owned(),
        account,
    })
}

fn infer_telegram_zip_scope(input: &Path) -> Result<Option<InferredScope>, Error> {
    let Some(meta) = zip::with_named_entry(
        input,
        ingest_telegram::RESULT_FILE_NAME,
        MAX_UNCOMPRESSED,
        |reader| ingest_telegram::telegram_meta_from_reader(reader, input),
    )?
    .flatten() else {
        return Ok(None);
    };
    let account = match meta.username {
        Some(name) if !name.is_empty() => name,
        _ => process_user_account()?,
    };
    Ok(Some(InferredScope {
        service: TELEGRAM_SERVICE.to_owned(),
        account,
    }))
}

fn infer_telegram_scope(input: &Path) -> Result<Option<InferredScope>, Error> {
    let path = if input.is_file() {
        input.to_path_buf()
    } else if input.is_dir() {
        input.join(ingest_telegram::RESULT_FILE_NAME)
    } else {
        return Ok(None);
    };
    let meta = ingest_telegram::telegram_meta(&path)?;
    if meta.is_none() && !ingest_telegram::is_chatexport_result_json(&path) {
        return Ok(None);
    }
    let account = match meta.and_then(|meta| meta.username) {
        Some(name) if !name.is_empty() => name,
        _ => process_user_account()?,
    };
    Ok(Some(InferredScope {
        service: TELEGRAM_SERVICE.to_owned(),
        account,
    }))
}

fn telegram_result_in_dir(dir: &Path) -> Result<Option<PathBuf>, Error> {
    let path = dir.join(ingest_telegram::RESULT_FILE_NAME);
    if ingest_telegram::telegram_meta(&path)?.is_some() {
        return Ok(Some(path));
    }
    let dir_name = dir.file_name().and_then(|name| name.to_str()).unwrap_or("");
    if ingest_telegram::is_chatexport_dir_name(dir_name) && path.is_file() {
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

/// Known export roots under `home` (same walk as [`ingest_home`]).
///
/// Files: official Grok `prod-grok-backend.json`, Telegram `result.json`,
/// `session_search.sqlite` with `session_docs`, known export zips (including
/// Facebook DYI and X account archives). Directories: ChatGPT (`user.json`
/// plus `conversations-*.json`), Facebook `your_facebook_activity/`, X
/// `data/account.js` trees, Obsidian vaults, and `.agents/reports`. Does not
/// walk `$HOME/memex`, does not follow symlinks, and stops at
/// [`HOME_SCAN_MAX_DEPTH`] (or `scan.home_scan_max_depth`).
pub(crate) fn discover_home_export_paths(
    home: &Path,
    config: &Config,
) -> Result<Vec<PathBuf>, Error> {
    crate::logging::reset_permission_denied_count();
    let mut out = Vec::new();
    walk_home_scan(
        home,
        home,
        config.scan.home_scan_max_depth,
        config,
        &mut out,
    )?;
    crate::logging::emit_permission_denied_summary();
    out.sort();
    out.dedup();
    Ok(out)
}

fn walk_home_scan(
    home: &Path,
    dir: &Path,
    depth: u32,
    config: &Config,
    out: &mut Vec<PathBuf>,
) -> Result<(), Error> {
    if is_obsidian_vault(dir) {
        out.push(dir.to_path_buf());
        return Ok(());
    }
    if is_agents_reports_dir(dir) {
        out.push(dir.to_path_buf());
        return Ok(());
    }
    if ingest_chatgpt::is_chatgpt_dir(dir) {
        out.push(dir.to_path_buf());
        return Ok(());
    }
    if ingest_facebook::is_facebook_dir(dir) {
        out.push(dir.to_path_buf());
        return Ok(());
    }
    if ingest_x::is_twitter_dir(dir) {
        out.push(dir.to_path_buf());
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
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
        let path = entry.path();
        let name = entry.file_name();
        // Skip by name/path before file_type, open, or read_dir of this child.
        if let Some(reason) = home_scan_skip_reason(home, &path, config) {
            crate::logging::log_expected_skip(&path, reason);
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                crate::logging::log_io_on_walk(&path, &err);
                continue;
            }
        };
        if file_type.is_symlink() {
            crate::logging::log_expected_skip(&path, "symlink");
            continue;
        }
        if file_type.is_dir() {
            if name == ".grok" {
                crate::logging::log_expected_skip(
                    &path,
                    "only session sqlite under .grok is a source",
                );
                if depth == 0 {
                    continue;
                }
                let sessions = path.join("sessions");
                match fs::symlink_metadata(&sessions) {
                    Ok(meta) if meta.file_type().is_dir() => {
                        walk_home_scan(home, &sessions, depth - 1, config, out)?;
                    }
                    Ok(_) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => crate::logging::log_io_on_walk(&sessions, &err),
                }
                continue;
            }
            if depth == 0 {
                continue;
            }
            walk_home_scan(home, &path, depth - 1, config, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if name == BACKEND_FILE_NAME {
            out.push(path);
            continue;
        }
        if name == ingest_telegram::RESULT_FILE_NAME {
            out.push(path);
            continue;
        }
        if name == SESSION_SEARCH_SQLITE_NAME {
            if is_home_scan_session_sqlite(&path) {
                out.push(path);
            } else {
                crate::logging::log_expected_skip(
                    &path,
                    "empty sqlite file or no session_docs table",
                );
            }
            continue;
        }
        if zip::is_zip_path(&path) {
            match zip::classify_zip(&path) {
                Ok(Some(_)) => out.push(path),
                Ok(None) => {}
                Err(error) => {
                    crate::logging::log_error_on_path(&path, &error);
                }
            }
        }
    }
    Ok(())
}

/// `$XDG_DATA_HOME` only when this scan `home` is the process `$HOME`.
fn scan_xdg_data_home(home: &Path) -> Option<PathBuf> {
    let process_home = std::env::var_os("HOME").filter(|value| !value.is_empty())?;
    if Path::new(&process_home) != home {
        return None;
    }
    std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home_scan_skip_reason(home: &Path, path: &Path, config: &Config) -> Option<&'static str> {
    let dest = crate::config::expand_tilde(&config.memex_dir, home);
    if path == dest || path.starts_with(&dest) {
        return Some("$HOME/memex is the archive destination, not a source");
    }
    if path.starts_with(home.join(".config")) {
        return Some("directory named .config");
    }
    if path.starts_with(home.join(".local").join("share").join("lutris")) {
        return Some("Lutris data directory");
    }
    if path.starts_with(home.join(".local").join("share").join("Steam")) {
        return Some("Steam data directory");
    }
    if path.starts_with(home.join(".local").join("share").join("TelegramDesktop")) {
        return Some("Telegram Desktop tdata directory, not a JSON export");
    }
    if path.starts_with(home.join("majestic").join("tests")) {
        return Some("crate test fixtures");
    }
    let xdg_data_home = scan_xdg_data_home(home);
    if config.skips_directory(path, home, xdg_data_home.as_deref()) {
        return Some("skip_directories or system trash");
    }
    let name = path.file_name()?;
    if is_sandbox_blocked_name(name) {
        return Some("Grok OSS blocked sandbox directory");
    }
    let name = name.to_str()?;
    match name {
        ".git" => Some("directory named .git"),
        "node_modules" => Some("directory named node_modules"),
        "target" => Some("directory named target"),
        ".cache" => Some("directory named .cache"),
        ".cargo" => Some("directory named .cargo"),
        ".rustup" => Some("directory named .rustup"),
        ".npm" => Some("directory named .npm"),
        ".nvm" => Some("directory named .nvm"),
        "Steam" => Some("directory named Steam"),
        "lutris" => Some("Lutris data directory"),
        "nix" => Some("directory named nix"),
        "proc" => Some("directory named proc"),
        ".config" => Some("directory named .config"),
        _ => None,
    }
}

fn is_home_scan_session_sqlite(path: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.file_type().is_file() || meta.len() == 0 {
        return false;
    }
    ingest_sqlite::has_session_docs_table(path)
}

fn find_grok_backends(input: &Path, home: &Path, config: &Config) -> Result<Vec<PathBuf>, Error> {
    let meta = fs::metadata(input)?;
    if meta.is_file() {
        if is_backend_file_name(input) {
            return Ok(vec![input.to_path_buf()]);
        }
        return Ok(Vec::new());
    }
    if !meta.is_dir() {
        return Ok(Vec::new());
    }
    let direct = input.join(BACKEND_FILE_NAME);
    if direct.is_file() {
        return Ok(vec![direct]);
    }
    let export_data = input.join("ttl/30d/export_data");
    if export_data.is_dir() {
        let mut backends = Vec::new();
        collect_uuid_backends(&export_data, &mut backends)?;
        if !backends.is_empty() {
            return Ok(backends);
        }
    }
    let mut backends = Vec::new();
    walk_grok_backends(input, WALK_DEPTH, home, config, &mut backends)?;
    Ok(backends)
}

fn walk_grok_backends(
    dir: &Path,
    depth: u32,
    home: &Path,
    config: &Config,
    out: &mut Vec<PathBuf>,
) -> Result<(), Error> {
    if depth == 0 {
        return Ok(());
    }
    let backend = dir.join(BACKEND_FILE_NAME);
    if backend.is_file() {
        out.push(backend);
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if is_sandbox_blocked_name(&name) || name == "assets" || name == ASSET_SERVER_DIR_NAME {
            continue;
        }
        let path = entry.path();
        if dump_skips_path(&path, home, config) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        walk_grok_backends(&path, depth - 1, home, config, out)?;
    }
    Ok(())
}

fn is_backend_file_name(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some(BACKEND_FILE_NAME)
}

fn infer_account_from_backends(backends: &[PathBuf]) -> Result<String, Error> {
    let mut account: Option<String> = None;
    for backend in backends {
        let name = x_username_for_backend(backend)?;
        match &account {
            None => account = Some(name),
            Some(existing) if existing == &name => {}
            Some(_) => return Err(Error::AccountDisagree),
        }
    }
    account.ok_or(Error::NotGrokExport)
}

fn x_username_for_backend(backend: &Path) -> Result<String, Error> {
    let auth_path = backend
        .parent()
        .unwrap_or(Path::new("."))
        .join(AUTH_FILE_NAME);
    if !auth_path.is_file() {
        return Err(Error::MissingXUsername);
    }
    let file = read_typed_file::<AuthFile>(&auth_path)?;
    x_username_from_auth(&file)
        .map(str::to_owned)
        .ok_or(Error::MissingXUsername)
}

fn x_username_from_auth(file: &AuthFile) -> Option<&str> {
    let Some(JsonAtom::Object(user)) = file.user.as_ref() else {
        return None;
    };
    // Account stem is user.xUsername. sessionTierId is leftover, not personal/business.
    match user.get("xUsername") {
        Some(JsonAtom::String(name)) if !name.is_empty() => Some(name.as_str()),
        _ => None,
    }
}

fn is_chat_history_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(CHAT_HISTORY_FILE_NAME))
}

fn session_chat_history(session_dir: &Path) -> Option<PathBuf> {
    let chat = session_dir.join(CHAT_HISTORY_FILE_NAME);
    chat.is_file().then_some(chat)
}

fn sibling_jsonl_leftover(session_dir: &Path, ingested: &[PathBuf]) -> Result<ExtraMap, Error> {
    let mut extra = ExtraMap::new();
    let Ok(entries) = fs::read_dir(session_dir) else {
        return Ok(extra);
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if !is_jsonl(&path) || is_chat_history_file(&path) {
            continue;
        }
        if ingested.iter().any(|ingested| ingested == &path) {
            continue;
        }
        paths.push(path);
    }
    paths.sort();
    for path in paths {
        let key = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("jsonl")
            .to_owned();
        extra.insert(key, read_jsonl_atoms(&path)?);
    }
    Ok(extra)
}

fn read_jsonl_atoms(path: &Path) -> Result<JsonAtom, Error> {
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(256 * 1024, file);
    let mut lines = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: JsonAtom = serde_json::from_str(trimmed).map_err(|error| {
            Error::ingest(format!(
                "failed to parse {} line {}: {error}",
                path.display(),
                index + 1
            ))
        })?;
        lines.push(value);
    }
    Ok(JsonAtom::Array(lines))
}

#[derive(Deserialize)]
struct SessionSummaryFile {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    generated_title: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    session_summary: Option<String>,
    #[serde(flatten)]
    extra: ExtraMap,
}

fn apply_session_summary(conversation: &mut Conversation, path: &Path) -> Result<(), Error> {
    let parsed: SessionSummaryFile = read_typed_file(path)?;
    if parsed.id.is_some() {
        conversation.id = parsed.id;
    }
    let title = parsed.generated_title.or(parsed.title);
    if nonempty(&title) {
        conversation.title = title;
    }
    if nonempty(&parsed.session_summary) {
        conversation.summary = parsed.session_summary;
    }
    conversation.extra = union_extra(conversation.extra.clone(), parsed.extra, true);
    Ok(())
}

#[derive(Deserialize)]
struct SessionJsonlLine {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    content: Option<JsonAtom>,
    #[serde(rename = "_id", default)]
    line_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(flatten)]
    extra: ExtraMap,
}

fn read_session_jsonl(
    path: &Path,
    conversation_id: Option<&str>,
    responses: &mut Vec<ResponseItem>,
) -> Result<(), Error> {
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(256 * 1024, file);
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed: SessionJsonlLine = serde_json::from_str(trimmed).map_err(|error| {
            Error::ingest(format!(
                "failed to parse {} line {}: {error}",
                path.display(),
                index + 1
            ))
        })?;
        responses.push(session_line_to_response(parsed, conversation_id));
    }
    Ok(())
}

fn session_line_to_response(line: SessionJsonlLine, conversation_id: Option<&str>) -> ResponseItem {
    ResponseItem {
        response: Response {
            _id: line.line_id.or(line.id),
            conversation_id: conversation_id.map(str::to_owned),
            message: line.content,
            model: line.model.or(line.model_id),
            sender: line.kind,
            extra: line.extra,
            ..Default::default()
        },
        share_link: None,
        extra: ExtraMap::new(),
    }
}

fn export_id(backend: &Path) -> Option<String> {
    let parent = backend.parent()?.file_name()?.to_str()?;
    if looks_like_uuid(parent) {
        Some(parent.to_owned())
    } else {
        None
    }
}

fn looks_like_uuid(name: &str) -> bool {
    let bytes = name.as_bytes();
    match bytes.len() {
        36 => {
            bytes[8] == b'-'
                && bytes[13] == b'-'
                && bytes[18] == b'-'
                && bytes[23] == b'-'
                && name.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        }
        32 => name.chars().all(|c| c.is_ascii_hexdigit()),
        _ => false,
    }
}

struct PendingAsset {
    uuid: String,
    relative_path: String,
    content: PathBuf,
}

struct HashedAsset {
    uuid: String,
    relative_path: String,
    size: u64,
    mtime: u64,
    bytes: Vec<u8>,
    digest: Digest,
}

fn catalog_assets(
    export_dir: &Path,
    export_index: u32,
    assets: &mut Vec<AssetEntry>,
    digests: &mut Vec<Digest>,
    bodies: &mut Vec<Vec<u8>>,
) -> Result<(), Error> {
    let mut pending = Vec::new();
    collect_asset_bucket(&export_dir.join("assets"), export_dir, &mut pending)?;
    collect_asset_bucket(
        &export_dir.join(ASSET_SERVER_DIR_NAME),
        export_dir,
        &mut pending,
    )?;
    let Ok(entries) = fs::read_dir(export_dir) else {
        merge_hashed_assets(
            export_index,
            hash_pending_assets(pending)?,
            assets,
            digests,
            bodies,
        );
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if name == "assets" || name == ASSET_SERVER_DIR_NAME {
            continue;
        }
        let uuid = name.to_string_lossy();
        if looks_like_uuid(&uuid) {
            collect_one_asset(&entry.path(), export_dir, uuid.as_ref(), &mut pending)?;
        }
    }
    merge_hashed_assets(
        export_index,
        hash_pending_assets(pending)?,
        assets,
        digests,
        bodies,
    );
    Ok(())
}

fn collect_asset_bucket(
    bucket: &Path,
    export_dir: &Path,
    pending: &mut Vec<PendingAsset>,
) -> Result<(), Error> {
    if !bucket.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(bucket)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let uuid = entry.file_name().to_string_lossy().into_owned();
        collect_one_asset(&entry.path(), export_dir, &uuid, pending)?;
    }
    Ok(())
}

fn collect_one_asset(
    dir: &Path,
    export_dir: &Path,
    uuid: &str,
    pending: &mut Vec<PendingAsset>,
) -> Result<(), Error> {
    let content = dir.join("content");
    if !content.is_file() {
        return Ok(());
    }
    pending.push(PendingAsset {
        uuid: uuid.to_owned(),
        relative_path: relative_to(export_dir, &content),
        content,
    });
    Ok(())
}

fn hash_pending_assets(pending: Vec<PendingAsset>) -> Result<Vec<HashedAsset>, Error> {
    pending
        .into_par_iter()
        .map(|file| {
            let bytes = fs::read(&file.content)?;
            let digest = hash::hash_bytes(&bytes);
            let mtime = unix_mtime(&fs::metadata(&file.content)?);
            Ok(HashedAsset {
                uuid: file.uuid,
                relative_path: file.relative_path,
                size: bytes.len() as u64,
                mtime,
                bytes,
                digest,
            })
        })
        .collect()
}

fn hashed_facebook_image(relative_path: String, bytes: Vec<u8>, mtime: u64) -> HashedAsset {
    let digest = hash::hash_bytes(&bytes);
    HashedAsset {
        uuid: digest.to_hex(),
        relative_path,
        size: bytes.len() as u64,
        mtime,
        bytes: Vec::new(),
        digest,
    }
}

fn merge_hashed_assets(
    export_index: u32,
    hashed: Vec<HashedAsset>,
    assets: &mut Vec<AssetEntry>,
    digests: &mut Vec<Digest>,
    bodies: &mut Vec<Vec<u8>>,
) {
    for item in hashed {
        if let Some(index) = digests.iter().position(|existing| *existing == item.digest) {
            add_asset_provenance(&mut assets[index], export_index);
            continue;
        }
        if item.bytes.is_empty()
            && let Some(index) = assets
                .iter()
                .position(|entry| entry.blob_len == 0 && entry.uuid == item.uuid)
        {
            add_asset_provenance(&mut assets[index], export_index);
            continue;
        }
        assets.push(AssetEntry {
            export_index,
            export_indices: vec![export_index],
            uuid: item.uuid,
            relative_path: item.relative_path,
            size: item.size,
            mtime: item.mtime,
            blob_off: 0,
            blob_len: 0,
        });
        digests.push(item.digest);
        bodies.push(item.bytes);
    }
}

fn unix_mtime(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn build_text(conversations: &[ConversationRecord]) -> Result<(String, Vec<TextSpan>), Error> {
    let mut text = String::new();
    let mut spans = Vec::new();
    for (index, record) in conversations.iter().enumerate() {
        let index = u32_fit_usize(index, "conversation index")?;
        if let Some(title) = &record.item.conversation.title {
            push_span(
                &mut text,
                &mut spans,
                title,
                RECORD_CONVERSATION,
                index,
                FIELD_TITLE,
            );
        }
        if let Some(summary) = &record.item.conversation.summary {
            push_span(
                &mut text,
                &mut spans,
                summary,
                RECORD_CONVERSATION,
                index,
                FIELD_SUMMARY,
            );
        }
        for response in &record.item.responses {
            if let Some(message) = &response.response.message {
                push_message_text(
                    &mut text,
                    &mut spans,
                    message,
                    RECORD_RESPONSE,
                    index,
                    FIELD_MESSAGE,
                );
            }
        }
    }
    Ok((text, spans))
}

fn push_span(
    text: &mut String,
    spans: &mut Vec<TextSpan>,
    value: &str,
    record_kind: u32,
    record_index: u32,
    field_id: u32,
) {
    if value.is_empty() {
        return;
    }
    let text_off = text.len() as u64;
    text.push_str(value);
    spans.push(TextSpan {
        text_off,
        len: value.len() as u64,
        record_kind,
        record_index,
        field_id,
    });
}

fn push_message_text(
    text: &mut String,
    spans: &mut Vec<TextSpan>,
    atom: &JsonAtom,
    record_kind: u32,
    record_index: u32,
    field_id: u32,
) {
    match atom {
        JsonAtom::String(value) => {
            push_span(text, spans, value, record_kind, record_index, field_id);
        }
        JsonAtom::Array(values) => {
            for value in values {
                push_content_part_text(text, spans, value, record_kind, record_index, field_id);
            }
        }
        JsonAtom::Object(_) => {
            push_content_part_text(text, spans, atom, record_kind, record_index, field_id);
        }
        JsonAtom::Null
        | JsonAtom::Bool(_)
        | JsonAtom::I64(_)
        | JsonAtom::U64(_)
        | JsonAtom::F64(_) => {}
    }
}

fn push_content_part_text(
    text: &mut String,
    spans: &mut Vec<TextSpan>,
    atom: &JsonAtom,
    record_kind: u32,
    record_index: u32,
    field_id: u32,
) {
    match atom {
        JsonAtom::String(value) => {
            push_span(text, spans, value, record_kind, record_index, field_id);
        }
        JsonAtom::Object(fields) => {
            let is_text_part =
                matches!(fields.get("type"), Some(JsonAtom::String(kind)) if kind == "text");
            if is_text_part && let Some(JsonAtom::String(body)) = fields.get("text") {
                push_span(text, spans, body, record_kind, record_index, field_id);
            }
        }
        JsonAtom::Array(values) => {
            for value in values {
                push_content_part_text(text, spans, value, record_kind, record_index, field_id);
            }
        }
        JsonAtom::Null
        | JsonAtom::Bool(_)
        | JsonAtom::I64(_)
        | JsonAtom::U64(_)
        | JsonAtom::F64(_) => {}
    }
}

fn create_output_parent(output: &Path) -> Result<(), Error> {
    match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            fs::create_dir_all(parent)?;
            Ok(())
        }
        _ => Ok(()),
    }
}

fn is_session_sqlite_path(path: &Path) -> bool {
    if is_grok_oss_db_name(path) {
        return false;
    }
    if is_session_search_sqlite_name(path) {
        return true;
    }
    is_sqlite_magic(path) && ingest_sqlite::has_session_docs_table(path)
}

fn is_session_search_sqlite_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(SESSION_SEARCH_SQLITE_NAME))
}

fn is_grok_oss_db_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(GROK_OSS_DB_NAME))
}

fn is_sqlite_magic(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut header = [0u8; 16];
    matches!(file.read_exact(&mut header), Ok(())) && header.as_slice() == SQLITE_MAGIC
}

fn is_obsidian_vault(path: &Path) -> bool {
    path.is_dir() && path.join(".obsidian").is_dir()
}

fn is_agents_reports_dir(path: &Path) -> bool {
    let mut components = path.components().rev();
    let Some(last) = components.next() else {
        return false;
    };
    let Some(prev) = components.next() else {
        return false;
    };
    last.as_os_str() == "reports" && prev.as_os_str() == ".agents"
}

fn is_markdown_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

fn is_markdown_input(path: &Path, home: &Path, config: &Config) -> Result<bool, Error> {
    let meta = fs::metadata(path)?;
    if meta.is_file() {
        return Ok(is_markdown_file(path));
    }
    if meta.is_dir() {
        return dir_has_markdown(path, home, config);
    }
    Ok(false)
}

fn skip_system_trash_walk_name(name: &std::ffi::OsStr) -> bool {
    let text = name.to_string_lossy();
    text == ".Trash" || text.starts_with(".Trash-")
}

fn is_sandbox_blocked_name(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with("sandbox-blocked-dir")
}

fn skip_walk_name(name: &OsStr) -> bool {
    name == ".git"
        || name == "node_modules"
        || name == ".obsidian"
        || skip_system_trash_walk_name(name)
        || is_sandbox_blocked_name(name)
}

fn dump_skips_path(path: &Path, home: &Path, config: &Config) -> bool {
    config.skips_directory(path, home, scan_xdg_data_home(home).as_deref())
}

fn dir_has_markdown(dir: &Path, home: &Path, config: &Config) -> Result<bool, Error> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            if skip_walk_name(&name) {
                continue;
            }
            let path = entry.path();
            if dump_skips_path(&path, home, config) {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if file_type.is_file() && is_markdown_file(&path) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn collect_markdown_files(
    root: &Path,
    home: &Path,
    config: &Config,
) -> Result<Vec<PathBuf>, Error> {
    if root.is_file() {
        if is_markdown_file(root) {
            return Ok(vec![root.to_path_buf()]);
        }
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            if skip_walk_name(&name) {
                continue;
            }
            let path = entry.path();
            if dump_skips_path(&path, home, config) {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if file_type.is_file() && is_markdown_file(&path) {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn markdown_item(root: &Path, path: &Path) -> Result<ConversationItem, Error> {
    let bytes = fs::read(path)?;
    let digest = hash::hash_bytes(&bytes);
    let mut extra = ExtraMap::new();
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => {
            extra.insert("utf8_lossy".to_owned(), JsonAtom::Bool(true));
            String::from_utf8_lossy(err.as_bytes()).into_owned()
        }
    };
    let (yaml_extra, body) = split_frontmatter(&text);
    extra = union_extra(extra, yaml_extra, true);
    let id = markdown_id(root, path);
    let title = first_heading(body).or_else(|| file_stem_title(path));
    let mtime = fs::metadata(path)
        .ok()
        .map(|meta| Timestamp::Other(JsonAtom::U64(unix_mtime(&meta))));
    Ok(ConversationItem {
        conversation: Conversation {
            id: Some(id.clone()),
            title,
            create_time: mtime.clone(),
            modify_time: mtime.clone(),
            extra,
            ..Default::default()
        },
        responses: vec![ResponseItem {
            response: Response {
                _id: Some(digest.to_hex()),
                conversation_id: Some(id),
                message: Some(JsonAtom::String(body.to_owned())),
                create_time: mtime,
                extra: ExtraMap::new(),
                ..Default::default()
            },
            share_link: None,
            extra: ExtraMap::new(),
        }],
        extra: ExtraMap::new(),
    })
}

fn markdown_id(root: &Path, path: &Path) -> String {
    if path == root {
        return path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
    }
    relative_to(root, path)
}

fn file_stem_title(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .map(str::to_owned)
}

fn first_heading(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('#') {
            continue;
        }
        let hashes = trimmed.bytes().take_while(|&byte| byte == b'#').count();
        if !(1..=6).contains(&hashes) {
            continue;
        }
        let rest = trimmed[hashes..].trim();
        let rest = rest.trim_end_matches('#').trim();
        if rest.is_empty() {
            continue;
        }
        return Some(rest.to_owned());
    }
    None
}

fn split_frontmatter(text: &str) -> (ExtraMap, &str) {
    let Some(after_open) = strip_open_fence(text) else {
        return (ExtraMap::new(), text);
    };
    let Some((yaml, body)) = split_close_fence(after_open) else {
        return (ExtraMap::new(), text);
    };
    (parse_simple_yaml(yaml), body)
}

fn strip_open_fence(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---")?;
    rest.strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
}

fn split_close_fence(after_open: &str) -> Option<(&str, &str)> {
    if let Some(rest) = after_open.strip_prefix("---") {
        let rest = rest
            .strip_prefix("\r\n")
            .or_else(|| rest.strip_prefix('\n'))
            .unwrap_or(rest);
        return Some(("", rest));
    }
    let mut search_from = 0;
    while let Some(rel) = after_open[search_from..].find('\n') {
        let abs = search_from + rel + 1;
        let rest = &after_open[abs..];
        if let Some(after) = rest.strip_prefix("---") {
            let after = after.strip_prefix('\r').unwrap_or(after);
            if after.is_empty() || after.starts_with('\n') {
                let yaml = &after_open[..abs.saturating_sub(1)];
                let body = after.strip_prefix('\n').unwrap_or(after);
                return Some((yaml, body));
            }
        }
        search_from = abs;
    }
    None
}

fn parse_simple_yaml(yaml: &str) -> ExtraMap {
    let mut extra = ExtraMap::new();
    for line in yaml.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        extra.insert(key.to_owned(), parse_yaml_scalar(value.trim()));
    }
    extra
}

fn parse_yaml_scalar(value: &str) -> JsonAtom {
    if value.is_empty() || value == "~" || value == "null" {
        return JsonAtom::Null;
    }
    if value == "true" {
        return JsonAtom::Bool(true);
    }
    if value == "false" {
        return JsonAtom::Bool(false);
    }
    if let Some(unquoted) = unquote_yaml(value) {
        return JsonAtom::String(unquoted);
    }
    if let Ok(number) = value.parse::<i64>() {
        return JsonAtom::I64(number);
    }
    JsonAtom::String(value.to_owned())
}

fn unquote_yaml(value: &str) -> Option<String> {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
        {
            return Some(value[1..value.len() - 1].to_owned());
        }
    }
    None
}

fn dir_account_name(path: &Path) -> Result<String, Error> {
    let named = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    named
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::ingest("could not infer account from the directory name; pass --account")
        })
}

fn process_user_account() -> Result<String, Error> {
    for key in ["USER", "USERNAME"] {
        if let Ok(user) = std::env::var(key)
            && !user.is_empty()
        {
            return Ok(user);
        }
    }
    Err(Error::ingest("USER is not set; pass --account"))
}

#[cfg(test)]
mod home_scan_skip_tests {
    use std::fs;
    use std::path::Path;

    use super::home_scan_skip_reason;
    use crate::config::Config;

    #[test]
    fn skips_sandbox_blocked_dir_by_name() {
        let home = Path::new("/tmp/fake-home-scan");
        let path = home.join(".grok").join("sandbox-blocked-dir.3");
        let config = Config::crate_defaults();
        assert_eq!(
            home_scan_skip_reason(home, &path, &config),
            Some("Grok OSS blocked sandbox directory")
        );
    }

    #[test]
    fn does_not_skip_agents_trash_on_crate_defaults() {
        let home = Path::new("/tmp/fake-home-scan");
        let path = home.join(".agents").join("trash");
        let config = Config::crate_defaults();
        assert_eq!(
            home_scan_skip_reason(home, &path, &config),
            None,
            "crate defaults must not skip ~/.agents/trash"
        );
        let reports = home.join(".agents").join("reports");
        assert_eq!(home_scan_skip_reason(home, &reports, &config), None);
    }

    #[test]
    fn skips_agents_trash_when_listed_in_skip_directories() {
        let home = Path::new("/tmp/fake-home-scan");
        let path = home.join(".agents").join("trash");
        let mut config = Config::crate_defaults();
        config.scan.skip_directories = vec![home.join(".agents").join("trash")];
        assert_eq!(
            home_scan_skip_reason(home, &path, &config),
            Some("skip_directories or system trash")
        );
        let nested = path.join("ChatExport_trash");
        assert_eq!(
            home_scan_skip_reason(home, &nested, &config),
            Some("skip_directories or system trash")
        );
    }

    #[test]
    fn skips_xdg_trash_on_crate_defaults() {
        let home = Path::new("/tmp/fake-home-scan");
        let config = Config::crate_defaults();
        let xdg = home
            .join(".local")
            .join("share")
            .join("Trash")
            .join("files")
            .join("secret.json");
        assert_eq!(
            home_scan_skip_reason(home, &xdg, &config),
            Some("skip_directories or system trash")
        );
        let mount = home.join(".Trash-1000").join("files");
        assert_eq!(
            home_scan_skip_reason(home, &mount, &config),
            Some("skip_directories or system trash")
        );
    }

    #[cfg(unix)]
    #[test]
    fn skip_by_name_does_not_read_dir_mode_000_sandbox_or_skip_list() {
        use crate::logging::capture_events;
        use std::os::unix::fs::PermissionsExt;
        use tracing::Level;

        let home =
            std::env::temp_dir().join(format!("majestic-skip-by-name-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("temp home");
        let grok = home.join(".grok");
        fs::create_dir_all(&grok).expect(".grok");
        let blocked = grok.join("sandbox-blocked-dir.9");
        fs::create_dir_all(&blocked).expect("sandbox-blocked-dir");
        fs::write(blocked.join("secret.json"), b"trap").expect("nested trap");
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).expect("mode 000");
        let home_blocked = home.join("sandbox-blocked-dir.1");
        fs::create_dir_all(&home_blocked).expect("home sandbox-blocked-dir");
        fs::write(home_blocked.join("secret.json"), b"trap").expect("home nested trap");
        fs::set_permissions(&home_blocked, fs::Permissions::from_mode(0o000))
            .expect("home sandbox mode 000");

        let trash = home.join(".agents").join("trash");
        fs::create_dir_all(trash.join("backup")).expect("trash nested");
        fs::write(trash.join("backup").join("old.json"), b"{}").expect("backup");
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o000)).expect("trash mode 000");

        let empty_sqlite = home
            .join(".grok")
            .join("sessions")
            .join("session_search.sqlite");
        fs::create_dir_all(empty_sqlite.parent().expect("sessions")).expect("sessions");
        fs::write(&empty_sqlite, b"").expect("empty sqlite");

        let mut config = Config::crate_defaults();
        config.scan.skip_directories = vec![home.join(".agents").join("trash")];

        let events = capture_events(|| {
            let found = super::discover_home_export_paths(&home, &config)
                .expect("home scan must not abort");
            assert!(
                found.is_empty(),
                "mode 000 skip dirs and empty sqlite must not be sources, got {found:?}"
            );
        });

        let _ = fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755));
        let _ = fs::set_permissions(&home_blocked, fs::Permissions::from_mode(0o755));
        let _ = fs::set_permissions(&trash, fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(&home);

        assert!(
            !events
                .iter()
                .any(|(level, message)| *level == Level::ERROR
                    && message.contains("permission denied")),
            "skip by name must not read_dir sandbox-blocked-dir or skip_directories, got {events:?}"
        );
        assert!(
            !events.iter().any(|(level, _)| *level == Level::INFO),
            "expected skip and empty sqlite must not log at info, got {events:?}"
        );
        let sqlite_debug = events.iter().any(|(level, message)| {
            *level == Level::DEBUG && message.contains("skipped a path that is not a memex source")
        });
        assert!(
            sqlite_debug,
            "empty sqlite must log at debug, got {events:?}"
        );
    }
}
