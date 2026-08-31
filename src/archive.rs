//! On-disk `.majestic` layout: header, rkyv payload, UTF-8 text blob, span table.
//!
//! Layout (little-endian):
//! - bytes 0..9: magic `MAJESTIC` plus format byte `0x01` (hex 01)
//! - bytes 9..13: version u32 = 1
//! - bytes 13..17: header length u32 = 81
//! - bytes 17..81: u64 offset/length pairs for payload, text, spans, tries
//! - payload: rkyv (unaligned + bytecheck) [`ArchiveRoot`]
//! - padding on new ingest so the UTF-8 text starts at a 2 MiB file offset
//!   ([`TEXT_PAGE_BYTES`]). Older short files may place text immediately after
//!   the payload and still open.
//! - text: UTF-8 searchable strings built at ingest
//! - spans: 32-byte records `(text_off, len, record_kind, record_index, field_id)`
//! - tries: two mmap FST maps plus packed posting lists (see [`crate::trie`])
//! - bao: after tries, a tiny prefix (root [32], blob_len u64, outboard_len u64),
//!   then concatenated canonical conversation bytes and uploaded file bodies,
//!   then one BaoTree outboard. The 81-byte header cannot hold another region
//!   pair. Format generation is magic byte `0x01`. `VERSION` is 1 (64-bit rkyv
//!   relative pointers). Older files without that byte are not readable; wiping
//!   `$HOME/memex` archives is safe because ingest does not delete source exports.
//!
//! Open memory-maps the file (`PROT_READ`, `MAP_SHARED`) and bytechecks the
//! rkyv payload. That mapping is a virtual map of the file, not a heap copy
//! of the file size. It does not parse JSON.

use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};

use memmap2::{Mmap, UncheckedAdvice};
use rkyv::{Archive as RkyvArchive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};

use crate::Error;
use crate::hash::{BAO_PREFIX_LEN, PackedBao};
use crate::schema::{AuthFile, BillingFile, ConversationItem, ExtraMap, JsonAtom, Timestamp};

/// File magic: ASCII `MAJESTIC` plus one format byte `0x01`.
pub const MAGIC: [u8; 9] = *b"MAJESTIC\x01";
/// Majestic format version 1 (64-bit rkyv relative pointers).
pub const VERSION: u32 = 1;
/// Byte length of [`MAGIC`].
pub const MAGIC_LEN: usize = MAGIC.len();
/// Offset of the u32 format version after magic.
pub const VERSION_OFF: usize = MAGIC_LEN;
/// Offset of the u32 header length.
pub const HEADER_LEN_OFF: usize = VERSION_OFF + 4;
/// Offset of the first u64 region pair (payload off).
pub const PAIRS_OFF: usize = HEADER_LEN_OFF + 4;
/// Fixed header size, including magic and version.
pub const HEADER_LEN: usize = PAIRS_OFF + 64;
/// Packed span record size.
pub const SPAN_LEN: usize = 32;
/// Intended page for the UTF-8 text blob: 2 MiB (2097152 bytes).
///
/// New ingest pads the file so text starts at this file offset. Open aligns
/// the mapping's virtual address the same way. Older short archives still open.
pub const TEXT_PAGE_BYTES: usize = 2 * 1024 * 1024;

#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static FORCE_COLLAPSE_ERRNO: std::cell::Cell<Option<i32>> =
        const { std::cell::Cell::new(None) };
}

/// Span `record_kind` for conversation title/summary.
pub const RECORD_CONVERSATION: u32 = 1;
/// Span `record_kind` for response message text.
pub const RECORD_RESPONSE: u32 = 2;
/// Span `field_id` for conversation title.
pub const FIELD_TITLE: u32 = 1;
/// Span `field_id` for conversation summary.
pub const FIELD_SUMMARY: u32 = 2;
/// Span `field_id` for response message strings.
pub const FIELD_MESSAGE: u32 = 3;

/// One Grok account export that contributed records.
#[derive(RkyvArchive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq)]
#[rkyv(derive(Debug, PartialEq))]
pub struct ExportManifest {
    /// Path as passed to ingest (string, not a live file handle).
    pub source_path: String,
    /// UUID directory name when the backend file sits in a uuid-looking folder.
    pub id: Option<String>,
    /// Unknown top-level keys from that backend object.
    pub extra: ExtraMap,
}

