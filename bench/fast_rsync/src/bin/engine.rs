//! In-process delta engine comparison on one file pair, no network:
//!
//!   engine OLD NEW
//!
//! fast_rsync: MD4 signature of OLD (rsync's sqrt block size, 8-byte strong hash), rolling
//! diff of NEW against it, apply in memory, BLAKE3 check of the result.
//! barqsync: what its remote push does: the receiver's coarse basis signature (aligned blocks
//! if the size is unchanged, FastCDC chunks otherwise), the sender's signature of NEW and the
//! diff, the refinement of what is still literal where it is worth it (both sides' fine
//! signatures), then the rebuild into a file.
//! Prints one tab-separated line per engine.

use barqsync::delta::{self, Params, ReadAt, refine};
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
    let (of, nf) = (File::open(old_path).unwrap(), File::open(new_path).unwrap());
    let (os, ns) = (old.len() as u64, new.len() as u64);
    let same = os == ns;
    let p = Params::DEFAULT;
    let t = Instant::now();
    let (bsig, csig) = if same {
        (Some(delta::block_signature(&of, os, &p).unwrap()), None)
    } else {
        (None, Some(delta::chunk_signature(&of, os, &p).unwrap()))
    };
    let t_sig = ms(t);
    let t = Instant::now();
    let coarse = match (&bsig, &csig) {
        (Some(b), _) => delta::diff_blocks(&delta::block_signature(&nf, ns, &p).unwrap(), b),
        (_, Some(c)) => delta::diff_chunks(&delta::chunk_signature(&nf, ns, &p).unwrap(), c),
        _ => unreachable!(),
    };
    // Refinement: the request and both sides' fine signatures, as over the wire.
    let unit = if same { p.block_size } else { p.cdc_avg };
    let fine = Params::for_size(os);
    let (dl, refine_bytes) = match refine::plan(&coarse, os, unit, &fine, refine::REFINE_MIN) {
        Some(gaps) => {
            let reqs: Vec<refine::GapReq> = gaps.iter().map(refine::Gap::request).collect();
            let theirs = refine::basis_sigs(&of, os, &fine, &reqs).unwrap();
            let ours = refine::new_side(&nf, ns, os, &fine, &gaps).unwrap();
            (refine::combine(&coarse, os, &fine, &gaps, &ours, &theirs), wire(&reqs) + wire(&theirs))
        }
        None => (coarse, 0),
    };
    let t_diff = ms(t);
    let sig_bytes = bsig.as_ref().map_or(0, wire) + csig.as_ref().map_or(0, wire) + refine_bytes;
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
        "barqsync\tmode={}{}\tsig_bytes={sig_bytes}\tdelta_bytes={}\tsig_ms={t_sig:.1}\tdiff_ms={t_diff:.1}\tapply_ms={t_apply:.1}\tverify_ms=0.0\ttotal_ms={:.1}",
        if same { "blocks" } else { "chunks" },
        if refine_bytes > 0 { "+refine" } else { "" },
        dl.literal_bytes() as usize + ops_bytes,
        t_sig + t_diff + t_apply
    );
}

