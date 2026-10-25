use super::apply::apply;
use super::*;
use proptest::prelude::*;
use std::io::Write;

/// Tiny sizes so property tests exercise many blocks, segments and chunks quickly.
const P: Params = Params { block_size: 256, segment: 4096, cdc_min: 64, cdc_avg: 256, cdc_max: 1024, cdc_region: 4096 };
/// A coarse first pass and the fine sizes that refine it, small enough for the same tests.
const COARSE: Params = Params { block_size: 1024, segment: 8192, cdc_min: 256, cdc_avg: 1024, cdc_max: 4096, cdc_region: 8192 };
const FINE: Params = Params { block_size: 128, segment: 8192, cdc_min: 64, cdc_avg: 256, cdc_max: 1024, cdc_region: 8192 };

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
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

#[derive(Debug, Clone)]
enum Edit {
    Overwrite { pos: usize, len: usize, seed: u64 },
    Insert { pos: usize, len: usize, seed: u64 },
    Delete { pos: usize, len: usize },
    Append { len: usize, seed: u64 },
    Truncate { keep: usize },
}

fn edit_strategy() -> impl Strategy<Value = Edit> {
    prop_oneof![
        (any::<usize>(), 1..3000usize, any::<u64>()).prop_map(|(pos, len, seed)| Edit::Overwrite { pos, len, seed }),
        (any::<usize>(), 1..3000usize, any::<u64>()).prop_map(|(pos, len, seed)| Edit::Insert { pos, len, seed }),
        (any::<usize>(), 1..3000usize).prop_map(|(pos, len)| Edit::Delete { pos, len }),
        (1..5000usize, any::<u64>()).prop_map(|(len, seed)| Edit::Append { len, seed }),
        any::<usize>().prop_map(|keep| Edit::Truncate { keep }),
    ]
}

fn apply_edit(data: &mut Vec<u8>, e: &Edit) {
    let n = data.len();
    match *e {
        Edit::Overwrite { pos, len, seed } if n > 0 => {
            let pos = pos % n;
            let len = len.min(n - pos);
            data[pos..pos + len].copy_from_slice(&pseudo_random(len, seed));
        }
        Edit::Insert { pos, len, seed } => {
            let pos = if n == 0 { 0 } else { pos % (n + 1) };
            data.splice(pos..pos, pseudo_random(len, seed));
        }
        Edit::Delete { pos, len } if n > 0 => {
            let pos = pos % n;
            data.drain(pos..(pos + len).min(n));
        }
        Edit::Append { len, seed } => data.extend(pseudo_random(len, seed)),
        Edit::Truncate { keep } => data.truncate(keep % (n + 1)),
        _ => {}
    }
}

/// Ops must tile `0..size` exactly, and every Copy must really match the basis.
fn check_delta(d: &Delta, new: &[u8], basis: &[u8]) {
    let mut pos = 0u64;
    for op in &d.ops {
        assert_eq!(op.off(), pos, "ops must be contiguous: {:?}", d.ops);
        if let Op::Copy { off, basis_off, len } = *op {
            let (o, b, l) = (off as usize, basis_off as usize, len as usize);
            assert_eq!(&new[o..o + l], &basis[b..b + l], "copy op doesn't match basis");
        }
        pos += op.len();
    }
    assert_eq!(pos, d.size);
    assert_eq!(d.size as usize, new.len());
}

fn rebuild(d: &Delta, basis: &Vec<u8>, new: &Vec<u8>, out_has_basis: bool) -> Vec<u8> {
    let mut f = tempfile::tempfile().unwrap();
    if out_has_basis {
        f.write_all(basis).unwrap();
    }
    apply(d, basis, new, &f, out_has_basis).unwrap();
    let mut out = vec![0u8; d.size as usize];
    FileExt::read_exact_at(&f, &mut out, 0).unwrap();
    assert_eq!(f.metadata().unwrap().len(), d.size);
    out
}