/// Conversation item tagged with which export it came from.
///
/// Identity is id + byte_len + modified + a range into the bao-covered blob.
/// Do not store a hash on this row; the outboard holds hashes.
#[derive(RkyvArchive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq)]
#[rkyv(derive(Debug, PartialEq))]
pub struct ConversationRecord {
    /// First export that contributed this row.
    pub export_index: u32,
    /// Every export that contributed (true-dup provenance).
    pub export_indices: Vec<u32>,
    /// Length of the canonical rkyv encoding.
    pub byte_len: u64,
    /// `modify_time`, else `create_time`.
    pub modified: Option<Timestamp>,
    /// Conversation id when present. Never invented.
    pub id: Option<String>,
    /// Offset into the bao-covered canonical blob.
    pub blob_off: u64,
    /// Length of this item's canonical bytes in that blob.
    pub blob_len: u64,
    pub item: ConversationItem,
}

/// `media_posts` / `projects` / `tasks` value tagged with export provenance.
#[derive(RkyvArchive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq)]
#[rkyv(derive(Debug, PartialEq))]
pub struct TaggedJson {
    pub export_index: u32,
    pub value: JsonAtom,
}

/// Auth sibling file tagged with export provenance. Do not print secret values.
#[derive(RkyvArchive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq)]
#[rkyv(derive(Debug, PartialEq))]
pub struct TaggedAuth {
    pub export_index: u32,
    pub file: AuthFile,
}

/// Billing sibling file tagged with export provenance.
#[derive(RkyvArchive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq)]
#[rkyv(derive(Debug, PartialEq))]
pub struct TaggedBilling {
    pub export_index: u32,
    pub file: BillingFile,
}

/// Asset catalog row. File body is stored in the bao-covered blob.
///
/// Identity is the BaoTree root of those bytes (a tree query, not a hash
/// field on this row). `blob_off` / `blob_len` is the file body range.
/// Uuid, size, and mtime are catalog metadata, not sameness.
#[derive(RkyvArchive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq)]
#[rkyv(derive(Debug, PartialEq))]
pub struct AssetEntry {
    /// First export that contributed this byte identity.
    pub export_index: u32,
    /// Every export that contributed this byte identity.
    pub export_indices: Vec<u32>,
    pub uuid: String,
    pub relative_path: String,
    pub size: u64,
    /// Filesystem mtime of `content` as unix seconds, or 0 if missing.
    pub mtime: u64,
    /// Offset of the stored file body in the bao-covered blob.
    pub blob_off: u64,
    /// Length of that stored file body.
    pub blob_len: u64,
}

/// Structured archive payload. Source of truth; the text blob is derived.
#[derive(RkyvArchive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq, Default)]
#[rkyv(derive(Debug, PartialEq))]
pub struct ArchiveRoot {
    pub exports: Vec<ExportManifest>,
    pub conversations: Vec<ConversationRecord>,
    pub media_posts: Vec<TaggedJson>,
    pub projects: Vec<TaggedJson>,
    pub tasks: Vec<TaggedJson>,
    pub auth: Vec<TaggedAuth>,
    pub billing: Vec<TaggedBilling>,
    pub assets: Vec<AssetEntry>,
}

/// One searchable span into the UTF-8 text blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextSpan {
    pub text_off: u64,
    pub len: u64,
    pub record_kind: u32,
    pub record_index: u32,
    pub field_id: u32,
}

impl TextSpan {
    pub fn write_bytes(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.text_off.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.record_kind.to_le_bytes());
        out.extend_from_slice(&self.record_index.to_le_bytes());
        out.extend_from_slice(&self.field_id.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
    }

    pub fn read_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < SPAN_LEN {
            return Err(Error::InvalidArchive);
        }
        Ok(Self {
            text_off: u64_from(bytes, 0)?,
            len: u64_from(bytes, 8)?,
            record_kind: u32_from(bytes, 16)?,
            record_index: u32_from(bytes, 20)?,
            field_id: u32_from(bytes, 24)?,
        })
    }
}

/// Counts from an opened archive. No secret values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveStats {
    pub path: PathBuf,
    pub version: u32,
    pub exports: usize,
    pub conversations: usize,
    pub media_posts: usize,
    pub projects: usize,
    pub tasks: usize,
    pub auth_files: usize,
    pub billing_files: usize,
    pub assets: usize,
    pub payload_bytes: u64,
    pub text_bytes: u64,
    pub spans: usize,
    pub export_labels: Vec<String>,
}

