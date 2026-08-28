//! Bao-tree hashing of canonical conversation bytes and uploaded file bytes.
//!
//! Conversation identity compares id, byte length, mtime, the BaoTree root of
//! the item encoding, and a sub-hash (BaoTree root of the responses encoding).
//! Asset identity is the BaoTree root of the `content` file bytes (the tree of
//! those bytes, not uuid, size, or mtime). Those digests are tree queries, not
//! fields on records. The file body is a range in this blob (`blob_off` /
//! `blob_len`).
//!
//! One concatenated blob of conversation encodings and uploaded file bodies is
//! covered by one outboard. The archive-wide root lives in a tiny prefix after
//! the tries region.
//!
//! Sync hash API: [`bao_tree::io::outboard::PostOrderMemOutboard::create`]
//! ([bao-tree 0.16](https://docs.rs/bao-tree/0.16.0/bao_tree/), accessed:
//! 2026-08-25).

use std::fs::File;
use std::path::Path;

use bao_tree::BlockSize;
use bao_tree::io::outboard::PostOrderMemOutboard;
use memmap2::Mmap;

use crate::Error;
use crate::schema::{ConversationItem, ResponseItem};

/// 1024-byte blocks, matching a BLAKE3 chunk.
pub const BLOCK_SIZE: BlockSize = BlockSize::ZERO;

/// Tiny bao prefix: root [32], blob_len u64, outboard_len u64.
pub const BAO_PREFIX_LEN: usize = 48;

/// BaoTree root length (32 bytes). Asset identity uses this digest as a
/// query, not a column on the catalog row. File bodies are stored in full.
pub const ROOT_LEN: usize = 32;

/// BaoTree root of a byte slice. Not stored on conversation or asset rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Digest {
    pub root: [u8; 32],
}

impl Digest {
    /// Lowercase hex of the 32-byte root. Used as a response id prefix.
    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(ROOT_LEN * 2);
        for byte in self.root {
            out.push(char::from(HEX[(byte >> 4) as usize]));
            out.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
        out
    }
}

/// Concatenated canonical bytes plus the single outboard that covers them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedBao {
    pub root: [u8; 32],
    pub blob: Vec<u8>,
    pub outboard: Vec<u8>,
}

/// rkyv encoding of a conversation wrapper. This is the hashed canonical form.
pub fn encode_item(item: &ConversationItem) -> Result<Vec<u8>, Error> {
    let bytes = crate::rkyv_to_bytes(item)?;
    Ok(bytes.to_vec())
}

/// Concatenated rkyv encodings of each response (inner sub-hash payload).
pub fn encode_responses(responses: &[ResponseItem]) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    for item in responses {
        bytes.extend_from_slice(&encode_response(item)?);
    }
    Ok(bytes)
}

/// rkyv encoding of one response wrapper (same `_id`, different body).
pub fn encode_response(item: &ResponseItem) -> Result<Vec<u8>, Error> {
    let bytes = crate::rkyv_to_bytes(item)?;
    Ok(bytes.to_vec())
}

/// BaoTree root of `data`.
pub fn hash_bytes(data: &[u8]) -> Digest {
    let outboard = PostOrderMemOutboard::create(data, BLOCK_SIZE);
    Digest {
        root: *outboard.root.as_bytes(),
    }
}

/// BaoTree root of a file's bytes. Empty files hash the empty slice.
///
/// Used to skip duplicate Facebook zip bytes (same size, then this digest).
pub fn hash_path(path: &Path) -> Result<Digest, Error> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    if size == 0 {
        return Ok(hash_bytes(&[]));
    }
    // SAFETY: read-only mmap of a file we opened. We do not truncate it
    // while hashing.
    let mmap = unsafe { Mmap::map(&file)? };
    Ok(hash_bytes(&mmap))
}

/// Root of the conversation encoding.
pub fn hash_item(item: &ConversationItem) -> Result<Digest, Error> {
    Ok(hash_bytes(&encode_item(item)?))
}

/// Sub-hash: BaoTree root of the concatenated response encodings.
pub fn hash_responses(responses: &[ResponseItem]) -> Result<Digest, Error> {
    Ok(hash_bytes(&encode_responses(responses)?))
}

/// Sub-hash of one response wrapper.
pub fn hash_response(item: &ResponseItem) -> Result<Digest, Error> {
    Ok(hash_bytes(&encode_response(item)?))
}

/// Hash a stored byte range of the bao-covered blob (tree over that slice).
pub fn hash_range(blob: &[u8], off: u64, len: u64) -> Result<Digest, Error> {
    Ok(hash_bytes(blob_slice(blob, off, len)?))
}

fn blob_slice(blob: &[u8], off: u64, len: u64) -> Result<&[u8], Error> {
    let start = usize::try_from(off).map_err(|_| Error::InvalidArchive)?;
    let extra = usize::try_from(len).map_err(|_| Error::InvalidArchive)?;
    let end = start.checked_add(extra).ok_or(Error::InvalidArchive)?;
    blob.get(start..end).ok_or(Error::InvalidArchive)
}

/// One outboard covering `blob`.
pub fn pack_blob(blob: Vec<u8>) -> PackedBao {
    let outboard = PostOrderMemOutboard::create(&blob, BLOCK_SIZE);
    PackedBao {
        root: *outboard.root.as_bytes(),
        outboard: outboard.data,
        blob,
    }
}
