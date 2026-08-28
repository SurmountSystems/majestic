//! Rerunnable zstd bench of uncompressed archives under a directory.
//!
//! Reads `*.archive` and `*.majestic` under the given dir (default `$HOME/memex`).
//! Writes JSONL (one object per row) and a markdown report. Temps live under
//! `$HOME/memex/bench-zstd/` and are `gio trash`'d after each level. Never
//! deletes source archives or Downloads. 1-pass compresses with no trained
//! dict. 2-pass trains a dict on samples, then compresses with that dict.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use serde::Serialize;

use crate::Error;
use crate::archive::Archive;
use crate::config::Config;
use crate::home_dir;
use crate::ingest::{discover_home_export_paths, ingest};
use crate::{SearchFlags, search_with};

/// Minimum trained dict size: 64 KiB.
pub const DICT_SIZE_MIN: u64 = 64 * 1024;
/// Maximum trained dict size: 100 MiB.
pub const DICT_SIZE_MAX: u64 = 100 * 1024 * 1024;
const SAMPLE_CHUNK: usize = 8 * 1024;
const SAMPLE_BUDGET_FACTOR: u64 = 8;
const SAMPLE_BUDGET_MAX: usize = 256 * 1024 * 1024;
const LEVELS: std::ops::RangeInclusive<i32> = 1..=22;

/// `clamp(64KiB, S/100, 100MiB)` after uncompressed size `S`.
pub fn dict_size_bytes(uncompressed: u64) -> u64 {
    uncompressed
        .saturating_div(100)
        .clamp(DICT_SIZE_MIN, DICT_SIZE_MAX)
}

#[derive(Debug, Clone)]
pub struct BenchZstdOptions {
    pub dir: PathBuf,
    pub jsonl: PathBuf,
    pub report: PathBuf,
    pub re_ingest: bool,
    pub home: PathBuf,
}

impl BenchZstdOptions {
    pub fn from_flags(
        dir: Option<PathBuf>,
        jsonl: Option<PathBuf>,
        report: Option<PathBuf>,
        re_ingest: bool,
    ) -> Result<Self, Error> {
        let home = home_dir()?;
        let dir = dir.unwrap_or_else(|| home.join("memex"));
        let bench_root = home.join("memex").join("bench-zstd");
        Ok(Self {
            dir,
            jsonl: jsonl.unwrap_or_else(|| bench_root.join("report.jsonl")),
            report: report.unwrap_or_else(|| bench_root.join("report.md")),
            re_ingest,
            home,
        })
    }
}

#[derive(Serialize)]
struct HeaderRow<'a> {
    kind: &'a str,
    power_profile: String,
    garuda_inxi: String,
    dir: String,
    re_ingest: bool,
}

#[derive(Serialize)]
struct LevelRow<'a> {
    kind: &'a str,
    archive: String,
    level: i32,
    dict_pass: u8,
    uncompressed_bytes: u64,
    compressed_bytes: u64,
    ratio: f64,
    dict_size: u64,
    train_ms: u128,
    compress_ms: u128,
    decompress_ms: u128,
    lizard_hits: usize,
    the_hits: usize,
    lizard_ms: u128,
    the_ms: u128,
}

#[derive(Serialize)]
struct ReIngestRow<'a> {
    kind: &'a str,
    pass: u8,
    ms: u128,
    exports: usize,
    conversations: usize,
    source_count: usize,
}