impl fmt::Display for ArchiveStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "majestic archive version {}", self.version)?;
        writeln!(f, "path: {}", self.path.display())?;
        writeln!(f, "export dumps: {}", self.exports)?;
        writeln!(f, "unique conversations: {}", self.conversations)?;
        writeln!(f, "media_posts: {}", self.media_posts)?;
        writeln!(f, "projects: {}", self.projects)?;
        writeln!(f, "tasks: {}", self.tasks)?;
        writeln!(f, "auth_files: {}", self.auth_files)?;
        writeln!(f, "billing_files: {}", self.billing_files)?;
        writeln!(f, "assets: {}", self.assets)?;
        writeln!(f, "payload_bytes: {}", self.payload_bytes)?;
        writeln!(f, "text_bytes: {}", self.text_bytes)?;
        writeln!(f, "spans: {}", self.spans)?;
        for (index, label) in self.export_labels.iter().enumerate() {
            writeln!(f, "export {index}: {label}")?;
        }
        Ok(())
    }
}

/// Memory-mapped `.majestic` file. The mapping is the lifetime of borrowed views.
///
/// Open maps the whole file with `PROT_READ` and `MAP_SHARED`. That is a virtual
/// mapping of the file, not a heap allocation of the file size. Search uses the
/// UTF-8 text region as a subslice of that map. It does not copy the text blob
/// into a `Vec`. Resident size (RSS) is the pages the CPU has faulted, usually
/// because PCRE2 read them. Mapping the file does not make RSS equal the file
/// size. A systemwide search maps every listed archive and holds those maps
/// while PCRE2 runs on each packed span of the already-mapped text. Search does not call
/// [`Self::release_pages`] after every archive while the user is still
/// searching.
pub struct Archive {
    path: PathBuf,
    _file: File,
    mmap: MappedBytes,
    payload_off: usize,
    payload_len: usize,
    text_off: usize,
    text_len: usize,
    spans_off: usize,
    spans_len: usize,
    tries_off: usize,
    tries_len: usize,
    bao_off: usize,
}

impl Archive {
    /// mmap the file, parse the header, bytecheck the rkyv payload. No JSON.
    ///
    /// On Linux, after the map is in place, open calls `madvise` `MADV_HUGEPAGE`
    /// then `madvise` `MADV_COLLAPSE` on the text. Collapse errors are DEBUG.
    /// They do not fail this function. The file is not copied to hugetlbfs.
    /// The map does not use `MAP_HUGETLB` on the file descriptor.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let file = File::open(path)?;
        // SAFETY: Archive keeps the mapping for the life of `mmap`. We never
        // truncate this file while an Archive is live. The map is `PROT_READ`
        // and `MAP_SHARED`.
        let mmap = map_archive_file(&file)?;
        if mmap.len() < HEADER_LEN {
            return Err(Error::InvalidArchive);
        }
        if mmap[0..MAGIC_LEN] != MAGIC {
            return Err(Error::InvalidMagic);
        }
        let version = u32_from(&mmap, VERSION_OFF)?;
        if version != VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        let header_len = u32_from(&mmap, HEADER_LEN_OFF)? as usize;
        if header_len != HEADER_LEN {
            return Err(Error::InvalidArchive);
        }
        let payload_off = read_u64(&mmap, PAIRS_OFF)?;
        let payload_len = read_u64(&mmap, PAIRS_OFF + 8)?;
        let text_off = read_u64(&mmap, PAIRS_OFF + 16)?;
        let text_len = read_u64(&mmap, PAIRS_OFF + 24)?;
        let spans_off = read_u64(&mmap, PAIRS_OFF + 32)?;
        let spans_len = read_u64(&mmap, PAIRS_OFF + 40)?;
        let tries_off = read_u64(&mmap, PAIRS_OFF + 48)?;
        let tries_len = read_u64(&mmap, PAIRS_OFF + 56)?;
        let bao_off = tries_off
            .checked_add(tries_len)
            .ok_or(Error::InvalidArchive)?;
        if mmap.len() < bao_off {
            return Err(Error::InvalidArchive);
        }
        if mmap.len() > bao_off {
            let _ = parse_bao_region(&mmap[bao_off..])?;
        }
        let archive = Self {
            path: path.to_path_buf(),
            _file: file,
            mmap,
            payload_off,
            payload_len,
            text_off,
            text_len,
            spans_off,
            spans_len,
            tries_off,
            tries_len,
            bao_off,
        };
        archive.payload_slice()?;
        let text = archive.text_slice()?;
        advise_text_pages(&archive.path, text);
        archive.spans_slice()?;
        let tries = archive.tries_slice()?;
        if !tries.is_empty() {
            crate::trie::validate_tries_header(tries)?;
        }
        let _ = archive.root()?;
        Ok(archive)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// File offset of the UTF-8 text blob, as stored in the header.
    pub fn text_file_offset(&self) -> u64 {
        self.text_off as u64
    }