proptest! {
    // 256 cases by default; set PROPTEST_CASES for longer runs.

    #[test]
    fn deltas_always_reproduce_the_new_file(
        len in 0..40_000usize,
        seed in any::<u64>(),
        edits in prop::collection::vec(edit_strategy(), 0..6),
    ) {
        let basis = pseudo_random(len, seed);
        let mut new = basis.clone();
        for e in &edits {
            apply_edit(&mut new, e);
        }
        let (bn, bb) = (block_signature(&new, new.len() as u64, &P).unwrap(), block_signature(&basis, basis.len() as u64, &P).unwrap());
        let (cn, cb) = (chunk_signature(&new, new.len() as u64, &P).unwrap(), chunk_signature(&basis, basis.len() as u64, &P).unwrap());
        let (blocks, chunks) = (diff_blocks(&bn, &bb), diff_chunks(&cn, &cb));
        let merged = merge_in_place(&chunks, &blocks);
        prop_assert!(merged.literal_bytes() <= blocks.literal_bytes().min(chunks.literal_bytes()));
        for d in [blocks, chunks, merged] {
            check_delta(&d, &new, &basis);
            prop_assert_eq!(&rebuild(&d, &basis, &new, false), &new);
            prop_assert_eq!(&rebuild(&d, &basis, &new, true), &new);
        }
    }

    #[test]
    fn refined_deltas_reproduce_the_new_file_with_less_literal_data(
        len in 0..60_000usize,
        seed in any::<u64>(),
        edits in prop::collection::vec(edit_strategy(), 0..6),
        by_content in any::<bool>(),
    ) {
        let basis = pseudo_random(len, seed);
        let mut new = basis.clone();
        for e in &edits {
            apply_edit(&mut new, e);
        }
        let (n, b) = (new.len() as u64, basis.len() as u64);
        let coarse = if n == b && !by_content {
            diff_blocks(&block_signature(&new, n, &COARSE).unwrap(), &block_signature(&basis, b, &COARSE).unwrap())
        } else {
            diff_chunks(&chunk_signature(&new, n, &COARSE).unwrap(), &chunk_signature(&basis, b, &COARSE).unwrap())
        };
        let gaps: Vec<_> = refine::gaps(&coarse, b).into_iter().filter(|g| g.basis_len > 0).collect();
        let refined = refine::local(&coarse, &new, n, &basis, b, &FINE, &gaps).unwrap();
        check_delta(&refined, &new, &basis);
        prop_assert!(refined.literal_bytes() <= coarse.literal_bytes());
        prop_assert_eq!(&rebuild(&refined, &basis, &new, false), &new);
        prop_assert_eq!(&rebuild(&refined, &basis, &new, true), &new);
    }
}

#[test]
fn gaps_pair_literal_runs_with_their_basis_stretches() {
    let d = Delta {
        size: 200,
        ops: vec![
            Op::Copy { off: 0, basis_off: 0, len: 100 },
            Op::Literal { off: 100, len: 50 },
            Op::Copy { off: 150, basis_off: 120, len: 30 },
            Op::Literal { off: 180, len: 20 },
        ],
    };
    let g = refine::gaps(&d, 210);
    assert_eq!(g, [refine::Gap { off: 100, len: 50, basis_off: 100, basis_len: 20 }, refine::Gap { off: 180, len: 20, basis_off: 150, basis_len: 60 }]);
    // A literal at the start reaches back to the basis start; one with no basis room has none.
    let d = Delta { size: 10, ops: vec![Op::Literal { off: 0, len: 4 }, Op::Copy { off: 4, basis_off: 0, len: 6 }] };
    assert_eq!(refine::gaps(&d, 6), [refine::Gap { off: 0, len: 4, basis_off: 0, basis_len: 0 }]);
}