/// Run the bench. Does not delete source archives or Downloads.
pub fn run_bench_zstd(options: BenchZstdOptions) -> Result<(), Error> {
    let bench_root = options.home.join("memex").join("bench-zstd");
    fs::create_dir_all(&bench_root)?;
    if let Some(parent) = options.jsonl.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = options.report.parent() {
        fs::create_dir_all(parent)?;
    }

    let header = HeaderRow {
        kind: "header",
        power_profile: snapshot_cmd("powerprofilesctl", &["get"]),
        garuda_inxi: snapshot_cmd_timeout("garuda-inxi", &[]),
        dir: options.dir.display().to_string(),
        re_ingest: options.re_ingest,
    };

    let jsonl_file = File::create(&options.jsonl)?;
    let mut jsonl = BufWriter::new(jsonl_file);
    writeln!(jsonl, "{}", serde_json::to_string(&header)?)?;

    let mut markdown = String::new();
    markdown.push_str("# memex zstd bench\n\n");
    markdown.push_str(&format!("- dir: `{}`\n", options.dir.display()));
    markdown.push_str(&format!("- power profile: {}\n", header.power_profile));
    markdown.push_str("\n## garuda-inxi\n\n```\n");
    markdown.push_str(&header.garuda_inxi);
    markdown.push_str("\n```\n");

    if options.re_ingest {
        let re_rows = run_re_ingest(&options.home, &bench_root)?;
        for row in &re_rows {
            writeln!(jsonl, "{}", serde_json::to_string(row)?)?;
        }
        markdown.push_str("\n## re-ingest\n\n");
        markdown.push_str("| pass | ms | exports | conversations | sources |\n");
        markdown.push_str("|------|----|---------|---------------|--------|\n");
        for row in &re_rows {
            markdown.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                row.pass, row.ms, row.exports, row.conversations, row.source_count
            ));
        }
    }

    let archives = collect_uncompressed_archives(&options.dir)?;
    markdown.push_str("\n## levels\n\n");
    markdown.push_str("| archive | level | dict_pass | uncompressed | compressed | ratio | dict | train_ms | compress_ms | decompress_ms | lizard | the |\n");
    markdown.push_str("|---------|-------|-----------|--------------|------------|-------|------|----------|-------------|---------------|--------|-----|\n");

    for archive in &archives {
        let uncompressed = fs::metadata(archive)?.len();
        let dict_size = dict_size_bytes(uncompressed);
        let (lizard_hits, lizard_ms) = timed_search(archive, "lizard")?;
        let (the_hits, the_ms) = timed_search(archive, "the")?;
        let samples = read_dict_samples(archive, dict_size)?;

        for level in LEVELS {
            for dict_pass in [1_u8, 2] {
                let stem = archive
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("archive");
                let compressed_path =
                    bench_root.join(format!("tmp-{stem}-l{level}-p{dict_pass}.zst"));
                let decompressed_path =
                    bench_root.join(format!("tmp-{stem}-l{level}-p{dict_pass}.bin"));

                let two_pass = dict_pass == 2;
                let train_start = Instant::now();
                let dict = dictionary_for_pass(&samples.0, &samples.1, dict_size, two_pass)?;
                let train_ms = if two_pass {
                    train_start.elapsed().as_millis()
                } else {
                    0
                };

                let compress_start = Instant::now();
                let compressed_bytes = crate::zstd_file::compress_file(
                    archive,
                    &compressed_path,
                    level,
                    dict.as_deref(),
                )?;
                let compress_ms = compress_start.elapsed().as_millis();

                let decompress_start = Instant::now();
                crate::zstd_file::decompress_file(
                    &compressed_path,
                    &decompressed_path,
                    dict.as_deref(),
                )?;
                let decompress_ms = decompress_start.elapsed().as_millis();

                let ratio = if compressed_bytes == 0 {
                    0.0
                } else {
                    uncompressed as f64 / compressed_bytes as f64
                };
                let row = LevelRow {
                    kind: "row",
                    archive: archive.display().to_string(),
                    level,
                    dict_pass,
                    uncompressed_bytes: uncompressed,
                    compressed_bytes,
                    ratio,
                    dict_size,
                    train_ms,
                    compress_ms,
                    decompress_ms,
                    lizard_hits,
                    the_hits,
                    lizard_ms,
                    the_ms,
                };
                writeln!(jsonl, "{}", serde_json::to_string(&row)?)?;
                markdown.push_str(&format!(
                    "| `{}` | {} | {} | {} | {} | {:.3} | {} | {} | {} | {} | {} | {} |\n",
                    archive.display(),
                    level,
                    dict_pass,
                    uncompressed,
                    compressed_bytes,
                    ratio,
                    dict_size,
                    train_ms,
                    compress_ms,
                    decompress_ms,
                    lizard_hits,
                    the_hits
                ));

                gio_trash(&compressed_path)?;
                gio_trash(&decompressed_path)?;
            }
        }
    }

    jsonl.flush()?;
    fs::write(&options.report, markdown.as_bytes())?;
    Ok(())
}

fn collect_uncompressed_archives(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut out = Vec::new();
    collect_archives_walk(dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_archives_walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) == Some("bench-zstd") {
                continue;
            }
            collect_archives_walk(&path, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        if ext.eq_ignore_ascii_case("archive") || ext.eq_ignore_ascii_case("majestic") {
            out.push(path);
        }
    }
    Ok(())
}

