//! Audit 2026-10-30 (A4): property tests of delta creation and reconstruction through the
//! public library API, with the production block and chunk sizes (`Params::DEFAULT`,
//! `Params::for_size`), not only the tiny sizes of `src/delta/tests.rs`.
//!
//! What this adds to the existing unit properties:
//! - `delta::local_delta` end to end (coarse pass, CDC fallback, merge, refinement at
//!   4 KiB) on sizes crossing the 4 KiB / 64 KiB / 128 KiB boundaries;
//! - the remote flow as `remote::sender`/`remote::receiver` run it (receiver's coarse
//!   signature, sender's diff, `refine::plan` with `REFINE_MIN`, the `Refine`/`Refined`
//!   round trip), with the signatures carried through `proto::encode`/`read_frame` in
//!   both wire formats;
//! - `--inplace` application (out *is* the basis) whenever the delta is in place;
//! - thread-count independence of `local_delta` with refinement across segments;
//! - multi-MiB files crossing the 4 MiB segment and apply-piece boundaries;
//! - robustness of the refinement helpers and `apply` against malformed peer input
//!   (no panic; errors instead).
//!
//! `PROPTEST_CASES` raises the number of cases (default here: 48 per property).

use barqsync::delta::apply::apply;
use barqsync::delta::refine::{self, GapSig, Grain};
use barqsync::delta::{self, ChunkEntry, Delta, Op, Params, cdc};
use barqsync::proto::wire::Wire;
use barqsync::proto::{self, Frame, Msg, NeedReq, Sig};
use proptest::prelude::*;
use std::fs::File;
use std::io::{Cursor, Write};
use std::os::unix::fs::FileExt;

// ---------------------------------------------------------------------------
// Data and edits
// ---------------------------------------------------------------------------

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Random, repetitive (many identical chunks), or mixed content.
fn content(len: usize, seed: u64, kind: u8) -> Vec<u8> {
    match kind % 3 {
        0 => noise(len, seed),
        1 => (0..len).map(|i| ((i / 7) % 13) as u8).collect(),
        _ => {
            let r = noise(len, seed);
            r.iter().enumerate().map(|(i, &b)| if (i / 5000) % 3 == 1 { 0 } else { b }).collect()
        }
    }
}

#[derive(Debug, Clone)]
enum Edit {
    Overwrite { pos: usize, len: usize, seed: u64 },
    Insert { pos: usize, len: usize, seed: u64 },
    Delete { pos: usize, len: usize },
    Append { len: usize, seed: u64 },
    Truncate { keep: usize },
    /// Copy a range of the file to another position (moved/duplicated data).
    Duplicate { from: usize, to: usize, len: usize },
}

fn edit_strategy() -> impl Strategy<Value = Edit> {
    // Lengths up to ~70 KiB so edits span fine (4 KiB) and coarse (64 KiB) blocks.
    let len = prop_oneof![1..64usize, 1..5000usize, 4000..70_000usize];
    prop_oneof![
        (any::<usize>(), len.clone(), any::<u64>()).prop_map(|(pos, len, seed)| Edit::Overwrite { pos, len, seed }),
        (any::<usize>(), len.clone(), any::<u64>()).prop_map(|(pos, len, seed)| Edit::Insert { pos, len, seed }),
        (any::<usize>(), len.clone()).prop_map(|(pos, len)| Edit::Delete { pos, len }),
        (len.clone(), any::<u64>()).prop_map(|(len, seed)| Edit::Append { len, seed }),
        any::<usize>().prop_map(|keep| Edit::Truncate { keep }),
        (any::<usize>(), any::<usize>(), len).prop_map(|(from, to, len)| Edit::Duplicate { from, to, len }),
    ]
}

