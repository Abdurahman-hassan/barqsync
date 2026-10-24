//! Refining a coarse delta.
//!
//! The first signature is coarse (64 KiB blocks, or 32 KiB chunks when the file changed
//! size), so it is cheap to hash and send whatever the file. What the coarse delta leaves
//! literal is then refined: for each literal run (a [`Gap`]) the sender knows where the same
//! stretch sits in the basis, between the same matched neighbours, and asks for fine
//! signatures of just those basis stretches ([`Params::for_size`]: 4 KiB up to 256 MiB). A
//! file that was only appended to has no basis stretch worth refining and costs nothing
//! extra; a database with scattered edits pays for fine signatures of its changed areas only.
//!
//! Within a gap two kinds of fine match are combined, as in [`merge_in_place`]:
//! - aligned blocks on the basis's block grid, compared at the gap's offset shift (data that
//!   stayed in place, such as database pages, or moved as a whole);
//! - content-defined chunks, when the two stretches differ in length (data was inserted or
//!   removed inside the gap).

use super::*;

/// Less basis data than this between matched ranges is not worth a network round trip
/// (locally, where there is none, any gap holding a fine block is refined).
pub const REFINE_MIN: u64 = 64 << 10;

/// Fine block and chunk sizes, sent with a refinement request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Grain {
    pub block: u32,
    pub min: u32,
    pub avg: u32,
    pub max: u32,
}

impl Grain {
    pub fn of(p: &Params) -> Grain {
        Grain { block: p.block_size as u32, min: p.cdc_min as u32, avg: p.cdc_avg as u32, max: p.cdc_max as u32 }
    }

    /// The parameters for this grain, or `None` if they make no sense (they come from the
    /// other side).
    pub fn params(&self) -> Option<Params> {
        let block = self.block as usize;
        let ok = (512..=Params::DEFAULT.block_size).contains(&block) && Params::DEFAULT.segment.is_multiple_of(block);
        ok.then_some(Params { block_size: block, ..Params::DEFAULT }).and_then(|p| with_cdc(p, self.min, self.avg, self.max))
    }
}

/// A literal run of a coarse delta (`off`, `len` in the new file) and the stretch of the
/// basis between the same matched neighbours (`basis_off`, `basis_len`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    pub off: u64,
    pub len: u64,
    pub basis_off: u64,
    pub basis_len: u64,
}

impl Gap {
    /// The two stretches differ in length: something was inserted or removed in between,
    /// so content-defined chunks are needed besides aligned blocks.
    fn shifted(&self) -> bool {
        self.len != self.basis_len
    }

    pub fn request(&self) -> GapReq {
        GapReq { off: self.basis_off, len: self.basis_len, chunks: self.shifted() }
    }
}

/// A basis stretch to sign finely, sent in a refinement request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct GapReq {
    pub off: u64,
    pub len: u64,
    pub chunks: bool,
}

/// Fine signature of one basis stretch: the hashes of the basis's grid blocks covering it,
/// and its content-defined chunks if they were asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct GapSig {
    pub blocks: Vec<Hash128>,
    pub chunks: Vec<ChunkEntry>,
}

/// The literal runs of `d`, each with the basis stretch between its neighbouring copies.
pub fn gaps(d: &Delta, basis_size: u64) -> Vec<Gap> {
    let mut out = Vec::new();
    for (i, op) in d.ops.iter().enumerate() {
        let Op::Literal { off, len } = *op else { continue };
        // Literals never follow literals (Delta::push merges them): the neighbours are copies.
        let lo = match i.checked_sub(1).map(|j| d.ops[j]) {
            Some(Op::Copy { basis_off, len, .. }) => (basis_off + len).min(basis_size),
            _ => 0,
        };
        let hi = match d.ops.get(i + 1) {
            Some(&Op::Copy { basis_off, .. }) => basis_off.min(basis_size),
            _ => basis_size,
        };
        out.push(Gap { off, len, basis_off: lo, basis_len: hi.saturating_sub(lo) });
    }
    out
}

