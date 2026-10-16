//! Delta engine: find which parts of a new file already exist in an old ("basis")
//! file, so only the rest has to be written (and, for remote syncs, sent).
//!
//! Two signature schemes, both computed in parallel over fixed segments:
//!
//! - **Aligned blocks**: hash every block at the same offsets in both files. Ideal for
//!   files modified in place or appended to (VM images, databases, logs). Matches are
//!   always "in place" (same offset), which lets the receiver clone the basis and write
//!   only the changed blocks.
//! - **Content-defined chunks** (FastCDC): chunk boundaries follow the content, so data
//!   that moved (inserts/deletes) still matches. Used as the fallback when aligned
//!   blocks match poorly. Segment edges are forced cut points, so chunking is
//!   deterministic and parallel; a shift costs at most one extra chunk per segment.
//!
//! Matching uses BLAKE3 truncated to 128 bits, so every reused range is
//! cryptographically verified (no separate whole-file pass, which would have to
//! re-read the entire output).

pub mod apply;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

pub type Hash128 = [u8; 16];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// Aligned block size.
    pub block_size: usize,
    /// Parallel work unit and CDC forced-cut interval; a multiple of `block_size`.
    pub segment: usize,
    pub cdc_min: usize,
    pub cdc_avg: usize,
    pub cdc_max: usize,
}

impl Params {
    pub const DEFAULT: Params = Params {
        block_size: 64 << 10,
        segment: 4 << 20,
        cdc_min: 16 << 10,
        cdc_avg: 32 << 10,
        cdc_max: 64 << 10,
    };
}

/// Files smaller than this are always sent whole: a delta can't save enough to matter.
pub const MIN_DELTA_SIZE: u64 = 64 << 10;

pub fn strong_hash(data: &[u8]) -> Hash128 {
    blake3::hash(data).as_bytes()[..16].try_into().unwrap()
}

/// Positional reads, so many threads can read one source concurrently.
/// Implemented for local files; the remote receiver will implement it over received data.
pub trait ReadAt: Sync {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()>;
}

impl ReadAt for File {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        FileExt::read_exact_at(self, buf, off)
    }
}

impl ReadAt for Vec<u8> {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        let off = usize::try_from(off).map_err(|_| io::ErrorKind::UnexpectedEof)?;
        let src = self.get(off..off + buf.len()).ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

thread_local! {
    static SEG_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Read `len` bytes at `off` into a per-thread buffer and pass them to `f`.
/// `f` must not re-enter this function (it runs while the buffer is borrowed).
pub(crate) fn with_range<T>(src: &dyn ReadAt, off: u64, len: usize, f: impl FnOnce(&mut [u8]) -> T) -> io::Result<T> {
    SEG_BUF.with(|b| {
        let mut buf = b.borrow_mut();
        if buf.len() < len {
            buf.resize(len, 0);
        }
        src.read_exact_at(&mut buf[..len], off)?;
        Ok(f(&mut buf[..len]))
    })
}

/// (offset, length) of each `segment`-sized piece of a `size`-byte file.
fn segments(size: u64, segment: usize) -> Vec<(u64, usize)> {
    let seg = segment as u64;
    (0..size.div_ceil(seg)).map(|i| (i * seg, (size - i * seg).min(seg) as usize)).collect()
}

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

/// One step of rebuilding the new file; ops are ordered by `off` and cover `0..size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// `new[off..off+len] == basis[basis_off..basis_off+len]`.
    Copy { off: u64, basis_off: u64, len: u64 },
    /// `new[off..off+len]` must come from the new file itself.
    Literal { off: u64, len: u64 },
}

impl Op {
    pub fn off(&self) -> u64 {
        match *self {
            Op::Copy { off, .. } | Op::Literal { off, .. } => off,
        }
    }