fn apply_edit(data: &mut Vec<u8>, e: &Edit) {
    let n = data.len();
    match *e {
        Edit::Overwrite { pos, len, seed } if n > 0 => {
            let pos = pos % n;
            let len = len.min(n - pos);
            data[pos..pos + len].copy_from_slice(&noise(len, seed));
        }
        Edit::Insert { pos, len, seed } => {
            let pos = if n == 0 { 0 } else { pos % (n + 1) };
            data.splice(pos..pos, noise(len, seed));
        }
        Edit::Delete { pos, len } if n > 0 => {
            let pos = pos % n;
            data.drain(pos..(pos + len).min(n));
        }
        Edit::Append { len, seed } => data.extend(noise(len, seed)),
        Edit::Truncate { keep } => data.truncate(keep % (n + 1)),
        Edit::Duplicate { from, to, len } if n > 0 => {
            let from = from % n;
            let len = len.min(n - from);
            let piece = data[from..from + len].to_vec();
            let to = to % (n + 1);
            data.splice(to..to, piece);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// Ops tile `0..size`, every copy really matches the basis, and the size is the new file's.
fn check_delta(d: &Delta, new: &[u8], basis: &[u8]) -> Result<(), TestCaseError> {
    let mut pos = 0u64;
    for op in &d.ops {
        prop_assert_eq!(op.off(), pos, "ops must be contiguous: {:?}", d.ops);
        prop_assert!(!op.is_empty(), "empty op {:?}", op);
        if let Op::Copy { off, basis_off, len } = *op {
            let (o, b, l) = (off as usize, basis_off as usize, len as usize);
            prop_assert!(b + l <= basis.len(), "copy past the basis end: {:?} (basis {})", op, basis.len());
            prop_assert!(new[o..o + l] == basis[b..b + l], "copy op {:?} does not match the basis", op);
        }
        pos += op.len();
    }
    prop_assert_eq!(pos, d.size);
    prop_assert_eq!(d.size as usize, new.len());
    Ok(())
}

/// Rebuild into a fresh file (`out_has_basis = false`) or into a clone of the basis.
fn rebuild(d: &Delta, basis: &Vec<u8>, new: &Vec<u8>, out_has_basis: bool) -> Vec<u8> {
    let mut f = tempfile::tempfile().unwrap();
    if out_has_basis {
        f.write_all(basis).unwrap();
    }
    apply(d, basis, new, &f, out_has_basis).unwrap();
    read_all(&f, d.size)
}

/// `--inplace`: the output file *is* the basis (reads and writes hit the same file).
fn rebuild_inplace(d: &Delta, basis: &[u8], new: &Vec<u8>) -> Vec<u8> {
    let mut f = tempfile::tempfile().unwrap();
    f.write_all(basis).unwrap();
    apply(d, &f, new, &f, true).unwrap();
    read_all(&f, d.size)
}

fn read_all(f: &File, size: u64) -> Vec<u8> {
    assert_eq!(f.metadata().unwrap().len(), size, "output length");
    let mut out = vec![0u8; size as usize];
    FileExt::read_exact_at(f, &mut out, 0).unwrap();
    out
}

fn reconstructs(d: &Delta, basis: &Vec<u8>, new: &Vec<u8>) -> Result<(), TestCaseError> {
    check_delta(d, new, basis)?;
    prop_assert!(rebuild(d, basis, new, false) == *new, "rebuild into an empty file differs");
    prop_assert!(rebuild(d, basis, new, true) == *new, "patching a clone of the basis differs");
    if d.is_in_place() {
        prop_assert!(rebuild_inplace(d, basis, new) == *new, "--inplace application differs");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The remote flow (sender.rs / receiver.rs), messages carried through the wire codec
// ---------------------------------------------------------------------------

/// Encode a message and read it back, as the peer would.
fn through_wire(m: Msg, wire: Wire, level: Option<i32>) -> Msg {
    let bytes = proto::encode(&Frame::Msg(m), level, wire).unwrap();
    match proto::read_frame(&mut Cursor::new(bytes)).unwrap() {
        Some(Frame::Msg(m)) => m,
        other => panic!("unexpected frame {other:?}"),
    }
}

/// The delta a remote push/pull would apply for `new` against the receiver's `basis`,
/// or `None` where it would send the file whole.
fn remote_delta(new: &[u8], basis: &Vec<u8>, wire: Wire, level: Option<i32>, min_bytes: u64) -> Option<Delta> {
    let (n, b) = (new.len() as u64, basis.len() as u64);
    // Receiver (`basis_signature`): blocks when the size is unchanged, chunks otherwise.
    let p = Params::DEFAULT;
    let sig = if n == b {
        Sig::Blocks(delta::block_signature(basis, b, &p).unwrap())
    } else {
        Sig::Chunks(delta::chunk_signature(basis, b, &p).unwrap())
    };
    let need = Msg::Need { dir: 0, files: vec![NeedReq { idx: 0, sig: Some(sig) }], xattrs: Vec::new() };
    let Msg::Need { mut files, .. } = through_wire(need, wire, level) else { panic!("not a Need") };
    let sig = files.remove(0).sig.unwrap();
    let new_vec = new.to_vec();
    // Sender (`send_delta`).
    let (d, basis_size, unit) = match sig {
        Sig::Blocks(basis_sig) => {
            let bs = basis_sig.block_size as usize;
            let p = Params { block_size: bs, segment: (Params::DEFAULT.segment / bs).max(1) * bs, ..Params::DEFAULT };
            (delta::diff_blocks(&delta::block_signature(&new_vec, n, &p).unwrap(), &basis_sig), basis_sig.size, bs)
        }
        Sig::Chunks(basis_sig) => {
            let p = basis_sig.params(Params::DEFAULT).expect("receiver chunk sizes");
            (delta::diff_chunks(&delta::chunk_signature(&new_vec, n, &p).unwrap(), &basis_sig), basis_sig.size, p.cdc_avg)
        }
        _ => unreachable!(),
    };
    // Sender's `refine`, with the receiver's answer through the wire.
    let fine = Params::for_size(basis_size);
    let d = match refine::plan(&d, basis_size, unit, &fine, min_bytes) {
        None => d,
        Some(gaps) => {
            let req = Msg::Refine { id: 7, grain: Grain::of(&fine), gaps: gaps.iter().map(refine::Gap::request).collect() };
            let Msg::Refine { grain, gaps: reqs, .. } = through_wire(req, wire, level) else { panic!("not a Refine") };
            let rp = grain.params().expect("receiver accepts the sender's grain");
            let theirs = refine::basis_sigs(basis, b, &rp, &reqs).unwrap();
            let Msg::Refined { gaps: theirs, .. } = through_wire(Msg::Refined { id: 7, gaps: theirs }, wire, level) else {
                panic!("not Refined")
            };
            let ours = refine::new_side(&new_vec, n, basis_size, &fine, &gaps).unwrap();
            refine::combine(&d, basis_size, &fine, &gaps, &ours, &theirs)
        }
    };
    // `send_ops`: nothing matched → the file goes whole.
    (d.matched_bytes() > 0).then_some(d)
}

fn wire_of(i: u8) -> (Wire, Option<i32>) {
    match i % 4 {
        0 => (Wire::Postcard, None),
        1 => (Wire::Postcard, Some(3)),
        2 => (Wire::Rkyv, None),
        _ => (Wire::Rkyv, Some(1)),
    }
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(48)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]

    /// `delta::local_delta` with the production sizes reproduces the new file.
    #[test]
    fn local_delta_with_production_sizes_reproduces_the_new_file(
        len in prop_oneof![0..9000usize, 60_000..140_000usize, 0..420_000usize],
        seed in any::<u64>(),
        kind in any::<u8>(),
        edits in prop::collection::vec(edit_strategy(), 0..6),
    ) {
        let basis = content(len, seed, kind);
        let mut new = basis.clone();
        for e in &edits {
            apply_edit(&mut new, e);
        }
        let (n, b) = (new.len() as u64, basis.len() as u64);
        let d = delta::local_delta(&new, n, &basis, b, &Params::DEFAULT).unwrap();
        reconstructs(&d, &basis, &new)?;
        // An unchanged file is matched completely.
        if new == basis && n > 0 {
            prop_assert_eq!(d.literal_bytes(), 0);
        }
    }

    /// The remote push/pull flow (coarse signature, diff, refinement round trip through
    /// the wire codec) reproduces the new file whenever it yields a delta.
    #[test]
    fn remote_delta_flow_reproduces_the_new_file(
        len in prop_oneof![60_000..140_000usize, 0..420_000usize],
        seed in any::<u64>(),
        kind in any::<u8>(),
        edits in prop::collection::vec(edit_strategy(), 0..6),
        wire in any::<u8>(),
        eager in any::<bool>(),
    ) {
        let basis = content(len, seed, kind);
        let mut new = basis.clone();
        for e in &edits {
            apply_edit(&mut new, e);
        }
        let (w, level) = wire_of(wire);
        // `eager`: refine any gap (local threshold) instead of REFINE_MIN.
        let min_bytes = if eager { 0 } else { refine::REFINE_MIN };
        if let Some(d) = remote_delta(&new, &basis, w, level, min_bytes) {
            reconstructs(&d, &basis, &new)?;
        }
    }

    /// `--delta=cdc` with the production sizes: remote and local deltas agree and rebuild.
    #[test]
    fn cdc_mode_with_production_sizes_reproduces_the_new_file(
        len in prop_oneof![0..9000usize, 0..300_000usize],
        seed in any::<u64>(),
        kind in any::<u8>(),
        edits in prop::collection::vec(edit_strategy(), 0..5),
    ) {
        let basis = content(len, seed, kind);
        let mut new = basis.clone();
        for e in &edits {
            apply_edit(&mut new, e);
        }
        let p = Params::DEFAULT;
        let local = cdc::local_delta(&new, &basis, &p);
        let remote = cdc::diff(&new, &cdc::signature(&basis, &p), &p);
        prop_assert_eq!(&local, &remote);
        reconstructs(&local, &basis, &new)?;
    }

    /// The delta does not depend on the number of threads, also with refinement across
    /// several segments (small coarse sizes so a few hundred KiB span many segments).
    #[test]
    fn local_delta_is_the_same_with_any_thread_count(
        len in 0..600_000usize,
        seed in any::<u64>(),
        kind in any::<u8>(),
        edits in prop::collection::vec(edit_strategy(), 0..5),
    ) {
        let p = Params { block_size: 16 << 10, segment: 64 << 10, cdc_min: 8 << 10, cdc_avg: 16 << 10, cdc_max: 32 << 10, cdc_region: 64 << 10 };
        let basis = content(len, seed, kind);
        let mut new = basis.clone();
        for e in &edits {
            apply_edit(&mut new, e);
        }
        let (n, b) = (new.len() as u64, basis.len() as u64);
        let run = |threads: usize| {
            rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap().install(|| delta::local_delta(&new, n, &basis, b, &p).unwrap())
        };
        let one = run(1);
        reconstructs(&one, &basis, &new)?;
        for threads in [2, 7] {
            prop_assert_eq!(&run(threads), &one, "threads {}", threads);
        }
    }

    /// Malformed refinement answers (they come from the peer) never panic, and the
    /// combined delta still tiles the file.
    #[test]
    fn malformed_refinement_answers_do_not_panic(
        len in 70_000..200_000usize,
        seed in any::<u64>(),
        edits in prop::collection::vec(edit_strategy(), 1..4),
        junk in prop::collection::vec((any::<u32>(), any::<[u8; 16]>(), 0..40usize, any::<bool>()), 0..6),
    ) {
        let basis = noise(len, seed);
        let mut new = basis.clone();
        for e in &edits {
            apply_edit(&mut new, e);
        }
        let (n, b) = (new.len() as u64, basis.len() as u64);
        let p = Params::DEFAULT;
        let coarse = delta::diff_chunks(&delta::chunk_signature(&new, n, &p).unwrap(), &delta::chunk_signature(&basis, b, &p).unwrap());
        let fine = Params::for_size(b);
        let gaps = refine::gaps(&coarse, b);
        let ours = refine::new_side(&new, n, b, &fine, &gaps).unwrap();
        // One answer per gap (so `combine` uses them), each made of junk.
        let theirs: Vec<GapSig> = gaps
            .iter()
            .enumerate()
            .map(|(i, g)| {
                let (l, h, nb, exact) = junk.get(i % junk.len().max(1)).copied().unwrap_or((0, [0; 16], 0, false));
                let mut chunks = vec![ChunkEntry { len: l % 70_000, hash: h }; nb % 5];
                if exact && g.basis_len > 0 && g.basis_len <= u32::MAX as u64 {
                    // Lengths that add up to the stretch, so the chunk branch is taken.
                    chunks = vec![ChunkEntry { len: g.basis_len as u32, hash: h }];
                }
                GapSig { blocks: vec![h; nb], chunks }
            })
            .collect();
        let d = refine::combine(&coarse, b, &fine, &gaps, &ours, &theirs);
        let mut pos = 0;
        for op in &d.ops {
            prop_assert_eq!(op.off(), pos);
            pos += op.len();
        }
        prop_assert_eq!(pos, n);
    }

    /// Out-of-range refinement requests (from the peer) never panic.
    #[test]
    fn out_of_range_refinement_requests_do_not_panic(
        len in 0..100_000usize,
        reqs in prop::collection::vec((any::<u64>(), any::<u64>(), any::<bool>()), 0..8),
        block in prop::sample::select(vec![512u32, 4096, 65536]),
    ) {
        let basis = noise(len, 3);
        let grain = Grain { block, min: block / 2, avg: block, max: block * 2 };
        let p = grain.params().unwrap();
        let reqs: Vec<refine::GapReq> = reqs.into_iter().map(|(off, l, chunks)| refine::GapReq { off, len: l, chunks }).collect();
        let sigs = refine::basis_sigs(&basis, len as u64, &p, &reqs).unwrap();
        prop_assert!(sigs.is_empty() || sigs.len() == reqs.len());
    }

    /// Chunk and grain sizes from the peer are either refused or usable.
    #[test]
    fn peer_chunk_sizes_are_refused_or_usable(min in any::<u32>(), avg in any::<u32>(), max in any::<u32>(), block in any::<u32>()) {
        let data = noise(20_000, 5);
        let sig = delta::ChunkSig { size: 0, min, avg, max, chunks: Vec::new() };
        if let Some(p) = sig.params(Params::DEFAULT) {
            let s = delta::chunk_signature(&data, data.len() as u64, &p).unwrap();
            prop_assert_eq!(s.iter().map(|(_, l, _)| l as u64).sum::<u64>(), 20_000);
        }
        if let Some(p) = (Grain { block, min, avg, max }).params() {
            prop_assert!(delta::block_signature(&data, data.len() as u64, &p).is_ok());
        }
    }
}

// ---------------------------------------------------------------------------
// Deterministic edge cases
// ---------------------------------------------------------------------------

/// Empty and boundary sizes, through every path.
#[test]
fn empty_and_boundary_sizes_reproduce_the_new_file() {
    let sizes = [0usize, 1, 4095, 4096, 4097, 65_535, 65_536, 65_537, 131_072, 131_073, 262_144];
    for &bl in &sizes {
        for &nl in &sizes {
            let basis = noise(bl, 11);
            // The new file shares the basis's prefix where it can.
            let mut new = noise(nl, 11);
            if nl > 10 {
                new[nl / 2] ^= 0xff;
            }
            let (n, b) = (nl as u64, bl as u64);
            let d = delta::local_delta(&new, n, &basis, b, &Params::DEFAULT).unwrap();
            reconstructs(&d, &basis, &new).unwrap_or_else(|e| panic!("local {bl}->{nl}: {e}"));
            for w in 0..4 {
                let (wire, level) = wire_of(w);
                if let Some(d) = remote_delta(&new, &basis, wire, level, 0) {
                    reconstructs(&d, &basis, &new).unwrap_or_else(|e| panic!("remote {bl}->{nl} {wire:?}: {e}"));
                }
            }
            let c = cdc::local_delta(&new, &basis, &Params::DEFAULT);
            reconstructs(&c, &basis, &new).unwrap_or_else(|e| panic!("cdc {bl}->{nl}: {e}"));
        }
    }
}

/// Files of several MiB cross the 4 MiB segment and the 4 MiB apply-piece boundaries:
/// inserts, in-place edits, a delete and an append around them.
#[test]
fn multi_mib_files_across_segment_and_piece_boundaries() {
    let basis = noise((9 << 20) + 12_345, 99);
    let mut new = basis.clone();
    new[(4 << 20) - 10..(4 << 20) + 10].fill(0xaa); // straddles the segment edge
    new.splice((3 << 20)..(3 << 20), noise(1000, 1)); // shift everything after 3 MiB
    new.drain((6 << 20)..(6 << 20) + 70_000);
    new[(8 << 20)..(8 << 20) + 100_000].copy_from_slice(&noise(100_000, 2));
    new.extend(noise(300_000, 3));
    let (n, b) = (new.len() as u64, basis.len() as u64);

    let d = delta::local_delta(&new, n, &basis, b, &Params::DEFAULT).unwrap();
    reconstructs(&d, &basis, &new).unwrap();
    assert!(d.literal_bytes() < 1 << 20, "too little matched: {} literal bytes", d.literal_bytes());

    let r = remote_delta(&new, &basis, Wire::Postcard, Some(3), refine::REFINE_MIN).expect("a delta");
    reconstructs(&r, &basis, &new).unwrap();
    assert!(r.literal_bytes() < 1 << 20, "remote: too little matched: {} literal bytes", r.literal_bytes());

    // Same size, in-place edits only: blocks, and an in-place delta (`--inplace`).
    let mut same = basis.clone();
    for at in [0usize, (4 << 20) - 1, 5 << 20, (9 << 20) + 12_000] {
        same[at] ^= 0x55;
    }
    let r = remote_delta(&same, &basis, Wire::Rkyv, None, refine::REFINE_MIN).expect("a delta");
    assert!(r.is_in_place());
    reconstructs(&r, &basis, &same).unwrap();
    assert!(r.literal_bytes() <= 4 * 4096, "in-place edits refined to fine blocks: {}", r.literal_bytes());
}

/// `apply` given copies past the end of the basis (a malformed delta) fails cleanly.
#[test]
fn apply_rejects_copies_past_the_basis() {
    let basis = noise(10_000, 1);
    let new = noise(20_000, 2);
    let mut d = Delta::new(20_000);
    d.push(Op::Copy { off: 0, basis_off: 5_000, len: 10_000 });
    d.push(Op::Literal { off: 10_000, len: 10_000 });
    for has_basis in [false, true] {
        let f = tempfile::tempfile().unwrap();
        let r = apply(&d, &basis, &new, &f, has_basis);
        assert!(r.is_err(), "out_has_basis={has_basis}: a copy past the basis was applied");
    }
    // Basis as a real file: the same.
    let mut bf = tempfile::tempfile().unwrap();
    bf.write_all(&basis).unwrap();
    let f = tempfile::tempfile().unwrap();
    assert!(apply(&d, &bf, &new, &f, false).is_err());
}

/// `diff_blocks` with a basis signature whose hash list disagrees with its size (a
/// malformed peer signature) must not panic or copy beyond the basis.
#[test]
fn inconsistent_block_signatures_do_not_panic() {
    let p = Params::DEFAULT;
    let new = noise(300_000, 4);
    let ns = delta::block_signature(&new, 300_000, &p).unwrap();
    for (size, hashes) in [(0u64, ns.hashes.clone()), (300_000, Vec::new()), (1, ns.hashes.clone()), (10_000_000, ns.hashes.clone())] {
        let bs = delta::BlockSig { block_size: ns.block_size, size, hashes };
        let d = delta::diff_blocks(&ns, &bs);
        let mut pos = 0;
        for op in &d.ops {
            assert_eq!(op.off(), pos);
            if let Op::Copy { basis_off, len, .. } = *op {
                assert!(basis_off + len <= size, "copy past the claimed basis size {size}: {op:?}");
            }
            pos += op.len();
        }
        assert_eq!(pos, 300_000);
    }
}

/// `cdc::Mapped` (the `--delta=cdc` reader) reads exactly the file through `ReadAt`.
#[test]
fn mapped_files_read_like_the_bytes() {
    let data = noise(200_000, 8);
    let mut f = tempfile::tempfile().unwrap();
    f.write_all(&data).unwrap();
    let m = cdc::Mapped::new(&f, data.len() as u64).unwrap();
    assert_eq!(m.bytes(), &data[..]);
    let mut buf = vec![0u8; 1000];
    delta::ReadAt::read_exact_at(&m, &mut buf, 199_000).unwrap();
    assert_eq!(&buf[..], &data[199_000..]);
    assert!(delta::ReadAt::read_exact_at(&m, &mut buf, 199_500).is_err(), "read past the end");
    let empty = tempfile::tempfile().unwrap();
    assert!(cdc::Mapped::new(&empty, 0).unwrap().bytes().is_empty());
}
