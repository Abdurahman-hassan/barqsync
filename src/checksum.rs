//! Whole-file checksums for `-c/--checksum` (BLAKE3, truncated to 128 bits).
//!
//! Files are hashed in parallel on a dedicated pool. Callers block on a channel rather
//! than joining rayon work: the engine's tasks may block on network backpressure, and a
//! rayon join would let their threads steal (and nest) more such tasks.

use crossbeam_channel::bounded;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub type Sum = [u8; 16];

static POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| {
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        .thread_name(|i| format!("barqsync-sum-{i}"))
        .build()
        .expect("checksum thread pool")
});

pub fn file_sum(path: &Path) -> io::Result<Sum> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        crate::fsops::check_interrupt()?;
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                h.update(&buf[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    Ok(out)
}

/// Checksums of `paths`, in order, computed in parallel.
pub fn sums(paths: Vec<PathBuf>) -> Vec<io::Result<Sum>> {
    if paths.len() <= 1 {
        return paths.iter().map(|p| file_sum(p)).collect();
    }
    let n = paths.len();
    let (tx, rx) = bounded(n);
    for (i, p) in paths.into_iter().enumerate() {
        let tx = tx.clone();
        POOL.spawn(move || {
            let _ = tx.send((i, file_sum(&p)));
        });
    }
    drop(tx);
    let mut out: Vec<Option<io::Result<Sum>>> = (0..n).map(|_| None).collect();
    for (i, r) in rx.iter() {
        out[i] = Some(r);
    }
    out.into_iter().map(|r| r.unwrap_or_else(|| Err(io::Error::other("checksum worker failed")))).collect()
}