#[test]
fn refinement_requests_are_checked_and_bad_answers_ignored() {
    use refine::Grain;
    for size in [1 << 20, 256 << 20, 4 << 30] {
        let p = Params::for_size(size);
        assert_eq!(Grain::of(&p).params(), Some(Params { segment: Params::DEFAULT.segment, cdc_region: Params::DEFAULT.cdc_region, ..p }));
    }
    let good = Grain::of(&Params::for_size(1 << 20));
    for bad in [Grain { block: 0, ..good }, Grain { block: 100, ..good }, Grain { block: 3000, ..good }, Grain { block: 128 << 10, ..good }, Grain { min: 0, ..good }] {
        assert_eq!(bad.params(), None, "{bad:?}");
    }
    // An answer of the wrong shape leaves the coarse delta as it was.
    let basis = pseudo_random(300_000, 14);
    let mut new = basis.clone();
    new[100_000..100_010].fill(1);
    let coarse = diff_blocks(&block_signature(&new, 300_000, &Params::DEFAULT).unwrap(), &block_signature(&basis, 300_000, &Params::DEFAULT).unwrap());
    let fine = Params::for_size(300_000);
    let gaps = refine::plan(&coarse, 300_000, Params::DEFAULT.block_size, &fine, refine::REFINE_MIN).unwrap();
    let ours = refine::new_side(&new, 300_000, 300_000, &fine, &gaps).unwrap();
    assert_eq!(refine::combine(&coarse, 300_000, &fine, &gaps, &ours, &[]), coarse);
    let theirs = vec![refine::GapSig::default(); gaps.len()];
    assert_eq!(refine::combine(&coarse, 300_000, &fine, &gaps, &ours, &theirs).literal_bytes(), coarse.literal_bytes());
}

#[test]
fn an_oversized_refinement_request_is_declined_not_signed() {
    // The basis this side holds.
    let basis = pseudo_random(300_000, 7);
    let basis_size = basis.len() as u64;
    let p = Params::for_size(basis_size);
    // A plausible request (one gap spanning the basis) is answered.
    let one = vec![refine::GapReq { off: 0, len: basis_size, chunks: true }];
    assert_eq!(refine::basis_sigs(&basis, basis_size, &p, &one).unwrap().len(), 1);
    // A flood of gaps, each asking for the whole basis: far more than any real refinement
    // needs, so it is declined (empty answer) instead of signing the basis a million times.
    let flood = vec![refine::GapReq { off: 0, len: basis_size, chunks: true }; 1_000_000];
    assert!(refine::basis_sigs(&basis, basis_size, &p, &flood).unwrap().is_empty());
}

#[test]
fn aligned_local_deltas_refine_below_the_block_size() {
    // Coarse blocks of 1 KiB, chunks averaging 256 B, fine blocks of 512 B (like a 4-16 GiB
    // file: 64 KiB blocks, 32 KiB chunks, 32 KiB fine blocks). The aligned pass is taken, so
    // its unit is the block and the 512 B refinement applies, not the smaller chunk size.
    let p = Params { block_size: 1024, segment: 8192, cdc_min: 64, cdc_avg: 256, cdc_max: 1024, cdc_region: 8192 };
    let fine = Params { block_size: 512, ..p };
    let basis = pseudo_random(64 << 10, 17);
    let mut new = basis.clone();
    for i in 0..8 {
        new[i * 8000 + 100] ^= 0xFF;
    }
    let n = new.len() as u64;
    let d = local_delta_with(&new, n, &basis, n, &p, &fine).unwrap();
    check_delta(&d, &new, &basis);
    assert_eq!(d.literal_bytes(), 8 * 512, "one fine block per edit, not one coarse block");
}

#[test]
fn a_rewritten_file_is_not_refined() {
    let (basis, new) = (pseudo_random(1 << 20, 16), pseudo_random(1 << 20, 1016));
    let p = Params::DEFAULT;
    let coarse = diff_blocks(&block_signature(&new, 1 << 20, &p).unwrap(), &block_signature(&basis, 1 << 20, &p).unwrap());
    assert_eq!(coarse.matched_bytes(), 0);
    assert_eq!(refine::plan(&coarse, 1 << 20, p.block_size, &Params::for_size(1 << 20), 0), None);
}

