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
//!   that moved (inserts/deletes) still matches. Used when aligned blocks match poorly
//!   (locally) or the file changed size (remotely), merged with the aligned-block delta
//!   so data that stayed in place still matches block by block ([`merge_in_place`]).
//!   Chunked in parallel per segment and joined at the segment edges so the cuts are
//!   exactly those of one sequential pass (see [`chunk_signature`]).
//!
//! Block and chunk sizes scale with the file, from 4 KiB to 64 KiB
//! ([`Params::for_size`]).
//!
//! Matching uses BLAKE3 truncated to 128 bits, so every reused range is
//! cryptographically verified (no separate whole-file pass, which would have to
//! re-read the entire output).
//!
//! That is [`DeltaMode::Blocks`]. [`DeltaMode::Cdc`] is the original plan's design
//! instead: see [`cdc`].

pub mod apply;
pub mod cdc;
pub mod refine;

use crate::fsops::cache;
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
    /// `--delta=cdc`: unit of parallel chunking, stitched afterwards (see [`cdc`]).
    pub cdc_region: usize,
}

impl Params {
    /// Coarse sizes: the first signature of every delta, `--delta=cdc` throughout, and the
    /// upper bound of [`Params::for_size`].
    pub const DEFAULT: Params = Params {
        block_size: 64 << 10,
        segment: 4 << 20,
        cdc_min: 16 << 10,
        cdc_avg: 32 << 10,
        cdc_max: 64 << 10,
        cdc_region: 16 << 20,
    };

    /// Fine block and chunk sizes, used to refine what a coarse delta left literal
    /// ([`refine`]) for a basis of `size` bytes: a quarter of √size rounded up to a power of
    /// two, between 4 KiB and 64 KiB (4 KiB up to 256 MiB, 8 KiB up to 1 GiB, 16 KiB up to
    /// 4 GiB, 32 KiB up to 16 GiB, 64 KiB beyond). Chunks average the block size (half to
    /// twice it). Measured in `fast_rsync_comparison.md`.
    pub fn for_size(size: u64) -> Params {
        let unit = ((size.isqrt() / 4) as usize).next_power_of_two().clamp(4 << 10, Params::DEFAULT.block_size);
        Params { block_size: unit, cdc_min: unit / 2, cdc_avg: unit, cdc_max: unit * 2, ..Params::DEFAULT }
    }
}

/// Which delta design to use (`--delta`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum DeltaMode {
    /// Aligned blocks first, FastCDC fallback; BLAKE3-128; `pread`.
    #[default]
    Blocks,
    /// FastCDC for every delta; xxh3 filter confirmed by BLAKE3; `mmap`.
    Cdc,
}

/// Files of at most this size are always sent whole: a delta can't save enough to matter.
pub const MIN_DELTA_SIZE: u64 = 128 << 10;

/// Whether a new file of `size` bytes may be sent as a delta.
pub fn worth_delta(size: u64) -> bool {
    size > MIN_DELTA_SIZE
}

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

/// A file that is read once (hashed, then maybe read again for the rebuild), with
/// page-cache hints when it is big: see [`crate::fsops::cache`].
pub struct ReadOnce<'a> {
    file: &'a File,
    hinted: bool,
}

impl<'a> ReadOnce<'a> {
    pub fn new(file: &'a File, size: u64) -> ReadOnce<'a> {
        ReadOnce { file, hinted: cache::read_once(file, size) }
    }
}

impl ReadAt for ReadOnce<'_> {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        FileExt::read_exact_at(self.file, buf, off)?;
        if self.hinted {
            cache::drop_read(self.file, off, buf.len() as u64);
        }
        Ok(())
    }
}

impl Drop for ReadOnce<'_> {
    /// One thread's read-ahead can pull in pages of a range another thread has already
    /// read and dropped, so sweep the whole file once at the end.
    fn drop(&mut self) {
        if self.hinted {
            cache::drop_read(self.file, 0, 0);
        }
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
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
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
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
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

/// One content-defined chunk. Chunks tile the file in order, so offsets are implied
/// (see [`ChunkSig::iter`]) and not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct ChunkEntry {
    pub len: u32,
    pub hash: Hash128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct ChunkSig {
    pub size: u64,
    /// FastCDC sizes the chunks were cut with; the other side must cut its file the same way.
    pub min: u32,
    pub avg: u32,
    pub max: u32,
    pub chunks: Vec<ChunkEntry>,
}

impl ChunkSig {
    /// `(offset, length, hash)` of every chunk.
    pub fn iter(&self) -> impl Iterator<Item = (u64, u32, &Hash128)> + '_ {
        self.chunks.iter().scan(0u64, |off, c| {
            let at = *off;
            *off += c.len as u64;
            Some((at, c.len, &c.hash))
        })
    }

    /// `p` with this signature's chunk sizes, or `None` if FastCDC would reject them
    /// (the signature comes from the other side).
    pub fn params(&self, p: Params) -> Option<Params> {
        with_cdc(p, self.min, self.avg, self.max)
    }
}

