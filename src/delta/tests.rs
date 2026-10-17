use super::apply::apply;
use super::*;
use proptest::prelude::*;
use std::io::Write;

/// Tiny sizes so property tests exercise many blocks, segments and chunks quickly.
const P: Params = Params { block_size: 256, segment: 4096, cdc_min: 64, cdc_avg: 256, cdc_max: 1024, cdc_region: 4096 };

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
        for d in [diff_blocks(&bn, &bb), diff_chunks(&cn, &cb)] {
            check_delta(&d, &new, &basis);
            prop_assert_eq!(&rebuild(&d, &basis, &new, false), &new);
            prop_assert_eq!(&rebuild(&d, &basis, &new, true), &new);
        }
    }
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
            let got: Vec<(u64, u32)> = sig.chunks.iter().map(|c| (c.off, c.len)).collect();
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