#[test]
fn an_append_is_not_refined() {
    // Only the basis's last chunk differs (its cut moved): not worth a round trip.
    let basis = pseudo_random(8 << 20, 15);
    let mut new = basis.clone();
    new.extend(pseudo_random(500_000, 1015));
    let p = Params::DEFAULT;
    let coarse = diff_chunks(&chunk_signature(&new, new.len() as u64, &p).unwrap(), &chunk_signature(&basis, basis.len() as u64, &p).unwrap());
    assert!(coarse.literal_bytes() < 500_000 + p.cdc_max as u64);
    assert_eq!(refine::plan(&coarse, basis.len() as u64, p.cdc_avg, &Params::for_size(basis.len() as u64), refine::REFINE_MIN), None);
}

#[test]
fn in_place_overwrite_matches_almost_everything() {
    let basis = pseudo_random(1 << 20, 1);
    let mut new = basis.clone();
    new[300_000..300_100].copy_from_slice(&[0xAB; 100]);
    let p = Params::DEFAULT;
    let p = Params { segment: 256 << 10, ..p };
    let d = diff_blocks(&block_signature(&new, new.len() as u64, &p).unwrap(), &block_signature(&basis, basis.len() as u64, &p).unwrap());
    assert!(d.is_in_place());
    assert_eq!(d.literal_bytes(), p.block_size as u64, "one 64 KiB block changed");
}

#[test]
fn insert_at_start_defeats_blocks_but_not_cdc() {
    let basis = pseudo_random(2 << 20, 2);
    let mut new = b"inserted bytes".to_vec();
    new.extend_from_slice(&basis);
    let p = Params::DEFAULT;
    let blocks = diff_blocks(&block_signature(&new, new.len() as u64, &p).unwrap(), &block_signature(&basis, basis.len() as u64, &p).unwrap());
    assert_eq!(blocks.matched_bytes(), 0);
    let cdc = diff_chunks(&chunk_signature(&new, new.len() as u64, &p).unwrap(), &chunk_signature(&basis, basis.len() as u64, &p).unwrap());
    assert!(cdc.matched_bytes() as f64 > 0.9 * new.len() as f64, "cdc matched {} of {}", cdc.matched_bytes(), new.len());
    check_delta(&cdc, &new, &basis);
}

#[test]
fn block_and_chunk_sizes_scale_with_the_file() {
    let unit = |size: u64| Params::for_size(size).block_size >> 10;
    assert_eq!([unit(0), unit(4 << 20), unit(256 << 20)], [4, 4, 4]);
    assert_eq!([unit(1 << 30), unit(4 << 30), unit(16 << 30), unit(17 << 30), unit(1 << 40)], [8, 16, 32, 64, 64]);
    let p = Params::for_size(256 << 20);
    assert_eq!((p.cdc_min, p.cdc_avg, p.cdc_max), (2 << 10, 4 << 10, 8 << 10));
    assert_eq!(p.segment % p.block_size, 0);
}

#[test]
fn chunk_signature_carries_its_chunk_sizes() {
    let data = pseudo_random(100_000, 11);
    let sig = chunk_signature(&data, data.len() as u64, &P).unwrap();
    assert_eq!(sig.params(Params::DEFAULT), Some(Params { cdc_min: 64, cdc_avg: 256, cdc_max: 1024, ..Params::DEFAULT }));
    assert_eq!(sig.iter().map(|(_, len, _)| len as u64).sum::<u64>(), data.len() as u64);
    // Sizes FastCDC would reject (or panic on) are refused.
    for (min, avg, max) in [(0, 256, 1024), (64, 0, 1024), (64, 256, 0), (512, 256, 1024), (64, 2048, 1024), (64, 256, u32::MAX)] {
        let bad = ChunkSig { min, avg, max, ..sig.clone() };
        assert_eq!(bad.params(Params::DEFAULT), None, "{min}/{avg}/{max}");
    }
}

