//! The original plan's delta design (`--delta=cdc`): content-defined chunks for every
//! delta, an xxh3 hash table as a fast filter whose matches only count once BLAKE3
//! agrees, and files read through `mmap`.
//!
//! Chunking runs in parallel over [`Params::cdc_region`]-sized regions and is then
//! stitched, so the result is exactly what one sequential FastCDC pass over the whole
//! file produces, whatever the thread count. FastCDC restarts its rolling hash at every
//! cut, so the next cut depends only on where the current chunk starts. Each region is
//! chunked on its own from its first byte. Then, region by region, the sequential chain
//! continues from the last cut before the region until it lands on one of the region's
//! own cuts; from there the two chains are the same. A region's last chunk ends where
//! its data ends rather than at a real cut, so it is never used, except at the end of
//! the file.
//!
//! Mapped files must not shrink while they are read: touching a page past the new end
//! of the file raises `SIGBUS`. The size is checked after mapping (a file that already
//! changed falls back to a whole-file transfer), but not during the read.

use super::{Delta, Op, Params, ReadAt};
use crate::fsops::cache;
use fastcdc::v2020::FastCDC;
use memmap2::{Advice, Mmap};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use xxhash_rust::xxh3::xxh3_64;

pub type Blake3 = [u8; 32];

/// One chunk of a basis file, as the receiver sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct ChunkSignature {
    pub offset: u64,
    pub length: u32,
    pub xxh3_hash: u64,
    pub blake3_hash: Blake3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct CdcSig {
    pub size: u64,
    pub chunks: Vec<ChunkSignature>,
}

/// Chunks per rayon task when hashing.
const HASH_BATCH: usize = 64;

/// Chunk boundaries `(offset, length)` of `data`: those of one sequential FastCDC pass.
pub fn chunks(data: &[u8], p: &Params) -> Vec<(u64, u32)> {
    let len = data.len();
    let region = p.cdc_region.max(2 * p.cdc_max);
    let regions: Vec<(usize, usize)> = (0..len.div_ceil(region)).map(|i| (i * region, len.min((i + 1) * region))).collect();
    let ends: Vec<Vec<usize>> = regions
        .par_iter()
        .map(|&(s, e)| FastCDC::new(&data[s..e], p.cdc_min, p.cdc_avg, p.cdc_max).map(|c| s + c.offset + c.length).collect())
        .collect();
    let mut out = Vec::with_capacity(len / p.cdc_avg + 1);
    let mut pos = 0;
    for (&(start, end), ends) in regions.iter().zip(&ends) {
        let real = if end == len { &ends[..] } else { &ends[..ends.len() - 1] };
        while pos < end {
            if pos == start || real.binary_search(&pos).is_ok() {
                // On the region's own chain: the rest of it is the sequential result.
                for &cut in &real[real.partition_point(|&c| c <= pos)..] {
                    out.push((pos as u64, (cut - pos) as u32));
                    pos = cut;
                }
                break;
            }
            let cut = pos + next_cut(&data[pos..len.min(pos + p.cdc_max)], p);
            out.push((pos as u64, (cut - pos) as u32));
            pos = cut;
        }
    }
    debug_assert_eq!(pos, len);
    out
}

/// Length of the chunk starting at `data[0]` (`data` holds at most `cdc_max` bytes).
fn next_cut(data: &[u8], p: &Params) -> usize {
    FastCDC::new(data, p.cdc_min, p.cdc_avg, p.cdc_max).next().map_or(data.len(), |c| c.length)
}

/// One sequential FastCDC pass (the reference the parallel version must equal).
pub fn chunks_sequential(data: &[u8], p: &Params) -> Vec<(u64, u32)> {
    FastCDC::new(data, p.cdc_min, p.cdc_avg, p.cdc_max).map(|c| (c.offset as u64, c.length as u32)).collect()
}

fn slice(data: &[u8], (off, len): (u64, u32)) -> &[u8] {
    &data[off as usize..off as usize + len as usize]
}

fn blake3(b: &[u8]) -> Blake3 {
    *blake3::hash(b).as_bytes()
}

pub fn signature(data: &[u8], p: &Params) -> CdcSig {
    let chunks = chunks(data, p)
        .into_par_iter()
        .with_min_len(HASH_BATCH)
        .map(|c| {
            let b = slice(data, c);
            ChunkSignature { offset: c.0, length: c.1, xxh3_hash: xxh3_64(b), blake3_hash: blake3(b) }
        })
        .collect();
    CdcSig { size: data.len() as u64, chunks }
}

/// What the matcher needs to know about the basis: its chunks with their xxh3 hashes,
/// and a BLAKE3 hash on demand.
trait Basis: Sync {
    fn chunk(&self, i: usize) -> (u64, u32, u64);
    fn count(&self) -> usize;
    fn blake3(&self, i: usize) -> Blake3;
}