/// The gaps of `d` worth refining with `fine`: those holding at least one fine block of
/// basis data, if `fine` is finer than `coarse_unit` and the gaps hold `min_bytes` or more
/// basis bytes in all ([`REFINE_MIN`] over the network). A file that matched nothing at
/// all (rewritten, compressed, encrypted) is not refined: it is sent whole, as before.
pub fn plan(d: &Delta, basis_size: u64, coarse_unit: usize, fine: &Params, min_bytes: u64) -> Option<Vec<Gap>> {
    if fine.block_size >= coarse_unit || d.matched_bytes() == 0 {
        return None;
    }
    let gaps: Vec<Gap> = gaps(d, basis_size).into_iter().filter(|g| g.basis_len >= fine.block_size as u64).collect();
    let total: u64 = gaps.iter().map(|g| g.basis_len).sum();
    (total > 0 && total >= min_bytes).then_some(gaps)
}

/// Start and end of the basis block grid covering `off..off + len`.
fn grid(off: u64, len: u64, unit: u64, basis_size: u64) -> (u64, u64) {
    let start = off / unit * unit;
    let end = off.saturating_add(len).div_ceil(unit).saturating_mul(unit).min(basis_size);
    (start, end.max(start))
}

/// `src` seen from `base` on.
struct Window<'a> {
    src: &'a dyn ReadAt,
    base: u64,
}

impl ReadAt for Window<'_> {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        self.src.read_exact_at(buf, self.base + off)
    }
}

/// The most basis data a refinement is allowed to ask this side to sign, over and above
/// its own size: a genuine refinement signs the basis stretches between matched ranges,
/// so it never needs more than the basis a few times over. A peer that asks for far more
/// (millions of gaps each spanning the whole basis) gets no refinement rather than this
/// side signing unboundedly; the ranges then stay literal, which is correct, just larger.
const REFINE_BUDGET: u64 = 4;

/// The receiver's side: fine signatures of the basis stretches asked for. An implausibly
/// large request (see [`REFINE_BUDGET`]) is declined with an empty answer.
pub fn basis_sigs(basis: &dyn ReadAt, basis_size: u64, p: &Params, reqs: &[GapReq]) -> io::Result<Vec<GapSig>> {
    let budget = basis_size.saturating_mul(REFINE_BUDGET).max(Params::DEFAULT.segment as u64);
    let mut asked = 0u64;
    for r in reqs {
        asked = asked.saturating_add(r.len.min(basis_size));
    }
    if asked > budget {
        return Ok(Vec::new());
    }
    reqs.par_iter()
        .map(|r| {
            let off = r.off.min(basis_size);
            let len = r.len.min(basis_size - off);
            let (start, end) = grid(off, len, p.block_size as u64, basis_size);
            let blocks = block_signature(&Window { src: basis, base: start }, end - start, p)?.hashes;
            let chunks = if r.chunks { chunk_signature(&Window { src: basis, base: off }, len, p)?.chunks } else { Vec::new() };
            Ok(GapSig { blocks, chunks })
        })
        .collect()
}

/// The sender's side of one gap: the hashes of its own data at the positions of the basis
/// grid blocks shifted by the gap's offset difference (`None` where that falls outside the
/// file), and its content-defined chunks when the gap's two stretches differ in length.
pub struct NewSide {
    blocks: Vec<Option<Hash128>>,
    chunks: Option<ChunkSig>,
}