#[test]
fn grown_file_with_scattered_edits_costs_only_its_changed_blocks() {
    // A database-like file: small edits all over, then growth at the end. Coarse chunks
    // lose the chunks around every edit; refined with fine aligned blocks only the touched
    // blocks and the tail are literal.
    let basis = pseudo_random(8 << 20, 12);
    let mut new = basis.clone();
    for i in 0..40 {
        let off = i * 200_000 + 1_234;
        new[off..off + 100].copy_from_slice(&pseudo_random(100, i as u64));
    }
    new.extend(pseudo_random(50_000, 1001));
    let (n, b) = (new.len() as u64, basis.len() as u64);
    let coarse = diff_chunks(&chunk_signature(&new, n, &Params::DEFAULT).unwrap(), &chunk_signature(&basis, b, &Params::DEFAULT).unwrap());
    let p = Params::for_size(b);
    let gaps = refine::plan(&coarse, b, Params::DEFAULT.cdc_avg, &p, refine::REFINE_MIN).expect("worth refining");
    let refined = refine::local(&coarse, &new, n, &basis, b, &p, &gaps).unwrap();
    check_delta(&refined, &new, &basis);
    assert!(refined.is_in_place());
    let most = 40 * 2 * p.block_size as u64 + 50_000 + p.block_size as u64;
    assert!(refined.literal_bytes() <= most, "refined literal {} > {most}", refined.literal_bytes());
    assert!(refined.literal_bytes() * 4 < coarse.literal_bytes(), "refining must beat coarse chunks here");
    // local_delta (coarse blocks here, since most of them match) refines just as well.
    let local = local_delta(&new, n, &basis, b, &Params::DEFAULT).unwrap();
    check_delta(&local, &new, &basis);
    assert!(local.literal_bytes() <= most, "local literal {} > {most}", local.literal_bytes());
}

#[test]
fn appended_file_keeps_its_prefix() {
    let basis = pseudo_random(500_000, 3);
    let mut new = basis.clone();
    new.extend(pseudo_random(10_000, 4));
    let p = Params { segment: 256 << 10, ..Params::DEFAULT };
    let d = diff_blocks(&block_signature(&new, new.len() as u64, &p).unwrap(), &block_signature(&basis, basis.len() as u64, &p).unwrap());
    // Everything up to the basis's last partial block matches.
    let full_blocks = basis.len() as u64 / p.block_size as u64 * p.block_size as u64;
    assert_eq!(d.matched_bytes(), full_blocks);
}

// ---------------------------------------------------------------------------
// --delta=cdc
// ---------------------------------------------------------------------------

/// Data with long stretches where FastCDC finds no cut (forced cuts at `cdc_max`),
/// mixed with random runs, so regions start and end in every kind of chunk.
fn mixed(len: usize, seed: u64) -> Vec<u8> {
    let r = pseudo_random(len, seed);
    r.iter().enumerate().map(|(i, &b)| if (i / 3000) % 3 == 1 { (i % 7) as u8 } else { b }).collect()
}

fn in_pool<T: Send>(threads: usize, f: impl FnOnce() -> T + Send) -> T {
    rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap().install(f)
}

