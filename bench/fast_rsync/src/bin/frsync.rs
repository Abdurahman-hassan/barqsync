//! frsync: a minimal directory push over a remote shell built on fast_rsync 0.2.0, so the
//! library can be compared with barqsync on a real link.
//!
//!   frsync [-e RSH] [--block N] SRC_DIR HOST:DST_DIR
//!
//! Flow (one round trip, pipelined like barqsync's per-directory request):
//!   client -> server  FILELIST (relative path, size) for every regular file under SRC_DIR
//!   server -> client  SIG per file: the MD4 signature of the existing destination file
//!   client -> server  FILE per file: a librsync delta (or the whole file) + BLAKE3 of the result
//!   client -> server  END
//!   server -> client  DONE once every file is written
//! The server applies each delta in memory, checks the BLAKE3 hash (fast_rsync's README
//! requires verifying the output, since MD4 is insecure), writes a temp file and renames it.
//! Signatures, diffs and applies run in parallel across files (rayon); fast_rsync itself is
//! single-threaded per file.

use fast_rsync::{Signature, SignatureOptions, apply_limited, diff};
use std::fs::{self, File, FileTimes};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const FILELIST: u8 = 1;
const SIG: u8 = 2;
const FILE: u8 = 3;
const END: u8 = 4;
const DONE: u8 = 5;

const KIND_WHOLE: u8 = 0;
const KIND_DELTA: u8 = 1;
const CRYPTO_HASH_SIZE: u32 = 8;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = if args.first().map(String::as_str) == Some("--server") { server(&args[1..]) } else { client(&args) };
    if let Err(e) = r {
        eprintln!("frsync: {e}");
        std::process::exit(1);
    }
}

/// rsync's rule of thumb: about sqrt(size), a multiple of 8, within [700, 128 KiB].
fn block_size(size: u64, fixed: Option<u32>) -> u32 {
    fixed.unwrap_or_else(|| (((size as f64).sqrt() as u32) / 8 * 8).clamp(700, 128 << 10))
}

// ---------------------------------------------------------------------------
// Framing: tag u8, length u64 LE, payload.
// ---------------------------------------------------------------------------

fn frame(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let mut v = Vec::with_capacity(9 + len);
    v.push(tag);
    v.extend_from_slice(&(len as u64).to_le_bytes());
    for p in parts {
        v.extend_from_slice(p);
    }
    v
}

fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 9];
    r.read_exact(&mut h)?;
    let len = u64::from_le_bytes(h[1..9].try_into().unwrap()) as usize;
    let mut v = vec![0u8; len];
    r.read_exact(&mut v)?;
    Ok((h[0], v))
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

/// A writer thread that owns the pipe and counts the bytes it writes.
fn spawn_writer(mut w: impl Write + Send + 'static, sent: &'static AtomicU64) -> (mpsc::Sender<Vec<u8>>, std::thread::JoinHandle<io::Result<()>>) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let h = std::thread::spawn(move || {
        // Flush whenever the queue runs dry, so a peer waiting on a frame gets it.
        while let Ok(f) = rx.recv() {
            w.write_all(&f)?;
            sent.fetch_add(f.len() as u64, Ordering::Relaxed);
            while let Ok(f) = rx.try_recv() {
                w.write_all(&f)?;
                sent.fetch_add(f.len() as u64, Ordering::Relaxed);
            }
            w.flush()?;
        }
        w.flush()
    });
    (tx, h)
}

fn hash(data: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update_rayon(data);
    *h.finalize().as_bytes()
}

// ---------------------------------------------------------------------------
// Client (sender)
// ---------------------------------------------------------------------------

struct SrcFile {
    rel: String,
    path: PathBuf,
    size: u64,
    mtime: SystemTime,
}

fn walk(root: &Path, rel: &str, out: &mut Vec<SrcFile>) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(root.join(rel))?.collect::<io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().into_string().map_err(|_| io::Error::other("non-UTF-8 name"))?;
        let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let ft = e.file_type()?;
        if ft.is_dir() {
            walk(root, &r, out)?;
        } else if ft.is_file() {
            let m = e.metadata()?;
            out.push(SrcFile { rel: r, path: e.path(), size: m.len(), mtime: m.modified()? });
        }
    }
    Ok(())
}

