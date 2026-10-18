//! In-process delta engine comparison on one file pair, no network:
//!
//!   engine OLD NEW
//!
//! fast_rsync: MD4 signature of OLD (rsync's sqrt block size, 8-byte strong hash), rolling
//! diff of NEW against it, apply in memory, BLAKE3 check of the result.
//! barqsync: what its remote push does: the receiver's basis signature with block and chunk
//! sizes scaled to the file (aligned blocks if the size is unchanged, blocks and FastCDC
//! chunks otherwise), the sender's signatures of NEW, the diff (both deltas merged when the
//! size changed), then the rebuild into a file.
//! Prints one tab-separated line per engine.

use barqsync::delta::{self, Params, ReadAt};
use fast_rsync::{Signature, SignatureOptions, apply_limited, diff};
use std::fs::{self, File};
use std::time::Instant;

/// Postcard-encoded length, as barqsync sends it.
fn wire<T: serde::Serialize>(v: &T) -> usize {
    postcard::to_allocvec(v).unwrap().len()
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (old_path, new_path) = (&args[0], &args[1]);
    let old = fs::read(old_path).unwrap();
    let new = fs::read(new_path).unwrap();
    let want = blake3::hash(&new);

    // fast_rsync
    let bs = (((old.len() as f64).sqrt() as u32) / 8 * 8).clamp(700, 128 << 10);
    let t = Instant::now();
    let sig = Signature::calculate(&old, SignatureOptions { block_size: bs, crypto_hash_size: 8 });
    let t_sig = ms(t);
    let t = Instant::now();
    let idx = sig.index();
    let mut d = Vec::new();
    diff(&idx, &new, &mut d).unwrap();
    let t_diff = ms(t);
    let t = Instant::now();
    let mut out = Vec::with_capacity(new.len());
    apply_limited(&old, &d, &mut out, new.len()).unwrap();
    let t_apply = ms(t);
    let t = Instant::now();
    let mut h = blake3::Hasher::new();
    h.update_rayon(&out);
    assert_eq!(h.finalize(), want, "fast_rsync output differs");
    let t_verify = ms(t);
    println!(
        "fast_rsync\tblock={bs}\tsig_bytes={}\tdelta_bytes={}\tsig_ms={t_sig:.1}\tdiff_ms={t_diff:.1}\tapply_ms={t_apply:.1}\tverify_ms={t_verify:.1}\ttotal_ms={:.1}",
        sig.serialized().len(),
        d.len(),
        t_sig + t_diff + t_apply + t_verify
    );
    drop((out, idx));

    // barqsync
    let p = Params::for_size(old.len() as u64);
    let (of, nf) = (File::open(old_path).unwrap(), File::open(new_path).unwrap());
    let same = old.len() == new.len();
    let t = Instant::now();
    let (bsig, csig) = if same {
        (delta::block_signature(&of, old.len() as u64, &p).unwrap(), None)
    } else {
        let (b, c) = delta::signatures(&of, old.len() as u64, &p).unwrap();
        (b, Some(c))
    };
    let t_sig = ms(t);
    let t = Instant::now();
    let dl = match &csig {
        None => delta::diff_blocks(&delta::block_signature(&nf, new.len() as u64, &p).unwrap(), &bsig),
        Some(c) => {
            let (nb, nc) = delta::signatures(&nf, new.len() as u64, &p).unwrap();
            delta::merge_in_place(&delta::diff_chunks(&nc, c), &delta::diff_blocks(&nb, &bsig))
        }
    };
    let t_diff = ms(t);
    // Wire sizes (postcard, as barqsync sends them).
    let sig_bytes = wire(&bsig) + csig.as_ref().map_or(0, wire);
    let ops_bytes = wire(&dl.ops);
    let out_path = format!("{new_path}.engine-out");
    let _ = fs::remove_file(&out_path);
    let t = Instant::now();
    let out = File::options().create(true).read(true).write(true).truncate(true).open(&out_path).unwrap();
    let lit: &dyn ReadAt = &nf;
    delta::apply::apply(&dl, &of, lit, &out, false).unwrap();
    let t_apply = ms(t);
    drop(out);
    assert_eq!(blake3::hash(&fs::read(&out_path).unwrap()), want, "barqsync output differs");
    fs::remove_file(&out_path).unwrap();
    println!(
        "barqsync\tmode={}K-{}\tsig_bytes={sig_bytes}\tdelta_bytes={}\tsig_ms={t_sig:.1}\tdiff_ms={t_diff:.1}\tapply_ms={t_apply:.1}\tverify_ms=0.0\ttotal_ms={:.1}",
        p.block_size >> 10,
        if same { "blocks" } else { "both" },
        dl.literal_bytes() as usize + ops_bytes,
        t_sig + t_diff + t_apply
    );
}