    pub fn payload_slice(&self) -> Result<&[u8], Error> {
        slice_at(&self.mmap, self.payload_off, self.payload_len)
    }

    pub fn text_slice(&self) -> Result<&[u8], Error> {
        slice_at(&self.mmap, self.text_off, self.text_len)
    }

    /// True when [`Self::text_slice`] is a view into the mmap, not a heap copy.
    pub fn text_slice_is_mmap_view(&self) -> bool {
        let Ok(text) = self.text_slice() else {
            return false;
        };
        let mmap_start = self.mmap.as_ptr() as usize;
        let mmap_end = mmap_start.saturating_add(self.mmap.len());
        let text_start = text.as_ptr() as usize;
        let text_end = text_start.saturating_add(text.len());
        text_start >= mmap_start && text_end <= mmap_end
    }

    /// Ask the kernel to drop resident pages of this mapping.
    ///
    /// On Unix this is `MADV_DONTNEED` for a read-only file map. Search does
    /// not call this after every archive during a live scan (that throws away
    /// warm cache). Dropping `Archive` unmaps the file. Optional use is after
    /// the whole search if a caller still holds maps. A virtual map is not a
    /// heap allocation; this is not `free`.
    pub fn release_pages(&self) {
        self.mmap.dont_need();
    }

    pub fn text(&self) -> Result<&str, Error> {
        std::str::from_utf8(self.text_slice()?).map_err(|_| Error::InvalidArchive)
    }

    pub fn spans_slice(&self) -> Result<&[u8], Error> {
        slice_at(&self.mmap, self.spans_off, self.spans_len)
    }

    pub fn tries_slice(&self) -> Result<&[u8], Error> {
        slice_at(&self.mmap, self.tries_off, self.tries_len)
    }

    /// Bao-covered concat: conversation encodings, then uploaded file bodies.
    /// Empty on archives written before the trailing bao region existed.
    pub fn bao_blob(&self) -> Result<&[u8], Error> {
        match self.bao_parts()? {
            None => Ok(&[]),
            Some(parts) => Ok(parts.blob),
        }
    }

    /// Single BaoTree outboard for [`Self::bao_blob`].
    pub fn bao_outboard(&self) -> Result<&[u8], Error> {
        match self.bao_parts()? {
            None => Ok(&[]),
            Some(parts) => Ok(parts.outboard),
        }
    }

    /// Archive-wide BaoTree root. None when the trailing region is absent.
    pub fn bao_root(&self) -> Result<Option<[u8; 32]>, Error> {
        Ok(self.bao_parts()?.map(|parts| parts.root))
    }

    fn bao_parts(&self) -> Result<Option<BaoParts<'_>>, Error> {
        if self.mmap.len() == self.bao_off {
            return Ok(None);
        }
        if self.mmap.len() < self.bao_off {
            return Err(Error::InvalidArchive);
        }
        let region = &self.mmap[self.bao_off..];
        Ok(Some(parse_bao_region(region)?))
    }

    pub fn span_count(&self) -> Result<usize, Error> {
        let bytes = self.spans_slice()?;
        if bytes.len() % SPAN_LEN != 0 {
            return Err(Error::InvalidArchive);
        }
        Ok(bytes.len() / SPAN_LEN)
    }

    pub fn span(&self, index: usize) -> Result<TextSpan, Error> {
        let bytes = self.spans_slice()?;
        let start = index.checked_mul(SPAN_LEN).ok_or(Error::InvalidArchive)?;
        let end = start.checked_add(SPAN_LEN).ok_or(Error::InvalidArchive)?;
        let rec = bytes.get(start..end).ok_or(Error::InvalidArchive)?;
        TextSpan::read_bytes(rec)
    }

    /// Zero-copy root after bytecheck.
    pub fn root(&self) -> Result<&ArchivedArchiveRoot, Error> {
        let slice = self.payload_slice()?;
        if slice.is_empty() {
            return Err(Error::InvalidArchive);
        }
        rkyv::access::<ArchivedArchiveRoot, rkyv::rancor::Error>(slice).map_err(Error::bytecheck)
    }

    /// Owned root via rkyv deserialize. Still no JSON.
    pub fn deserialize_root(&self) -> Result<ArchiveRoot, Error> {
        rkyv::deserialize::<ArchiveRoot, rkyv::rancor::Error>(self.root()?)
            .map_err(Error::bytecheck)
    }

    pub fn stats(&self) -> Result<ArchiveStats, Error> {
        let root = self.root()?;
        let mut export_labels = Vec::with_capacity(root.exports.len());
        for export in root.exports.iter() {
            let mut label = export.source_path.as_str().to_owned();
            if let Some(id) = export.id.as_ref() {
                label.push_str(" (id=");
                label.push_str(id.as_str());
                label.push(')');
            }
            export_labels.push(label);
        }
        Ok(ArchiveStats {
            path: self.path.clone(),
            version: VERSION,
            exports: root.exports.len(),
            conversations: root.conversations.len(),
            media_posts: root.media_posts.len(),
            projects: root.projects.len(),
            tasks: root.tasks.len(),
            auth_files: root.auth.len(),
            billing_files: root.billing.len(),
            assets: root.assets.len(),
            payload_bytes: self.payload_len as u64,
            text_bytes: self.text_len as u64,
            spans: self.span_count()?,
            export_labels,
        })
    }
}