/// Compute the sender's side of `gaps`, which needs nothing from the receiver, so it can
/// run while the receiver computes its side.
pub fn new_side(new: &dyn ReadAt, new_size: u64, basis_size: u64, p: &Params, gaps: &[Gap]) -> io::Result<Vec<NewSide>> {
    let unit = p.block_size as u64;
    gaps.par_iter()
        .map(|g| {
            let (start, end) = grid(g.basis_off, g.basis_len, unit, basis_size);
            let n = (end - start).div_ceil(unit) as usize;
            let shift = g.off as i128 - g.basis_off as i128;
            // The shifted positions inside the new file form one run of blocks.
            let fits = |i: usize| {
                let b = start + i as u64 * unit;
                let x = b as i128 + shift;
                x >= 0 && x + unit.min(end - b) as i128 <= new_size as i128
            };
            let first = (0..n).find(|&i| fits(i)).unwrap_or(n);
            let last = (first..n).rev().find(|&i| fits(i)).map_or(first, |i| i + 1);
            let mut blocks = vec![None; n];
            if first < last {
                let (wb, we) = (start + first as u64 * unit, (start + last as u64 * unit).min(end));
                let hashes = block_signature(&Window { src: new, base: (wb as i128 + shift) as u64 }, we - wb, p)?.hashes;
                for (slot, h) in blocks[first..last].iter_mut().zip(hashes) {
                    *slot = Some(h);
                }
            }
            let chunks = if g.shifted() { Some(chunk_signature(&Window { src: new, base: g.off }, g.len, p)?) } else { None };
            Ok(NewSide { blocks, chunks })
        })
        .collect()
}

/// Replace each gap's literal run in `d` with the copies its fine signatures find. `theirs`
/// is the receiver's answer; a missing or malformed answer keeps a run literal.
pub fn combine(d: &Delta, basis_size: u64, p: &Params, gaps: &[Gap], ours: &[NewSide], theirs: &[GapSig]) -> Delta {
    if ours.len() != gaps.len() || theirs.len() != gaps.len() {
        return d.clone();
    }
    let unit = p.block_size as u64;
    let mut out = Delta::new(d.size);
    let mut k = 0;
    for op in &d.ops {
        match *op {
            Op::Literal { off, len } if k < gaps.len() && gaps[k].off == off => {
                let (g, new, basis) = (&gaps[k], &ours[k], &theirs[k]);
                k += 1;
                // Content matches inside the gap.
                let mut run = Delta::new(d.size);
                let basis_chunks: u64 = basis.chunks.iter().map(|c| c.len as u64).sum();
                match &new.chunks {
                    Some(nc) if !basis.chunks.is_empty() && basis_chunks == g.basis_len => {
                        let mut by_hash: HashMap<Hash128, (u64, u32)> = HashMap::with_capacity(basis.chunks.len());
                        let mut pos = g.basis_off;
                        for c in &basis.chunks {
                            by_hash.entry(c.hash).or_insert((pos, c.len));
                            pos += c.len as u64;
                        }
                        for (rel, clen, hash) in nc.iter() {
                            let (o, l) = (g.off + rel, clen as u64);
                            match by_hash.get(hash) {
                                Some(&(basis_off, blen)) if blen == clen => run.push(Op::Copy { off: o, basis_off, len: l }),
                                _ => run.push(Op::Literal { off: o, len: l }),
                            }
                        }
                    }
                    _ => run.push(Op::Literal { off, len }),
                }
                // Aligned matches at the gap's shift.
                let (start, end) = grid(g.basis_off, g.basis_len, unit, basis_size);
                let shift = g.off as i128 - g.basis_off as i128;
                let copies: Vec<(u64, u64, u64)> = if basis.blocks.len() == new.blocks.len() {
                    let same = new.blocks.iter().zip(&basis.blocks).enumerate().filter(|(_, (n, b))| n.as_ref() == Some(*b));
                    same.map(|(i, _)| {
                        let b = start + i as u64 * unit;
                        ((b as i128 + shift) as u64, b, unit.min(end - b))
                    })
                    .collect()
                } else {
                    Vec::new()
                };
                overlay(&mut out, &run.ops, &copies);
            }
            _ => out.push(*op),
        }
    }
    out
}

/// Both files local: refine `gaps` of `d` directly.
pub fn local(d: &Delta, new: &dyn ReadAt, new_size: u64, basis: &dyn ReadAt, basis_size: u64, p: &Params, gaps: &[Gap]) -> io::Result<Delta> {
    let reqs: Vec<GapReq> = gaps.iter().map(Gap::request).collect();
    let (ours, theirs) = rayon::join(|| new_side(new, new_size, basis_size, p, gaps), || basis_sigs(basis, basis_size, p, &reqs));
    Ok(combine(d, basis_size, p, gaps, &ours?, &theirs?))
}
