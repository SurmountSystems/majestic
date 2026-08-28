//! Peek zip central directories and stream one JSON value at a time.
//!
//! crates.io `zip` exposes each entry as [`std::io::Read`] (deflate is
//! decoded as the visitor pulls bytes). This module does not extract to disk
//! and does not dump a whole entry into a `Vec` before parsing.
//! Skip encrypted zips, skip entries larger than
//! [`crate::config::ZipConfig::max_uncompressed_bytes`] (default 8 GiB), and
//! skip zip-inside-zip.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use zip::ZipArchive;

use crate::Error;

/// Skip uncompressed entry sizes above 8 GiB.
pub(crate) const MAX_UNCOMPRESSED: u64 = 8 * 1024 * 1024 * 1024;

/// Known inner names decide how a zip is ingested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZipKind {
    ChatGpt,
    Grok,
    Telegram,
    Facebook,
    Twitter,
}

/// Last path component of a zip entry name (`ChatGPT/user.json` → `user.json`).
pub(crate) fn inner_file_name(name: &str) -> &str {
    name.rsplit(['/', '\\'])
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(name)
}

pub(crate) fn is_zip_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

fn is_nested_zip_name(name: &str) -> bool {
    inner_file_name(name)
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("zip"))
}

fn is_directory_name(name: &str) -> bool {
    name.ends_with('/') || name.ends_with('\\')
}

fn open_archive(path: &Path) -> Result<ZipArchive<File>, Error> {
    let file = File::open(path)?;
    ZipArchive::new(file).map_err(|error| map_zip_path(path, error))
}

fn map_zip_path(path: &Path, error: zip::result::ZipError) -> Error {
    Error::ingest(format!("zip {}: {error}", path.display()))
}

/// Names in the central directory, in archive order.
pub(crate) fn list_zip_names(path: &Path) -> Result<Vec<String>, Error> {
    let archive = open_archive(path)?;
    Ok(archive.file_names().map(str::to_owned).collect())
}

/// Classify from central-directory names. Does not extract.
pub(crate) fn classify_zip(path: &Path) -> Result<Option<ZipKind>, Error> {
    if !is_zip_path(path) {
        return Ok(None);
    }
    let names = match list_zip_names(path) {
        Ok(names) => names,
        Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(err) => {
            crate::logging::log_error_on_path(path, &err);
            return Ok(None);
        }
    };
    Ok(kind_from_names(&names))
}

pub(crate) fn kind_from_names(names: &[String]) -> Option<ZipKind> {
    let mut has_user = false;
    let mut has_conversations = false;
    let mut has_grok = false;
    let mut has_telegram = false;
    let mut has_facebook = false;
    let mut has_account_js = false;
    let mut has_tweet_js = false;
    for name in names {
        if crate::ingest_facebook::has_activity_path(name) {
            has_facebook = true;
        }
        if is_directory_name(name) || is_nested_zip_name(name) {
            continue;
        }
        let file_name = inner_file_name(name);
        if file_name.eq_ignore_ascii_case("user.json") {
            has_user = true;
        }
        if crate::ingest_chatgpt::is_conversations_json_name(file_name) {
            has_conversations = true;
        }
        if file_name == "prod-grok-backend.json" {
            has_grok = true;
        }
        if file_name == crate::ingest_telegram::RESULT_FILE_NAME {
            has_telegram = true;
        }
        if crate::ingest_x::is_account_js_name(file_name) {
            has_account_js = true;
        }
        if crate::ingest_x::is_tweet_js_name(file_name) {
            has_tweet_js = true;
        }
    }
    if has_facebook {
        Some(ZipKind::Facebook)
    } else if has_user && has_conversations {
        Some(ZipKind::ChatGpt)
    } else if has_grok {
        Some(ZipKind::Grok)
    } else if has_telegram {
        Some(ZipKind::Telegram)
    } else if has_account_js && has_tweet_js {
        Some(ZipKind::Twitter)
    } else {
        None
    }
}

fn skip_reason(
    name: &str,
    encrypted: bool,
    uncompressed: u64,
    is_dir: bool,
    max_uncompressed: u64,
) -> Option<&'static str> {
    if is_dir || is_directory_name(name) {
        return Some("directory entry");
    }
    if encrypted {
        return Some("encrypted zip entry");
    }
    if uncompressed > max_uncompressed {
        return Some("uncompressed zip entry exceeds the configured size limit");
    }
    if is_nested_zip_name(name) {
        return Some("zip-inside-zip");
    }
    None
}