/// Write stats for `path` to `out`.
pub fn write_stats(path: impl AsRef<Path>, mut out: impl Write) -> Result<(), Error> {
    let archive = Archive::open(path)?;
    write!(out, "{}", archive.stats()?)?;
    Ok(())
}

pub(crate) fn write_archive(
    path: &Path,
    root: &ArchiveRoot,
    text: &[u8],
    spans: &[TextSpan],
    bao: &PackedBao,
) -> Result<(), Error> {
    write_archive_with_text_align(path, root, text, spans, bao, true)
}

fn write_archive_with_text_align(
    path: &Path,
    root: &ArchiveRoot,
    text: &[u8],
    spans: &[TextSpan],
    bao: &PackedBao,
    align_text: bool,
) -> Result<(), Error> {
    let payload = crate::rkyv_to_bytes(root)?;
    u32_fit_usize(spans.len(), "text span count")?;
    let mut span_bytes = Vec::with_capacity(spans.len() * SPAN_LEN);
    for span in spans {
        span.write_bytes(&mut span_bytes);
    }
    let text_str =
        std::str::from_utf8(text).map_err(|_| Error::ingest("text blob is not utf-8"))?;
    let tries = crate::trie::pack_tries(text_str, spans)?;

    let payload_off = HEADER_LEN as u64;
    let payload_len = payload.len() as u64;
    let payload_end = payload_off
        .checked_add(payload_len)
        .ok_or_else(|| Error::ingest("payload end overflow"))?;
    let text_off = if align_text {
        align_up_to_text_page(payload_end)?
    } else {
        payload_end
    };
    let pad = text_off.saturating_sub(payload_end);
    let text_len = text.len() as u64;
    let spans_off = text_off
        .checked_add(text_len)
        .ok_or_else(|| Error::ingest("span offset overflow"))?;
    let spans_len = span_bytes.len() as u64;
    let tries_off = spans_off
        .checked_add(spans_len)
        .ok_or_else(|| Error::ingest("tries offset overflow"))?;
    let tries_len = tries.len() as u64;

    let mut header = [0u8; HEADER_LEN];
    header[0..MAGIC_LEN].copy_from_slice(&MAGIC);
    header[VERSION_OFF..VERSION_OFF + 4].copy_from_slice(&VERSION.to_le_bytes());
    header[HEADER_LEN_OFF..HEADER_LEN_OFF + 4].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    write_u64(&mut header, PAIRS_OFF, payload_off);
    write_u64(&mut header, PAIRS_OFF + 8, payload_len);
    write_u64(&mut header, PAIRS_OFF + 16, text_off);
    write_u64(&mut header, PAIRS_OFF + 24, text_len);
    write_u64(&mut header, PAIRS_OFF + 32, spans_off);
    write_u64(&mut header, PAIRS_OFF + 40, spans_len);
    write_u64(&mut header, PAIRS_OFF + 48, tries_off);
    write_u64(&mut header, PAIRS_OFF + 56, tries_len);

    let tmp_path = tmp_beside(path);
    let write_result = (|| {
        let mut file = File::create(&tmp_path)?;
        file.write_all(&header)?;
        file.write_all(payload.as_slice())?;
        write_zero_pad(&mut file, pad)?;
        file.write_all(text)?;
        file.write_all(&span_bytes)?;
        file.write_all(&tries)?;
        file.write_all(&bao.root)?;
        file.write_all(&(bao.blob.len() as u64).to_le_bytes())?;
        file.write_all(&(bao.outboard.len() as u64).to_le_bytes())?;
        file.write_all(&bao.blob)?;
        file.write_all(&bao.outboard)?;
        file.flush()?;
        Ok::<(), Error>(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    std::fs::rename(&tmp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp_path);
        Error::from(error)
    })?;
    Ok(())
}

fn align_up_to_text_page(value: u64) -> Result<u64, Error> {
    let align = TEXT_PAGE_BYTES as u64;
    let rem = value % align;
    if rem == 0 {
        Ok(value)
    } else {
        value
            .checked_add(align - rem)
            .ok_or_else(|| Error::ingest("text offset overflow while aligning to 2 MiB"))
    }
}

fn write_zero_pad(file: &mut File, pad: u64) -> io::Result<()> {
    if pad == 0 {
        return Ok(());
    }
    let buf = [0u8; 65536];
    let mut left = pad;
    while left > 0 {
        let n = usize::try_from(left.min(buf.len() as u64)).unwrap_or(buf.len());
        file.write_all(&buf[..n])?;
        left -= n as u64;
    }
    Ok(())
}

/// Read-only file map. Virtual address of a Linux mapping is 2 MiB aligned
/// when the reservation succeeds, so a 2 MiB-aligned text file offset sits
/// on a 2 MiB virtual address.
struct MappedBytes {
    inner: MappedInner,
}

enum MappedInner {
    #[cfg(unix)]
    Aligned(AlignedMmap),
    Memmap(Mmap),
}

#[cfg(unix)]
struct AlignedMmap {
    ptr: *mut libc::c_void,
    len: usize,
}

#[cfg(unix)]
unsafe impl Send for AlignedMmap {}
#[cfg(unix)]
unsafe impl Sync for AlignedMmap {}

#[cfg(unix)]
impl Drop for AlignedMmap {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.len > 0 {
            unsafe {
                let _ = libc::munmap(self.ptr, self.len);
            }
        }
    }
}

