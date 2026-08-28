//! zstd encode and decode for uncompressed memex archives.
//!
//! CLI default is level 3 with no dictionary. The bench may pass other levels
//! and a trained dict. Never deletes the input file.

use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use crate::Error;

/// Default `memex compress` zstd level. No dictionary.
pub const COMPRESS_LEVEL: i32 = 3;

/// zstd frame magic (`28 B5 2F FD`).
pub const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Compress `src` to `dst` at `level`. `dict` is optional (CLI passes `None`).
pub(crate) fn compress_file(
    src: &Path,
    dst: &Path,
    level: i32,
    dict: Option<&[u8]>,
) -> Result<u64, Error> {
    let input = File::open(src)?;
    let output = File::create(dst)?;
    let mut encoder = match dict {
        Some(dict) => zstd::stream::Encoder::with_dictionary(output, level, dict)
            .map_err(|error| Error::ingest(format!("zstd encoder: {error}")))?,
        None => zstd::stream::Encoder::new(output, level)
            .map_err(|error| Error::ingest(format!("zstd encoder: {error}")))?,
    };
    io::copy(
        &mut BufReader::with_capacity(1024 * 1024, input),
        &mut encoder,
    )?;
    let finished = encoder
        .finish()
        .map_err(|error| Error::ingest(format!("zstd finish: {error}")))?;
    Ok(finished.metadata()?.len())
}

/// Decompress `src` to `dst`. `dict` is optional (CLI passes `None`).
pub(crate) fn decompress_file(src: &Path, dst: &Path, dict: Option<&[u8]>) -> Result<(), Error> {
    let input = BufReader::new(File::open(src)?);
    let mut output = File::create(dst)?;
    match dict {
        Some(dict) => {
            let mut decoder = zstd::stream::Decoder::with_dictionary(input, dict)
                .map_err(|error| Error::ingest(format!("zstd decoder: {error}")))?;
            io::copy(&mut decoder, &mut output)?;
        }
        None => {
            let mut decoder = zstd::stream::Decoder::new(input)
                .map_err(|error| Error::ingest(format!("zstd decoder: {error}")))?;
            io::copy(&mut decoder, &mut output)?;
        }
    }
    Ok(())
}

/// Write `{src}.zst` next to `src` at [`COMPRESS_LEVEL`] with no dictionary. Keeps `src`.
pub fn compress_to_zst(src: &Path) -> Result<PathBuf, Error> {
    compress_to_zst_with_level(src, COMPRESS_LEVEL)
}

/// Write `{src}.zst` next to `src` at `level` with no dictionary. Keeps `src`.
pub fn compress_to_zst_with_level(src: &Path, level: i32) -> Result<PathBuf, Error> {
    if file_name_has_zst_extension(src) {
        return Err(Error::InvalidParams(format!(
            "{} is already a .zst file",
            src.display()
        )));
    }
    let mut dst = src.as_os_str().to_owned();
    dst.push(".zst");
    let dst = PathBuf::from(dst);
    compress_file(src, &dst, level, None)?;
    Ok(dst)
}

/// Write `FILE` from `FILE.zst`. Does not require a dict file. Keeps the `.zst`.
pub fn decompress_zst(src: &Path) -> Result<PathBuf, Error> {
    let name = src
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            Error::InvalidParams(format!(
                "{} is not a .zst file; pass FILE.zst",
                src.display()
            ))
        })?;
    let stem = strip_zst_suffix(name).ok_or_else(|| {
        Error::InvalidParams(format!(
            "{} is not a .zst file; pass FILE.zst",
            src.display()
        ))
    })?;
    let dst = src.with_file_name(stem);
    decompress_file(src, &dst, None)?;
    Ok(dst)
}

fn file_name_has_zst_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zst"))
}

fn strip_zst_suffix(name: &str) -> Option<&str> {
    let (stem, ext) = name.rsplit_once('.')?;
    if ext.eq_ignore_ascii_case("zst") && !stem.is_empty() {
        Some(stem)
    } else {
        None
    }
}