    pub fn len(&self) -> u64 {
        match *self {
            Op::Copy { len, .. } | Op::Literal { len, .. } => len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Delta {
    /// Size of the new file.
    pub size: u64,
    pub ops: Vec<Op>,
}

impl Delta {
    pub fn new(size: u64) -> Delta {
        Delta { size, ops: Vec::new() }
    }

    /// Append an op, merging it into the previous one when they are contiguous.
    pub fn push(&mut self, op: Op) {
        if op.is_empty() {
            return;
        }
        match (self.ops.last_mut(), op) {
            (Some(Op::Literal { off, len }), Op::Literal { off: o2, len: l2 }) if *off + *len == o2 => *len += l2,
            (Some(Op::Copy { off, basis_off, len }), Op::Copy { off: o2, basis_off: b2, len: l2 })
                if *off + *len == o2 && *basis_off + *len == b2 =>
            {
                *len += l2
            }
            _ => self.ops.push(op),
        }
    }

    pub fn literal_bytes(&self) -> u64 {
        self.ops.iter().filter(|o| matches!(o, Op::Literal { .. })).map(Op::len).sum()
    }

    pub fn matched_bytes(&self) -> u64 {
        self.size - self.literal_bytes()
    }

    /// Bytes that are already at the right offset in the basis.
    pub fn in_place_bytes(&self) -> u64 {
        self.ops
            .iter()
            .filter(|o| matches!(o, Op::Copy { off, basis_off, .. } if off == basis_off))
            .map(Op::len)
            .sum()
    }

    /// Every matched range sits at the same offset in both files, so the new file can
    /// be produced by overwriting literal ranges of the basis in place.
    pub fn is_in_place(&self) -> bool {
        self.ops.iter().all(|o| match o {
            Op::Copy { off, basis_off, .. } => off == basis_off,
            Op::Literal { .. } => true,
        })
    }
}

// ---------------------------------------------------------------------------
// Aligned blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockSig {
    pub block_size: u32,
    pub size: u64,
    pub hashes: Vec<Hash128>,
}

pub fn block_signature(src: &dyn ReadAt, size: u64, p: &Params) -> io::Result<BlockSig> {
    debug_assert_eq!(p.segment % p.block_size, 0);
    let per_seg: Vec<Vec<Hash128>> = segments(size, p.segment)
        .into_par_iter()
        .map(|(off, len)| with_range(src, off, len, |buf| buf.chunks(p.block_size).map(strong_hash).collect()))
        .collect::<io::Result<_>>()?;
    Ok(BlockSig { block_size: p.block_size as u32, size, hashes: per_seg.into_iter().flatten().collect() })
}

/// Compare two aligned-block signatures computed with the same block size.
pub fn diff_blocks(new: &BlockSig, basis: &BlockSig) -> Delta {
    assert_eq!(new.block_size, basis.block_size, "block sizes must match");
    let bs = new.block_size as u64;
    let mut d = Delta::new(new.size);
    for (i, h) in new.hashes.iter().enumerate() {
        let off = i as u64 * bs;
        let len = bs.min(new.size - off);
        let basis_len = if off < basis.size { bs.min(basis.size - off) } else { 0 };
        if basis_len == len && basis.hashes.get(i) == Some(h) {
            d.push(Op::Copy { off, basis_off: off, len });
        } else {
            d.push(Op::Literal { off, len });
        }
    }
    d
}

// ---------------------------------------------------------------------------
// Content-defined chunks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkEntry {
    pub off: u64,
    pub len: u32,
    pub hash: Hash128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkSig {
    pub size: u64,
    pub chunks: Vec<ChunkEntry>,
}

pub fn chunk_signature(src: &dyn ReadAt, size: u64, p: &Params) -> io::Result<ChunkSig> {
    let per_seg: Vec<Vec<ChunkEntry>> = segments(size, p.segment)
        .into_par_iter()
        .map(|(seg_off, len)| {
            with_range(src, seg_off, len, |buf| {
                fastcdc::v2020::FastCDC::new(buf, p.cdc_min, p.cdc_avg, p.cdc_max)
                    .map(|c| ChunkEntry {
                        off: seg_off + c.offset as u64,
                        len: c.length as u32,
                        hash: strong_hash(&buf[c.offset..c.offset + c.length]),
                    })
                    .collect()
            })
        })
        .collect::<io::Result<_>>()?;
    Ok(ChunkSig { size, chunks: per_seg.into_iter().flatten().collect() })
}

/// Match new-file chunks against basis chunks by content. A chunk found at the same
/// offset in the basis is preferred, which keeps the delta in place where possible.
pub fn diff_chunks(new: &ChunkSig, basis: &ChunkSig) -> Delta {
    let mut by_hash: HashMap<Hash128, (u64, u32)> = HashMap::with_capacity(basis.chunks.len());
    let mut at_off: HashMap<u64, Hash128> = HashMap::with_capacity(basis.chunks.len());
    for c in &basis.chunks {
        by_hash.entry(c.hash).or_insert((c.off, c.len));
        at_off.insert(c.off, c.hash);
    }
    let mut d = Delta::new(new.size);
    for c in &new.chunks {
        let len = c.len as u64;
        if at_off.get(&c.off) == Some(&c.hash) && c.off + len <= basis.size {
            d.push(Op::Copy { off: c.off, basis_off: c.off, len });
        } else if let Some(&(boff, blen)) = by_hash.get(&c.hash)
            && blen == c.len
        {
            d.push(Op::Copy { off: c.off, basis_off: boff, len });
        } else {
            d.push(Op::Literal { off: c.off, len });
        }
    }
    d
}

// ---------------------------------------------------------------------------
// Strategy
// ---------------------------------------------------------------------------

/// Compute a delta when both files are local: hash both concurrently with aligned
/// blocks, and fall back to content-defined chunking when fewer than half of the
/// bytes matched.
pub fn local_delta(new: &File, new_size: u64, basis: &File, basis_size: u64, p: &Params) -> io::Result<Delta> {
    let (ns, bs) = rayon::join(|| block_signature(new, new_size, p), || block_signature(basis, basis_size, p));
    let aligned = diff_blocks(&ns?, &bs?);
    if aligned.matched_bytes() * 2 >= new_size {
        return Ok(aligned);
    }
    let (nc, bc) = rayon::join(|| chunk_signature(new, new_size, p), || chunk_signature(basis, basis_size, p));
    let cdc = diff_chunks(&nc?, &bc?);
    Ok(if cdc.matched_bytes() > aligned.matched_bytes() { cdc } else { aligned })
}

#[cfg(test)]
mod tests;