impl MappedBytes {
    fn as_slice(&self) -> &[u8] {
        match &self.inner {
            #[cfg(unix)]
            MappedInner::Aligned(mapped) => unsafe {
                std::slice::from_raw_parts(mapped.ptr as *const u8, mapped.len)
            },
            MappedInner::Memmap(mapped) => mapped,
        }
    }

    fn dont_need(&self) {
        #[cfg(unix)]
        {
            match &self.inner {
                MappedInner::Aligned(mapped) => unsafe {
                    let _ = libc::madvise(mapped.ptr, mapped.len, libc::MADV_DONTNEED);
                },
                MappedInner::Memmap(mapped) => {
                    // SAFETY: read-only file map. Search has finished. The
                    // caller does not use borrowed views after this returns.
                    let _ = unsafe { mapped.unchecked_advise(UncheckedAdvice::DontNeed) };
                }
            }
        }
    }
}

impl Deref for MappedBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

fn map_archive_file(file: &File) -> io::Result<MappedBytes> {
    #[cfg(unix)]
    {
        let len_u64 = file.metadata()?.len();
        if let Ok(len) = usize::try_from(len_u64)
            && len > 0
            && let Ok(aligned) = unsafe { map_file_two_mebibyte_aligned(file, len) }
        {
            return Ok(MappedBytes {
                inner: MappedInner::Aligned(aligned),
            });
        }
    }
    // SAFETY: Archive keeps this mapping for the life of MappedBytes. The
    // file is not truncated while the map is live.
    let mmap = unsafe { Mmap::map(file)? };
    Ok(MappedBytes {
        inner: MappedInner::Memmap(mmap),
    })
}