fn client(args: &[String]) -> io::Result<()> {
    let mut rsh = "ssh".to_string();
    let mut block: Option<u32> = None;
    let mut pos = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-e" => rsh = it.next().cloned().unwrap_or_default(),
            "--block" => block = it.next().and_then(|s| s.parse().ok()),
            _ => pos.push(a.clone()),
        }
    }
    let [src, dst] = pos.as_slice() else {
        return Err(io::Error::other("usage: frsync [-e RSH] [--block N] SRC_DIR HOST:DST_DIR"));
    };
    let (host, dst_dir) = dst.split_once(':').ok_or_else(|| io::Error::other("DST must be HOST:DIR"))?;
    let src = Path::new(src.trim_end_matches('/'));
    let mut files = Vec::new();
    walk(src, "", &mut files)?;

    let mut parts = rsh.split_whitespace();
    let mut cmd = Command::new(parts.next().unwrap());
    cmd.args(parts).arg(host).arg("frsync").arg("--server").arg(dst_dir);
    if let Some(b) = block {
        cmd.arg(b.to_string());
    }
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    static SENT: AtomicU64 = AtomicU64::new(0);
    let (tx, writer) = spawn_writer(BufWriter::with_capacity(1 << 20, child.stdin.take().unwrap()), &SENT);
    let mut rd = BufReader::with_capacity(1 << 20, child.stdout.take().unwrap());
    let mut received = 0u64;

    let mut list = Vec::new();
    list.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for f in &files {
        list.extend_from_slice(&(f.rel.len() as u32).to_le_bytes());
        list.extend_from_slice(f.rel.as_bytes());
        list.extend_from_slice(&f.size.to_le_bytes());
    }
    tx.send(frame(FILELIST, &[&list])).unwrap();

    let delta_bytes = AtomicU64::new(0);
    let whole_bytes = AtomicU64::new(0);
    let err: std::sync::Mutex<Option<io::Error>> = Default::default();
    rayon::scope(|s| -> io::Result<()> {
        for _ in 0..files.len() {
            let (tag, body) = read_frame(&mut rd)?;
            received += 9 + body.len() as u64;
            if tag != SIG {
                return Err(io::Error::other(format!("expected SIG, got {tag}")));
            }
            let (files, tx, delta_bytes, whole_bytes, err) = (&files, tx.clone(), &delta_bytes, &whole_bytes, &err);
            s.spawn(move |_| {
                let r = (|| -> io::Result<()> {
                    let idx = u32_at(&body, 0) as usize;
                    let f = &files[idx];
                    let data = fs::read(&f.path)?;
                    let mut kind = KIND_WHOLE;
                    let mut payload = None;
                    if body[4] == 1 {
                        let sig = Signature::deserialize(body[5..].to_vec()).map_err(io::Error::other)?;
                        let mut d = Vec::new();
                        diff(&sig.index(), &data, &mut d).map_err(io::Error::other)?;
                        if d.len() < data.len() {
                            kind = KIND_DELTA;
                            payload = Some(d);
                        }
                    }
                    let h = hash(&data);
                    let mt = f.mtime.duration_since(UNIX_EPOCH).unwrap_or_default();
                    let body = payload.as_deref().unwrap_or(&data);
                    if kind == KIND_DELTA { delta_bytes } else { whole_bytes }.fetch_add(body.len() as u64, Ordering::Relaxed);
                    tx.send(frame(
                        FILE,
                        &[&(idx as u32).to_le_bytes(), &[kind], &h, &mt.as_secs().to_le_bytes(), &mt.subsec_nanos().to_le_bytes(), body],
                    ))
                    .unwrap();
                    Ok(())
                })();
                if let Err(e) = r {
                    err.lock().unwrap().get_or_insert(e);
                }
            });
        }
        Ok(())
    })?;
    if let Some(e) = err.into_inner().unwrap() {
        return Err(e);
    }
    tx.send(frame(END, &[])).unwrap();
    let (tag, body) = read_frame(&mut rd)?;
    received += 9 + body.len() as u64;
    if tag != DONE {
        return Err(io::Error::other(format!("expected DONE, got {tag}")));
    }
    drop(tx);
    writer.join().unwrap()?;
    child.wait()?;
    eprintln!(
        "frsync: {} files, sent {} bytes (delta payload {}, whole-file payload {}), received {} bytes",
        files.len(),
        SENT.load(Ordering::Relaxed),
        delta_bytes.load(Ordering::Relaxed),
        whole_bytes.load(Ordering::Relaxed),
        received
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Server (receiver)
// ---------------------------------------------------------------------------

fn server(args: &[String]) -> io::Result<()> {
    let dst = PathBuf::from(args.first().ok_or_else(|| io::Error::other("--server needs DST"))?);
    let block = args.get(1).and_then(|s| s.parse().ok());
    fs::create_dir_all(&dst)?;
    let mut rd = BufReader::with_capacity(1 << 20, io::stdin());
    static SENT: AtomicU64 = AtomicU64::new(0);
    let (tx, writer) = spawn_writer(BufWriter::with_capacity(1 << 20, io::stdout()), &SENT);

    let (tag, list) = read_frame(&mut rd)?;
    if tag != FILELIST {
        return Err(io::Error::other("expected FILELIST"));
    }
    let n = u32_at(&list, 0) as usize;
    let mut files: Vec<(PathBuf, u64)> = Vec::with_capacity(n);
    let mut off = 4;
    for _ in 0..n {
        let l = u32_at(&list, off) as usize;
        let rel = std::str::from_utf8(&list[off + 4..off + 4 + l]).map_err(io::Error::other)?;
        off += 4 + l;
        files.push((dst.join(rel), u64_at(&list, off)));
        off += 8;
    }

    let err: std::sync::Mutex<Option<io::Error>> = Default::default();
    let fail = |e: io::Error| {
        err.lock().unwrap().get_or_insert(e);
    };
    rayon::scope(|s| -> io::Result<()> {
        // Signatures for every file, sent as they finish, while FILE frames come back.
        let (files, sig_tx) = (&files, tx.clone());
        s.spawn(move |_| {
            use rayon::prelude::*;
            (0..files.len()).into_par_iter().for_each(|idx| {
                let old = fs::symlink_metadata(&files[idx].0).ok().filter(|m| m.is_file() && m.len() > 0).and_then(|_| fs::read(&files[idx].0).ok());
                let msg = match old {
                    Some(old) => {
                        let opts = SignatureOptions { block_size: block_size(old.len() as u64, block), crypto_hash_size: CRYPTO_HASH_SIZE };
                        let sig = Signature::calculate(&old, opts);
                        frame(SIG, &[&(idx as u32).to_le_bytes(), &[1], sig.serialized()])
                    }
                    None => frame(SIG, &[&(idx as u32).to_le_bytes(), &[0]]),
                };
                sig_tx.send(msg).unwrap();
            });
        });
        loop {
            let (tag, body) = read_frame(&mut rd)?;
            match tag {
                END => break,
                FILE => {
                    let fail = &fail;
                    s.spawn(move |_| {
                        if let Err(e) = write_file(files, &body) {
                            fail(e);
                        }
                    });
                }
                t => return Err(io::Error::other(format!("unexpected frame {t}"))),
            }
        }
        Ok(())
    })?;
    if let Some(e) = err.into_inner().unwrap() {
        return Err(e);
    }
    tx.send(frame(DONE, &[])).unwrap();
    drop(tx);
    writer.join().unwrap()
}

fn write_file(files: &[(PathBuf, u64)], body: &[u8]) -> io::Result<()> {
    let idx = u32_at(body, 0) as usize;
    let kind = body[4];
    let want: [u8; 32] = body[5..37].try_into().unwrap();
    let mtime = UNIX_EPOCH + Duration::new(u64_at(body, 37), u32_at(body, 45));
    let payload = &body[49..];
    let (path, size) = &files[idx];
    let applied;
    let data: &[u8] = if kind == KIND_DELTA {
        let old = fs::read(path)?;
        let mut out = Vec::with_capacity(*size as usize);
        apply_limited(&old, payload, &mut out, *size as usize).map_err(io::Error::other)?;
        applied = out;
        &applied
    } else {
        payload
    };
    if hash(data) != want {
        return Err(io::Error::other(format!("{}: BLAKE3 mismatch after applying the delta", path.display())));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(format!(".{}.frsync", path.file_name().unwrap().to_string_lossy()));
    let mut f = File::create(&tmp)?;
    f.write_all(data)?;
    f.set_times(FileTimes::new().set_modified(mtime))?;
    drop(f);
    fs::rename(&tmp, path)
}