fn read_dict_samples(path: &Path, dict_size: u64) -> Result<(Vec<u8>, Vec<usize>), Error> {
    let want = (dict_size.saturating_mul(SAMPLE_BUDGET_FACTOR) as usize)
        .clamp(dict_size.max(1) as usize, SAMPLE_BUDGET_MAX);
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(SAMPLE_CHUNK, file);
    let mut data = Vec::new();
    let mut sizes = Vec::new();
    let mut buf = vec![0u8; SAMPLE_CHUNK];
    while data.len() < want {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let take = n.min(want.saturating_sub(data.len()));
        data.extend_from_slice(&buf[..take]);
        sizes.push(take);
        if take < n {
            break;
        }
    }
    let summed: usize = sizes.iter().copied().sum();
    if summed != data.len() {
        return Err(Error::ingest(format!(
            "zstd sample sizes sum to {summed}, buffer is {}",
            data.len()
        )));
    }
    if data.is_empty() {
        return Err(Error::ingest("no sample bytes for zstd dict"));
    }
    Ok((data, sizes))
}

/// 1-pass: no trained dict (`None`). 2-pass: train on samples, then compress with that dict.
pub(crate) fn dictionary_for_pass(
    data: &[u8],
    sizes: &[usize],
    dict_size: u64,
    two_pass: bool,
) -> Result<Option<Vec<u8>>, Error> {
    if !two_pass {
        return Ok(None);
    }
    train_dict(data, sizes, dict_size).map(Some)
}

fn train_dict(data: &[u8], sizes: &[usize], dict_size: u64) -> Result<Vec<u8>, Error> {
    if data.is_empty() {
        return Err(Error::ingest("no sample bytes for zstd dict"));
    }
    let summed: usize = sizes.iter().copied().sum();
    if summed != data.len() {
        return Err(Error::ingest(format!(
            "zstd 2-pass dict: Src size is incorrect (sizes sum {summed}, buffer {})",
            data.len()
        )));
    }
    let max_size = (dict_size as usize)
        .max(256)
        .min(data.len().saturating_div(8).max(256));
    zstd::dict::from_continuous(data, sizes, max_size)
        .map_err(|error| Error::ingest(format!("zstd 2-pass dict: {error}")))
}

fn timed_search(archive: &Path, pattern: &str) -> Result<(usize, u128), Error> {
    let start = Instant::now();
    let opened = Archive::open(archive)?;
    let hits = search_with(&opened, pattern, SearchFlags::default())?;
    Ok((hits.len(), start.elapsed().as_millis()))
}

fn run_re_ingest(home: &Path, bench_root: &Path) -> Result<Vec<ReIngestRow<'static>>, Error> {
    let config = Config::load().unwrap_or_else(|_| Config::crate_defaults());
    let sources = discover_home_export_paths(home, &config)?;
    let tmp = bench_root.join("re-ingest.archive");
    if tmp.exists() {
        gio_trash(&tmp)?;
    }
    let first_start = Instant::now();
    let first = if sources.is_empty() {
        None
    } else {
        Some(ingest(&tmp, &sources)?)
    };
    let first_ms = first_start.elapsed().as_millis();
    let second_start = Instant::now();
    let second = if sources.is_empty() {
        None
    } else {
        Some(ingest(&tmp, &sources)?)
    };
    let second_ms = second_start.elapsed().as_millis();
    if tmp.exists() {
        gio_trash(&tmp)?;
    }
    Ok(vec![
        ReIngestRow {
            kind: "re-ingest",
            pass: 1,
            ms: first_ms,
            exports: first.as_ref().map(|r| r.exports).unwrap_or(0),
            conversations: first.as_ref().map(|r| r.conversations).unwrap_or(0),
            source_count: sources.len(),
        },
        ReIngestRow {
            kind: "re-ingest",
            pass: 2,
            ms: second_ms,
            exports: second.as_ref().map(|r| r.exports).unwrap_or(0),
            conversations: second.as_ref().map(|r| r.conversations).unwrap_or(0),
            source_count: sources.len(),
        },
    ])
}

fn gio_trash(path: &Path) -> Result<(), Error> {
    if !path.exists() {
        return Ok(());
    }
    let status = Command::new("gio")
        .args(["trash", "--"])
        .arg(path)
        .status()
        .map_err(|error| Error::ingest(format!("gio trash: {error}")))?;
    if !status.success() {
        return Err(Error::ingest(format!(
            "gio trash failed for {}",
            path.display()
        )));
    }
    Ok(())
}

fn snapshot_cmd(cmd: &str, args: &[&str]) -> String {
    match Command::new(cmd).args(args).output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            format!("{cmd} failed: {}", stderr.trim())
        }
        Err(err) => format!("{cmd} not available: {err}"),
    }
}