/// Reserve anonymous address space, then map the file with `PROT_READ` and
/// `MAP_SHARED` at a 2 MiB-aligned virtual address. Does not use `MAP_HUGETLB`.
///
/// # Safety
///
/// The file must stay open and must not be truncated while the returned map
/// is live.
#[cfg(unix)]
unsafe fn map_file_two_mebibyte_aligned(file: &File, len: usize) -> io::Result<AlignedMmap> {
    use std::os::fd::AsRawFd;

    let huge = TEXT_PAGE_BYTES;
    let reserve_len = len
        .checked_add(huge)
        .ok_or_else(|| io::Error::other("archive mapping length overflow"))?;
    let reserve = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            reserve_len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if reserve == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let hint = reserve as usize;
    let aligned = (hint + huge - 1) & !(huge - 1);
    let prefix = aligned.saturating_sub(hint);
    if prefix.checked_add(len).is_none_or(|end| end > reserve_len) {
        unsafe {
            libc::munmap(reserve, reserve_len);
        }
        return Err(io::Error::other(
            "aligned mapping does not fit the reservation",
        ));
    }
    let mapped = unsafe {
        libc::mmap(
            aligned as *mut libc::c_void,
            len,
            libc::PROT_READ,
            libc::MAP_SHARED | libc::MAP_FIXED,
            file.as_raw_fd(),
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        let err = io::Error::last_os_error();
        unsafe {
            libc::munmap(reserve, reserve_len);
        }
        return Err(err);
    }
    if prefix > 0 {
        unsafe {
            libc::munmap(reserve, prefix);
        }
    }
    let suffix_off = prefix + len;
    if suffix_off < reserve_len {
        unsafe {
            libc::munmap(
                (hint + suffix_off) as *mut libc::c_void,
                reserve_len - suffix_off,
            );
        }
    }
    Ok(AlignedMmap { ptr: mapped, len })
}

fn advise_text_pages(path: &Path, text: &[u8]) {
    if text.is_empty() {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        let ptr = text.as_ptr() as *mut libc::c_void;
        let len = text.len();
        // SAFETY: `text` is a subslice of the live file map.
        let _ = unsafe { libc::madvise(ptr, len, libc::MADV_HUGEPAGE) };
        if let Err(errno) = collapse_text_pages(ptr, len) {
            tracing::debug!(
                archive = %path.display(),
                errno,
                "MADV_COLLAPSE failed; keeping the mapped file"
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
    }
}

#[cfg(target_os = "linux")]
fn collapse_text_pages(ptr: *mut libc::c_void, len: usize) -> Result<(), i32> {
    #[cfg(test)]
    {
        if let Some(errno) = FORCE_COLLAPSE_ERRNO.with(std::cell::Cell::get) {
            return Err(errno);
        }
    }
    // SAFETY: `ptr`/`len` is the live text subslice of the file map.
    let rc = unsafe { libc::madvise(ptr, len, libc::MADV_COLLAPSE) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    }
}

struct BaoParts<'a> {
    root: [u8; 32],
    blob: &'a [u8],
    outboard: &'a [u8],
}

fn parse_bao_region(region: &[u8]) -> Result<BaoParts<'_>, Error> {
    if region.len() < BAO_PREFIX_LEN {
        return Err(Error::InvalidArchive);
    }
    let mut root = [0u8; 32];
    root.copy_from_slice(&region[..32]);
    let blob_len = usize::try_from(u64_from(region, 32)?).map_err(|_| Error::InvalidArchive)?;
    let outboard_len = usize::try_from(u64_from(region, 40)?).map_err(|_| Error::InvalidArchive)?;
    let blob_off = BAO_PREFIX_LEN;
    let outboard_off = blob_off
        .checked_add(blob_len)
        .ok_or(Error::InvalidArchive)?;
    let end = outboard_off
        .checked_add(outboard_len)
        .ok_or(Error::InvalidArchive)?;
    if region.len() != end {
        return Err(Error::InvalidArchive);
    }
    Ok(BaoParts {
        root,
        blob: &region[blob_off..outboard_off],
        outboard: &region[outboard_off..end],
    })
}

fn tmp_beside(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

fn read_u64(bytes: &[u8], off: usize) -> Result<usize, Error> {
    usize::try_from(u64_from(bytes, off)?).map_err(|_| Error::InvalidArchive)
}

pub(crate) fn u32_from(bytes: &[u8], off: usize) -> Result<u32, Error> {
    let slice = bytes.get(off..off + 4).ok_or(Error::InvalidArchive)?;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(slice);
    Ok(u32::from_le_bytes(buf))
}

pub(crate) fn u64_from(bytes: &[u8], off: usize) -> Result<u64, Error> {
    let slice = bytes.get(off..off + 8).ok_or(Error::InvalidArchive)?;
    let mut buf = [0u8; 8];
    buf.copy_from_slice(slice);
    Ok(u64::from_le_bytes(buf))
}

pub(crate) fn write_u64(bytes: &mut [u8], off: usize, value: u64) {
    bytes[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn slice_at(bytes: &[u8], off: usize, len: usize) -> Result<&[u8], Error> {
    let end = off.checked_add(len).ok_or(Error::InvalidArchive)?;
    bytes.get(off..end).ok_or(Error::InvalidArchive)
}

/// Convert a length or count into a `u32` on-disk field.
///
/// Pack uses this instead of `as u32` so a value past `u32::MAX` is [`Error`],
/// not a truncated field and not a rancor panic.
pub(crate) fn u32_fit(value: u64, what: &str) -> Result<u32, Error> {
    u32::try_from(value).map_err(|_| {
        Error::ingest(format!(
            "{what} is {value}, which does not fit in a 32-bit archive field (maximum is {})",
            u32::MAX
        ))
    })
}

/// [`u32_fit`] for a `usize` length (the `u32::try_from(len)` pack path).
pub(crate) fn u32_fit_usize(value: usize, what: &str) -> Result<u32, Error> {
    let value = u64::try_from(value).map_err(|_| {
        Error::ingest(format!(
            "{what} is {value}, which does not fit in a 32-bit archive field (maximum is {})",
            u32::MAX
        ))
    })?;
    u32_fit(value, what)
}

#[cfg(test)]
mod u32_fit_tests {
    use super::{u32_fit, u32_fit_usize};

    #[test]
    fn u32_fit_overflow_returns_error_not_panic() {
        let err = u32_fit(u64::from(u32::MAX) + 1, "payload length")
            .expect_err("a value past u32::MAX must be Error, not a panic");
        let message = err.to_string();
        assert!(
            message.contains("does not fit in a 32-bit archive field"),
            "plain English fit error, got {message}"
        );
        assert!(
            message.contains("payload length"),
            "error must name the field, got {message}"
        );
    }

    #[test]
    fn u32_fit_accepts_u32_max() {
        assert_eq!(
            u32_fit(u64::from(u32::MAX), "payload length").expect("u32::MAX fits"),
            u32::MAX
        );
        assert_eq!(u32_fit(0, "payload length").expect("zero fits"), 0);
        assert_eq!(
            u32_fit_usize(u32::MAX as usize, "span count").expect("usize u32::MAX fits"),
            u32::MAX
        );
    }
}

#[cfg(test)]
mod text_page_tests {
    use std::path::PathBuf;

    use super::*;
    use crate::hash::pack_blob;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("majestic-text-page-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-export.json")
    }

    #[test]
    fn new_archive_text_starts_on_two_mebibyte_file_offset() {
        let dir = scratch("aligned");
        let path = dir.join("archive.majestic");
        crate::ingest::ingest(&path, &[fixture()]).expect("ingest fixture");
        let archive = Archive::open(&path).expect("mmap open");
        assert_eq!(
            archive.text_file_offset() % TEXT_PAGE_BYTES as u64,
            0,
            "new ingest must start the UTF-8 text on a 2 MiB file offset, got {}",
            archive.text_file_offset()
        );
        assert!(
            archive.text_slice_is_mmap_view(),
            "search must use the mmap text slice, not a heap copy of the blob"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_unaligned_text_offset_still_searches() {
        let dir = scratch("unaligned");
        let padded = dir.join("padded.majestic");
        crate::ingest::ingest(&padded, &[fixture()]).expect("ingest fixture");
        let archive = Archive::open(&padded).expect("open padded");
        let root = archive.deserialize_root().expect("root");
        let text = archive.text().expect("text").as_bytes().to_vec();
        let mut spans = Vec::new();
        for index in 0..archive.span_count().expect("span count") {
            spans.push(archive.span(index).expect("span"));
        }
        let bao = pack_blob(archive.bao_blob().expect("bao blob").to_vec());
        drop(archive);
        let old = dir.join("old.majestic");
        write_archive_with_text_align(&old, &root, &text, &spans, &bao, false)
            .expect("write old short layout");
        let archive = Archive::open(&old).expect("old short archive still opens");
        assert_ne!(
            archive.text_file_offset() % TEXT_PAGE_BYTES as u64,
            0,
            "old short layout must keep an unaligned text file offset, got {}",
            archive.text_file_offset()
        );
        assert!(
            archive.text_slice_is_mmap_view(),
            "search must use the mmap text slice, not a heap copy of the blob"
        );
        let hits = crate::search(&archive, "Catfooding", false).expect("search old layout");
        assert!(
            !hits.is_empty(),
            "old short archives must still search, got {hits:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn forced_madvise_collapse_errno_does_not_abort_open() {
        struct ResetForce;
        impl Drop for ResetForce {
            fn drop(&mut self) {
                FORCE_COLLAPSE_ERRNO.with(|cell| cell.set(None));
            }
        }
        let _reset = ResetForce;
        let dir = scratch("force-collapse");
        let path = dir.join("archive.majestic");
        crate::ingest::ingest(&path, &[fixture()]).expect("ingest fixture");
        FORCE_COLLAPSE_ERRNO.with(|cell| cell.set(Some(libc::EINVAL)));
        Archive::open(&path).expect("forced MADV_COLLAPSE errno must not abort Archive::open");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