impl Basis for CdcSig {
    fn chunk(&self, i: usize) -> (u64, u32, u64) {
        let c = &self.chunks[i];
        (c.offset, c.length, c.xxh3_hash)
    }
    fn count(&self) -> usize {
        self.chunks.len()
    }
    fn blake3(&self, i: usize) -> Blake3 {
        self.chunks[i].blake3_hash
    }
}

/// A local basis: BLAKE3 is only computed for chunks that pass the xxh3 filter.
struct LocalBasis<'a> {
    data: &'a [u8],
    chunks: Vec<(u64, u32)>,
    xxh3: Vec<u64>,
}

impl Basis for LocalBasis<'_> {
    fn chunk(&self, i: usize) -> (u64, u32, u64) {
        (self.chunks[i].0, self.chunks[i].1, self.xxh3[i])
    }
    fn count(&self) -> usize {
        self.chunks.len()
    }
    fn blake3(&self, i: usize) -> Blake3 {
        blake3(slice(self.data, self.chunks[i]))
    }
}

/// Delta of `new` against a remote basis signature. BLAKE3 of a new chunk is computed
/// only when its xxh3 hash is in the basis.
pub fn diff(new: &[u8], basis: &CdcSig, p: &Params) -> Delta {
    match_chunks(new, &chunks(new, p), basis)
}

/// Delta between two local files.
pub fn local_delta(new: &[u8], basis: &[u8], p: &Params) -> Delta {
    let (nc, bc) = rayon::join(|| chunks(new, p), || chunks(basis, p));
    let xxh3 = bc.par_iter().with_min_len(HASH_BATCH).map(|&c| xxh3_64(slice(basis, c))).collect();
    match_chunks(new, &nc, &LocalBasis { data: basis, chunks: bc, xxh3 })
}

fn match_chunks(new: &[u8], new_chunks: &[(u64, u32)], basis: &dyn Basis) -> Delta {
    let mut by_hash: HashMap<u64, usize> = HashMap::with_capacity(basis.count());
    let mut at_off: HashMap<u64, usize> = HashMap::with_capacity(basis.count());
    for i in 0..basis.count() {
        let (off, _, x) = basis.chunk(i);
        by_hash.entry(x).or_insert(i);
        at_off.insert(off, i);
    }
    // Basis offset of each new chunk found in the basis. A chunk at the same offset in
    // both files is preferred, which keeps the delta in place where possible.
    let found: Vec<Option<u64>> = new_chunks
        .par_iter()
        .with_min_len(HASH_BATCH)
        .map(|&c| {
            let b = slice(new, c);
            let x = xxh3_64(b);
            let candidates = [at_off.get(&c.0).copied(), by_hash.get(&x).copied()];
            let mut strong = None;
            for i in candidates.into_iter().flatten() {
                let (off, len, bx) = basis.chunk(i);
                if bx == x && len == c.1 && *strong.get_or_insert_with(|| blake3(b)) == basis.blake3(i) {
                    return Some(off);
                }
            }
            None
        })
        .collect();
    let mut d = Delta::new(new.len() as u64);
    for (&(off, len), m) in new_chunks.iter().zip(found) {
        let len = len as u64;
        d.push(match m {
            Some(basis_off) => Op::Copy { off, basis_off, len },
            None => Op::Literal { off, len },
        });
    }
    d
}

/// A file mapped read-only, for hashing and for reading copy and literal ranges.
pub struct Mapped<'a> {
    map: Option<Mmap>,
    file: &'a File,
    hinted: bool,
}

impl<'a> Mapped<'a> {
    /// Map `file`, which must still be `size` bytes long. A file whose size changed
    /// gives an error of kind [`io::ErrorKind::Other`] tagged [`CHANGED`].
    pub fn new(file: &'a File, size: u64) -> io::Result<Mapped<'a>> {
        if size == 0 {
            return Ok(Mapped { map: None, file, hinted: false });
        }
        let map = unsafe { Mmap::map(file)? };
        if map.len() as u64 != size || file.metadata()?.len() != size {
            return Err(io::Error::other(CHANGED));
        }
        let hinted = cache::wanted(size);
        let _ = map.advise(if hinted { Advice::Sequential } else { Advice::Normal });
        Ok(Mapped { map: Some(map), file, hinted })
    }

    pub fn bytes(&self) -> &[u8] {
        self.map.as_deref().unwrap_or(&[])
    }
}

/// Message of the error [`Mapped::new`] returns for a file that changed size.
pub const CHANGED: &str = "file changed size while being synced";

pub fn is_changed(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Other && e.to_string() == CHANGED
}

impl ReadAt for Mapped<'_> {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        let off = usize::try_from(off).map_err(|_| io::ErrorKind::UnexpectedEof)?;
        let src = self.bytes().get(off..off + buf.len()).ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

impl Drop for Mapped<'_> {
    /// Big files: drop the pages from the page cache once unmapped (Linux; mapped
    /// pages on macOS stay cached like any other file data).
    fn drop(&mut self) {
        self.map = None;
        if self.hinted {
            cache::drop_read(self.file, 0, 0);
        }
    }
}