fn snapshot_cmd_timeout(cmd: &str, args: &[&str]) -> String {
    let mut command = Command::new("timeout");
    command.args(["20s", cmd]);
    command.args(args);
    match command.output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Ok(_) => snapshot_cmd(cmd, args),
        Err(_) => snapshot_cmd(cmd, args),
    }
}

/// Compress a few KB with zstd and decompress back. Used by tests.
pub fn zstd_roundtrip(bytes: &[u8], level: i32) -> Result<Vec<u8>, Error> {
    let compressed = zstd::bulk::compress(bytes, level)
        .map_err(|error| Error::ingest(format!("zstd compress: {error}")))?;
    zstd::bulk::decompress(&compressed, bytes.len())
        .map_err(|error| Error::ingest(format!("zstd decompress: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dict_size_clamps_one_percent() {
        assert_eq!(dict_size_bytes(0), DICT_SIZE_MIN);
        assert_eq!(dict_size_bytes(1_000_000), DICT_SIZE_MIN);
        assert_eq!(dict_size_bytes(10_000_000), 100_000);
        assert_eq!(dict_size_bytes(6_553_600), DICT_SIZE_MIN);
        assert_eq!(
            dict_size_bytes(DICT_SIZE_MAX.saturating_mul(100)),
            DICT_SIZE_MAX
        );
        assert_eq!(
            dict_size_bytes(DICT_SIZE_MAX.saturating_mul(1000)),
            DICT_SIZE_MAX
        );
    }

    #[test]
    fn train_dict_requires_sample_sizes_to_sum_to_buffer() {
        let data = vec![1u8; 4096];
        let sizes = vec![1024, 1024];
        let err = train_dict(&data, &sizes, 1024).expect_err("mismatched sizes");
        assert!(
            err.to_string().contains("Src size") || err.to_string().contains("zstd 2-pass dict"),
            "zstd must reject sizes that do not sum to the sample buffer, got {err}"
        );
    }

    #[test]
    fn train_dict_succeeds_on_eight_times_dict_budget() {
        let chunk = 8 * 1024;
        let dict_size = 1024u64;
        let payload = b"lizard the synthetic zstd train payload. ".repeat(chunk);
        assert!(payload.len() >= 8 * dict_size as usize);
        let mut sizes = Vec::new();
        let mut remaining = payload.len();
        while remaining > 0 {
            let n = chunk.min(remaining);
            sizes.push(n);
            remaining -= n;
        }
        assert_eq!(
            sizes.iter().sum::<usize>(),
            payload.len(),
            "sample sizes must equal the buffer"
        );
        let dict = train_dict(&payload, &sizes, dict_size).expect("train");
        assert!(!dict.is_empty(), "trained dict must not be empty");
    }

    #[test]
    fn zstd_roundtrip_few_kb() {
        let payload = b"lizard the synthetic zstd roundtrip payload. ".repeat(80);
        assert!(payload.len() > 2000, "fixture must be a few KB");
        assert!(payload.len() < 64 * 1024, "fixture must stay tiny");
        let out = zstd_roundtrip(&payload, 3).expect("tiny zstd roundtrip");
        assert_eq!(out, payload);
    }

    #[test]
    fn two_pass_dict_differs_from_one_pass() {
        let payload = b"lizard the synthetic zstd dictionary sample payload. ".repeat(2000);
        assert!(
            payload.len() > 80_000,
            "training samples must be tens of KB"
        );
        let chunk = 1024;
        let mut sizes = Vec::new();
        let mut remaining = payload.len();
        while remaining > 0 {
            let n = chunk.min(remaining);
            sizes.push(n);
            remaining -= n;
        }
        let dict_size = 1024;
        let one =
            dictionary_for_pass(&payload, &sizes, dict_size, false).expect("1-pass must not train");
        let two = dictionary_for_pass(&payload, &sizes, dict_size, true).expect("2-pass train");
        assert!(one.is_none(), "1-pass compresses with no trained dict");
        let dict = two.expect("2-pass must return a trained dict");
        assert!(!dict.is_empty(), "trained dict must not be empty");

        let no_dict = zstd::bulk::compress(&payload, 3).expect("1-pass compress");
        let with_dict = zstd::bulk::Compressor::with_dictionary(3, &dict)
            .expect("2-pass compressor")
            .compress(&payload)
            .expect("2-pass compress");
        assert_ne!(
            no_dict, with_dict,
            "trained dict must change compressed bytes versus no dict"
        );
    }
}