/// `p` with these FastCDC sizes, or `None` if FastCDC would reject them.
pub(crate) fn with_cdc(p: Params, min: u32, avg: u32, max: u32) -> Option<Params> {
    use fastcdc::v2020::{AVERAGE_MAX, AVERAGE_MIN, MAXIMUM_MAX, MAXIMUM_MIN, MINIMUM_MAX, MINIMUM_MIN};
    let (min, avg, max) = (min as usize, avg as usize, max as usize);
    let ok = (MINIMUM_MIN..=MINIMUM_MAX).contains(&min)
        && (AVERAGE_MIN..=AVERAGE_MAX).contains(&avg)
        && (MAXIMUM_MIN..=MAXIMUM_MAX).contains(&max)
        && min <= avg
        && avg <= max;
    ok.then_some(Params { cdc_min: min, cdc_avg: avg, cdc_max: max, ..p })
}

/// Content-defined chunks, cut exactly where one sequential FastCDC pass over the whole
/// file would cut (the original plan's merging at region boundaries; [`cdc`] explains why
/// this works), computed in parallel with one read of the file.
///
/// Each segment is read with `cdc_max` extra bytes and chunked from its first byte until a
/// chunk ends at or past the segment's end; with the extra bytes every one of those cuts is
/// real. A sequential pass then joins the segments: from the previous segment's last cut it
/// chunks on (reading `cdc_max`-byte windows) until it reaches one of this segment's cuts,
/// and takes the rest of the segment's chunks from there.
pub fn chunk_signature(src: &dyn ReadAt, size: u64, p: &Params) -> io::Result<ChunkSig> {
    // Per segment: cuts as (offset, length, hash).
    let per_seg: Vec<Vec<(u64, u32, Hash128)>> = segments(size, p.segment)
        .into_par_iter()
        .map(|(seg_off, len)| {
            let seg_end = seg_off + len as u64;
            let read = (size - seg_off).min((len + p.cdc_max) as u64) as usize;
            with_range(src, seg_off, read, |buf| {
                let mut cuts = Vec::new();
                for c in fastcdc::v2020::FastCDC::new(buf, p.cdc_min, p.cdc_avg, p.cdc_max) {
                    cuts.push((seg_off + c.offset as u64, c.length as u32, strong_hash(&buf[c.offset..c.offset + c.length])));
                    if seg_off + (c.offset + c.length) as u64 >= seg_end {
                        break;
                    }
                }
                cuts
            })
        })
        .collect::<io::Result<_>>()?;
    let mut chunks: Vec<ChunkEntry> = Vec::with_capacity(per_seg.iter().map(Vec::len).sum());
    let mut pos = 0u64;
    for seg in per_seg {
        let Some(last) = seg.last().map(|c| c.0 + c.1 as u64) else { continue };
        while pos < last {
            if let Ok(i) = seg.binary_search_by_key(&pos, |c| c.0) {
                chunks.extend(seg[i..].iter().map(|&(_, len, hash)| ChunkEntry { len, hash }));
                pos = last;
                break;
            }
            let window = (size - pos).min(p.cdc_max as u64) as usize;
            let c = with_range(src, pos, window, |buf| {
                let n = fastcdc::v2020::FastCDC::new(buf, p.cdc_min, p.cdc_avg, p.cdc_max).next().map_or(buf.len(), |c| c.length);
                ChunkEntry { len: n as u32, hash: strong_hash(&buf[..n]) }
            })?;
            pos += c.len as u64;
            chunks.push(c);
        }
    }
    debug_assert_eq!(pos, size);
    let (min, avg, max) = (p.cdc_min as u32, p.cdc_avg as u32, p.cdc_max as u32);
    Ok(ChunkSig { size, min, avg, max, chunks })
}

