//! User-facing output: itemized names (rsync `%n%L` style), deletions, errors and stats.

use crate::entry::Kind;
use std::io::{BufWriter, Stdout, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

pub struct Output {
    out: Mutex<BufWriter<Stdout>>,
    verbose: u8,
    format: Option<String>,
}

impl Output {
    pub fn new(verbose: u8, out_format: Option<String>) -> Output {
        let format = out_format.or_else(|| (verbose > 0).then(|| "%n%L".to_string()));
        Output { out: Mutex::new(BufWriter::with_capacity(64 * 1024, std::io::stdout())), verbose, format }
    }

    pub fn verbose(&self) -> u8 {
        self.verbose
    }

    fn line(&self, bytes: &[u8]) {
        let mut out = self.out.lock().unwrap();
        let _ = out.write_all(bytes);
        let _ = out.write_all(b"\n");
    }

    /// Report a transferred/created item. `rel` is relative to the transfer root
    /// (empty for the root itself, shown as `./`).
    pub fn item(&self, rel: &[u8], kind: Kind, link_target: Option<&[u8]>) {
        let Some(fmt) = &self.format else { return };
        let mut name = if rel.is_empty() { b".".to_vec() } else { rel.to_vec() };
        if kind == Kind::Dir {
            name.push(b'/');
        }
        let mut line = Vec::with_capacity(name.len() + 16);
        let mut chars = fmt.as_bytes().iter().copied().peekable();
        while let Some(c) = chars.next() {
            if c != b'%' {
                line.push(c);
                continue;
            }
            match chars.next() {
                Some(b'n') => line.extend_from_slice(&name),
                Some(b'L') => {
                    if let Some(t) = link_target {
                        line.extend_from_slice(b" -> ");
                        line.extend_from_slice(t);
                    }
                }
                Some(b'%') => line.push(b'%'),
                Some(other) => {
                    line.push(b'%');
                    line.push(other);
                }
                None => line.push(b'%'),
            }
        }
        self.line(&line);
    }

    pub fn deleting(&self, rel: &[u8], is_dir: bool) {
        if self.verbose == 0 {
            return;
        }
        let mut line = b"deleting ".to_vec();
        line.extend_from_slice(rel);
        if is_dir {
            line.push(b'/');
        }
        self.line(&line);
    }

    /// Informational message shown with `-v`.
    pub fn info(&self, msg: impl AsRef<[u8]>) {
        if self.verbose > 0 {
            self.line(msg.as_ref());
        }
    }

    /// Always shown (unless redirected); used for summaries.
    pub fn always(&self, msg: impl AsRef<[u8]>) {
        self.line(msg.as_ref());
    }

    pub fn error(&self, msg: impl std::fmt::Display) {
        self.flush();
        eprintln!("fastrsync: {msg}");
    }

    pub fn flush(&self) {
        let _ = self.out.lock().unwrap().flush();
    }
}

#[derive(Default)]
pub struct Stats {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub links: AtomicU64,
    pub specials: AtomicU64,
    pub total_size: AtomicU64,
    pub created: AtomicU64,
    pub transferred: AtomicU64,
    pub transferred_bytes: AtomicU64,
    pub cloned: AtomicU64,
    pub deleted: AtomicU64,
    pub errors: AtomicU64,
}

impl Stats {
    pub fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Relaxed);
    }

    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Relaxed)
    }

    pub fn print(&self, out: &Output, elapsed: Duration, human: bool, full: bool) {
        let g = Self::get;
        let num = |n: u64| if human { human_num(n) } else { n.to_string() };
        let secs = elapsed.as_secs_f64().max(1e-9);
        if full {
            let total = g(&self.files) + g(&self.dirs) + g(&self.links) + g(&self.specials);
            out.always(format!(
                "\nNumber of files: {} (reg: {}, dir: {}, link: {}, special: {})",
                num(total),
                num(g(&self.files)),
                num(g(&self.dirs)),
                num(g(&self.links)),
                num(g(&self.specials))
            ));
            out.always(format!("Number of created files: {}", num(g(&self.created))));
            out.always(format!("Number of deleted files: {}", num(g(&self.deleted))));
            out.always(format!(
                "Number of regular files transferred: {} ({} cloned)",
                num(g(&self.transferred)),
                num(g(&self.cloned))
            ));
            out.always(format!("Total file size: {} bytes", num(g(&self.total_size))));
            out.always(format!(
                "Total transferred file size: {} bytes",
                num(g(&self.transferred_bytes))
            ));
            out.always(format!("Elapsed: {secs:.3}s"));
        }
        out.always(format!(
            "\ntransferred {} files, {} bytes in {:.3}s ({}/s); total size is {}",
            num(g(&self.transferred)),
            num(g(&self.transferred_bytes)),
            secs,
            human_num((g(&self.transferred_bytes) as f64 / secs) as u64),
            num(g(&self.total_size)),
        ));
    }
}

pub fn human_num(n: u64) -> String {
    const UNITS: [&str; 6] = ["", "K", "M", "G", "T", "P"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n}") } else { format!("{v:.2}{}", UNITS[i]) }
}
