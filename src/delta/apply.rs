//! Produce the new file from a [`Delta`]: patch a clone of the basis, or rebuild it.

use super::{Delta, Op, ReadAt, with_range};
use crate::fsops::{cache, check_interrupt};
use rayon::prelude::*;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

/// Largest unit of work: ops are split into pieces of at most this size.
const PIECE: u64 = 4 << 20;

/// Write the new file into `out`.
///
/// - `out_has_basis = false`: `out` is empty; every range is written (copies from
///   `basis`, literals from `literal`).
/// - `out_has_basis = true`: `out` already holds the basis content (a clone of it, or
///   the destination itself for `--inplace`), so in-place copies are skipped and only
///   moved copies and literals are written. Reads always come from `basis`, never from
///   `out`; when `out` *is* the basis (`--inplace`) the delta must be in place.
pub fn apply(delta: &Delta, basis: &dyn ReadAt, literal: &dyn ReadAt, out: &File, out_has_basis: bool) -> io::Result<()> {
    let pieces: Vec<Op> = delta
        .ops
        .iter()
        .filter(|op| !(out_has_basis && matches!(op, Op::Copy { off, basis_off, .. } if off == basis_off)))
        .flat_map(|op| split(*op))
        .collect();
    if out_has_basis {
        // Patching a clone: only the changed ranges are written, so scattered
        // parallel writes are cheap.
        pieces.par_iter().try_for_each(|op| {
            check_interrupt()?;
            let (src, src_off) = source(op, basis, literal);
            with_range(src, src_off, op.len() as usize, |buf| out.write_all_at(buf, op.off()))?
        })?;
    } else {
        rebuild_in_order(&pieces, basis, literal, out, delta.size)?;
    }
    out.set_len(delta.size)
}

/// Pieces per batch when rebuilding a whole file (32 MiB).
const BATCH: usize = 8;

/// Write every piece of a new file in ascending order while reading the next batch
/// in parallel. Sequential writes keep the file contiguous: scattered parallel
/// writes into a sparse file degrade badly once the disk's write cache fills.
fn rebuild_in_order(pieces: &[Op], basis: &dyn ReadAt, literal: &dyn ReadAt, out: &File, size: u64) -> io::Result<()> {
    let mut hint = cache::Writer::new(out, size);
    let read_batch = |batch: &[Op]| -> io::Result<Vec<Vec<u8>>> {
        batch
            .par_iter()
            .map(|op| {
                check_interrupt()?;
                let (src, src_off) = source(op, basis, literal);
                let mut buf = vec![0u8; op.len() as usize];
                src.read_exact_at(&mut buf, src_off)?;
                Ok(buf)
            })
            .collect()
    };
    let write_batch = |batch: &[Op], bufs: Vec<Vec<u8>>, hint: &mut cache::Writer| -> io::Result<()> {
        for (op, buf) in batch.iter().zip(bufs) {
            out.write_all_at(&buf, op.off())?;
            hint.written_to(out, op.off() + op.len());
        }
        Ok(())
    };
    let mut batches = pieces.chunks(BATCH);
    let mut current = match batches.next() {
        Some(b) => Some((b, read_batch(b)?)),
        None => None,
    };
    while let Some((batch, bufs)) = current {
        let next = batches.next();
        let (written, read) = rayon::join(
            || write_batch(batch, bufs, &mut hint),
            || next.map(|n| read_batch(n).map(|bufs| (n, bufs))).transpose(),
        );
        written?;
        current = read?;
    }
    Ok(())
}

/// Where an op's bytes come from: the basis for copies, the new file for literals.
fn source<'a>(op: &Op, basis: &'a dyn ReadAt, literal: &'a dyn ReadAt) -> (&'a dyn ReadAt, u64) {
    match *op {
        Op::Copy { basis_off, .. } => (basis, basis_off),
        Op::Literal { off, .. } => (literal, off),
    }
}

fn split(op: Op) -> impl Iterator<Item = Op> {
    let n = op.len().div_ceil(PIECE);
    (0..n).map(move |i| {
        let d = i * PIECE;
        let len = PIECE.min(op.len() - d);
        match op {
            Op::Copy { off, basis_off, .. } => Op::Copy { off: off + d, basis_off: basis_off + d, len },
            Op::Literal { off, .. } => Op::Literal { off: off + d, len },
        }
    })
}