/// Match new-file chunks against basis chunks by content. A chunk found at the same
/// offset in the basis is preferred, which keeps the delta in place where possible.
pub fn diff_chunks(new: &ChunkSig, basis: &ChunkSig) -> Delta {
    let mut by_hash: HashMap<Hash128, (u64, u32)> = HashMap::with_capacity(basis.chunks.len());
    let mut at_off: HashMap<u64, Hash128> = HashMap::with_capacity(basis.chunks.len());
    for (off, len, hash) in basis.iter() {
        by_hash.entry(*hash).or_insert((off, len));
        at_off.insert(off, *hash);
    }
    let mut d = Delta::new(new.size);
    for (off, clen, hash) in new.iter() {
        let len = clen as u64;
        if at_off.get(&off) == Some(hash) && off + len <= basis.size {
            d.push(Op::Copy { off, basis_off: off, len });
        } else if let Some(&(boff, blen)) = by_hash.get(hash)
            && blen == clen
        {
            d.push(Op::Copy { off, basis_off: boff, len });
        } else {
            d.push(Op::Literal { off, len });
        }
    }
    d
}

/// Combine a content-defined-chunk delta with an aligned-block delta of the same new
/// file: every literal range of `chunks` that `blocks` found unchanged at the same offset
/// becomes a copy. The result never has more literal bytes than either input, so a file
/// that grew but kept its data in place (a database, a VM image) costs only its changed
/// blocks, while one whose data moved still matches by content.
pub fn merge_in_place(chunks: &Delta, blocks: &Delta) -> Delta {
    debug_assert_eq!(chunks.size, blocks.size);
    let copies: Vec<(u64, u64, u64)> = blocks
        .ops
        .iter()
        .filter_map(|o| match *o {
            Op::Copy { off, basis_off, len } => Some((off, basis_off, len)),
            Op::Literal { .. } => None,
        })
        .collect();
    let mut d = Delta::new(chunks.size);
    overlay(&mut d, &chunks.ops, &copies);
    d
}

/// Push `ops` into `out`, with every part of a literal op that one of `copies`
/// (`(off, basis_off, len)`, sorted by `off`, disjoint) covers turned into a copy.
pub(crate) fn overlay(out: &mut Delta, ops: &[Op], copies: &[(u64, u64, u64)]) {
    let mut i = 0;
    for op in ops {
        let Op::Literal { off, len } = *op else {
            out.push(*op);
            continue;
        };
        let (mut pos, end) = (off, off + len);
        while i < copies.len() && copies[i].0 + copies[i].2 <= pos {
            i += 1;
        }
        while i < copies.len() && copies[i].0 < end {
            let (c_off, c_basis, c_len) = copies[i];
            let (s, e) = (c_off.max(pos), (c_off + c_len).min(end));
            out.push(Op::Literal { off: pos, len: s - pos });
            out.push(Op::Copy { off: s, basis_off: c_basis + (s - c_off), len: e - s });
            pos = e;
            if c_off + c_len > end {
                break; // the copy continues past this literal
            }
            i += 1;
        }
        out.push(Op::Literal { off: pos, len: end - pos });
    }
}

// ---------------------------------------------------------------------------
// Strategy
// ---------------------------------------------------------------------------

/// Compute a delta when both files are local: hash both concurrently with aligned
/// blocks of `p`, and when fewer than half of the bytes matched, chunk both by content and
/// merge the two deltas ([`merge_in_place`]). Then refine what is still literal with the
/// file's finer sizes ([`refine`]).
pub fn local_delta(new: &dyn ReadAt, new_size: u64, basis: &dyn ReadAt, basis_size: u64, p: &Params) -> io::Result<Delta> {
    let (ns, bs) = rayon::join(|| block_signature(new, new_size, p), || block_signature(basis, basis_size, p));
    let aligned = diff_blocks(&ns?, &bs?);
    let coarse = if aligned.matched_bytes() * 2 >= new_size {
        aligned
    } else {
        let (nc, bc) = rayon::join(|| chunk_signature(new, new_size, p), || chunk_signature(basis, basis_size, p));
        merge_in_place(&diff_chunks(&nc?, &bc?), &aligned)
    };
    let fine = Params::for_size(basis_size);
    match refine::plan(&coarse, basis_size, p.block_size.min(p.cdc_avg), &fine) {
        Some(gaps) => refine::local(&coarse, new, new_size, basis, basis_size, &fine, &gaps),
        None => Ok(coarse),
    }
}

#[cfg(test)]
mod tests;