fn is_password_required(error: &zip::result::ZipError) -> bool {
    matches!(
        error,
        zip::result::ZipError::UnsupportedArchive(message)
            if *message == zip::result::ZipError::PASSWORD_REQUIRED
    )
}

fn log_skip_entry(path: &Path, name: &str, reason: &'static str) {
    tracing::debug!(
        path = %path.display(),
        entry = %name,
        reason,
        "skipped a zip entry"
    );
}

/// Inspect central-directory metadata via `by_index_raw` (no password). Skip
/// encrypted / huge / nested-zip / directory entries. Then `by_index` only for
/// plaintext entries. `PASSWORD_REQUIRED` is a skip, not an ingest failure.
fn open_plaintext_entry<'a>(
    archive: &'a mut ZipArchive<File>,
    path: &Path,
    index: usize,
    name: &str,
    max_uncompressed: u64,
) -> Result<Option<zip::read::ZipFile<'a>>, Error> {
    {
        let raw = match archive.by_index_raw(index) {
            Ok(raw) => raw,
            Err(error) if is_password_required(&error) => {
                log_skip_entry(path, name, "encrypted zip entry");
                return Ok(None);
            }
            Err(error) => return Err(map_zip_path(path, error)),
        };
        if let Some(reason) = skip_reason(
            name,
            raw.encrypted(),
            raw.size(),
            raw.is_dir(),
            max_uncompressed,
        ) {
            log_skip_entry(path, name, reason);
            return Ok(None);
        }
    }
    match archive.by_index(index) {
        Ok(file) => Ok(Some(file)),
        Err(error) if is_password_required(&error) => {
            log_skip_entry(path, name, "encrypted zip entry");
            Ok(None)
        }
        Err(error) => Err(map_zip_path(path, error)),
    }
}

/// Stream matching entries without extracting. One entry at a time.
pub(crate) fn for_each_matching_entry(
    path: &Path,
    max_uncompressed: u64,
    mut predicate: impl FnMut(&str) -> bool,
    mut each: impl FnMut(&str, &mut dyn Read) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut archive = open_archive(path)?;
    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    let mut indices: Vec<(usize, String)> = names
        .into_iter()
        .enumerate()
        .filter(|(_, name)| predicate(inner_file_name(name)) || predicate(name))
        .collect();
    indices.sort_by(|left, right| left.1.cmp(&right.1));
    for (index, name) in indices {
        let Some(file) = open_plaintext_entry(&mut archive, path, index, &name, max_uncompressed)?
        else {
            continue;
        };
        let mut reader = BufReader::with_capacity(256 * 1024, file);
        each(&name, &mut reader)?;
    }
    Ok(())
}

/// Open one inner file by last-component name and stream it.
pub(crate) fn with_named_entry<T>(
    path: &Path,
    file_name: &str,
    max_uncompressed: u64,
    f: impl FnOnce(&mut dyn Read) -> Result<T, Error>,
) -> Result<Option<T>, Error> {
    let mut archive = match open_archive(path) {
        Ok(archive) => archive,
        Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    let Some(index) = names
        .iter()
        .position(|name| inner_file_name(name).eq_ignore_ascii_case(file_name))
    else {
        return Ok(None);
    };
    let name = names[index].clone();
    let Some(file) = open_plaintext_entry(&mut archive, path, index, &name, max_uncompressed)?
    else {
        return Ok(None);
    };
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    f(&mut reader).map(Some)
}

#[cfg(test)]
mod skip_limit_tests {
    use super::*;

    #[test]
    fn skip_reason_honors_max_uncompressed_bytes() {
        assert_eq!(
            skip_reason("a.json", false, 100, false, 50),
            Some("uncompressed zip entry exceeds the configured size limit")
        );
        assert_eq!(skip_reason("a.json", false, 50, false, 50), None);
        assert_eq!(skip_reason("a.json", false, 100, false, 200), None);
        assert_eq!(
            skip_reason(
                "a.json",
                false,
                MAX_UNCOMPRESSED + 1,
                false,
                MAX_UNCOMPRESSED
            ),
            Some("uncompressed zip entry exceeds the configured size limit")
        );
        assert_eq!(
            skip_reason("a.json", false, MAX_UNCOMPRESSED, false, MAX_UNCOMPRESSED),
            None,
            "exactly the configured limit must ingest"
        );
    }

    #[test]
    fn default_max_uncompressed_is_eight_gib() {
        assert_eq!(MAX_UNCOMPRESSED, 8 * 1024 * 1024 * 1024);
    }
}