proptest! {
    #[test]
    fn parallel_cdc_equals_sequential_fastcdc(
        len in 0..60_000usize,
        seed in any::<u64>(),
        region in prop::sample::select(vec![2048usize, 3001, 4096, 7777]),
        low_entropy in any::<bool>(),
    ) {
        let data = if low_entropy { mixed(len, seed) } else { pseudo_random(len, seed) };
        let p = Params { cdc_region: region, segment: region / 1024 * 1024 + 1024, ..P };
        let want = cdc::chunks_sequential(&data, &p);
        for threads in [1, 2, 8] {
            prop_assert_eq!(&in_pool(threads, || cdc::chunks(&data, &p)), &want, "threads {}", threads);
            // The blocks design's CDC fallback cuts at the same places.
            let sig = in_pool(threads, || chunk_signature(&data, data.len() as u64, &p).unwrap());
            let got: Vec<(u64, u32)> = sig.iter().map(|(off, len, _)| (off, len)).collect();
            prop_assert_eq!(&got, &want, "chunk_signature, threads {}", threads);
        }
    }

    #[test]
    fn cdc_deltas_always_reproduce_the_new_file(
        len in 0..40_000usize,
        seed in any::<u64>(),
        edits in prop::collection::vec(edit_strategy(), 0..6),
    ) {
        let basis = pseudo_random(len, seed);
        let mut new = basis.clone();
        for e in &edits {
            apply_edit(&mut new, e);
        }
        let remote = cdc::diff(&new, &cdc::signature(&basis, &P), &P);
        let local = cdc::local_delta(&new, &basis, &P);
        prop_assert_eq!(&remote, &local);
        check_delta(&local, &new, &basis);
        prop_assert_eq!(&rebuild(&local, &basis, &new, false), &new);
        prop_assert_eq!(&rebuild(&local, &basis, &new, true), &new);
    }
}

#[test]
fn parallel_cdc_equals_sequential_with_default_chunk_sizes() {
    let data: Vec<u8> = [pseudo_random(3 << 20, 7), vec![0; 1 << 20], mixed(2 << 20, 8)].concat();
    let p = Params { cdc_region: 1 << 20, ..Params::DEFAULT };
    let want = cdc::chunks_sequential(&data, &p);
    for threads in [1, 2, 8] {
        assert_eq!(in_pool(threads, || cdc::chunks(&data, &p)), want, "threads {threads}");
    }
}

#[test]
fn cdc_matches_need_blake3_too() {
    let basis = pseudo_random(1 << 20, 5);
    let p = Params::DEFAULT;
    let mut sig = cdc::signature(&basis, &p);
    assert_eq!(cdc::diff(&basis, &sig, &p).literal_bytes(), 0);
    // Same xxh3 hashes, different BLAKE3: nothing may match.
    for c in &mut sig.chunks {
        c.blake3_hash[0] ^= 1;
    }
    assert_eq!(cdc::diff(&basis, &sig, &p).matched_bytes(), 0);
}

#[test]
fn cdc_handles_inserts_and_in_place_edits() {
    let basis = pseudo_random(4 << 20, 6);
    let p = Params::DEFAULT;
    let mut inserted = b"inserted bytes".to_vec();
    inserted.extend_from_slice(&basis);
    let d = cdc::local_delta(&inserted, &basis, &p);
    assert!(d.matched_bytes() as f64 > 0.95 * inserted.len() as f64, "matched {} of {}", d.matched_bytes(), inserted.len());
    check_delta(&d, &inserted, &basis);
    let mut edited = basis.clone();
    edited[2_000_000..2_000_100].fill(0xAB);
    let d = cdc::local_delta(&edited, &basis, &p);
    assert!(d.is_in_place(), "an overwrite keeps every match in place");
    assert!(d.literal_bytes() <= 2 * p.cdc_max as u64, "literal {}", d.literal_bytes());
}

#[test]
fn mapped_file_rejects_a_changed_size() {
    let mut f = tempfile::tempfile().unwrap();
    f.write_all(&pseudo_random(10_000, 9)).unwrap();
    let m = cdc::Mapped::new(&f, 10_000).unwrap();
    assert_eq!(m.bytes(), &pseudo_random(10_000, 9)[..]);
    drop(m);
    let e = cdc::Mapped::new(&f, 12_000).err().unwrap();
    assert!(cdc::is_changed(&e), "{e}");
}
