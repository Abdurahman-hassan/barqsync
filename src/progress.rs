//! Overall progress (`--progress`, `-P`, `--info=progress2`): one status line, redrawn
//! in place, in the style of rsync's `--info=progress2`:
//!
//! ```text
//!     123,456,789  45%   12.34MB/s    0:00:12 (xfr#123, to-chk=1000/2345)
//! ```
//!
//! Files are transferred in parallel, so rsync's per-file `--progress` lines would
//! interleave meaninglessly; every progress flag shows this overall line instead.
//! "Planned" grows while the scan is still finding files, like rsync's incremental `ir-chk`.

use crate::output::{Output, human_num};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

static PLANNED_BYTES: AtomicU64 = AtomicU64::new(0);
static PLANNED_FILES: AtomicU64 = AtomicU64::new(0);
static DONE_BYTES: AtomicU64 = AtomicU64::new(0);
static DONE_FILES: AtomicU64 = AtomicU64::new(0);

/// A file of `size` bytes will be transferred.
pub fn planned(size: u64) {
    PLANNED_BYTES.fetch_add(size, Relaxed);
    PLANNED_FILES.fetch_add(1, Relaxed);
}

/// `n` more bytes of a planned file have been written (or sent), before it is done, so a
/// big file moves the line while it travels instead of all at once at the end.
pub fn bytes(n: u64) {
    DONE_BYTES.fetch_add(n, Relaxed);
}

/// A planned file has been written (or sent, on a sending client); `rest` is the part of
/// its size not already counted with [`bytes`] (all of it, for a file written in one go).
pub fn done(rest: u64) {
    DONE_BYTES.fetch_add(rest, Relaxed);
    DONE_FILES.fetch_add(1, Relaxed);
}

fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `last`: the scan is over, so the file count is final (`to-chk`); before that the
/// count is still growing (`ir-chk`, as rsync labels its incremental file list).
fn status(started: Instant, human: bool, last: bool) -> String {
    let (pb, pf) = (PLANNED_BYTES.load(Relaxed), PLANNED_FILES.load(Relaxed));
    let (db, df) = (DONE_BYTES.load(Relaxed), DONE_FILES.load(Relaxed));
    let secs = started.elapsed().as_secs_f64();
    // Nothing planned yet reads 0%, not 100%, until the scan is over.
    let all_done = pf == df && (last || pf > 0);
    let pct = (db * 100).checked_div(pb).map_or(if all_done { 100 } else { 0 }, |p| p.min(100));
    let rate = if secs > 0.0 { (db as f64 / secs) as u64 } else { 0 };
    let t = secs as u64;
    let bytes = if human { human_num(db) } else { grouped(db) };
    format!(
        "{bytes:>15} {pct:>3}% {:>10}B/s {:>4}:{:02}:{:02} (xfr#{df}, {}={}/{pf})",
        human_num(rate),
        t / 3600,
        t / 60 % 60,
        t % 60,
        if last { "to-chk" } else { "ir-chk" },
        pf.saturating_sub(df),
    )
}

/// Redraws the status line twice a second until dropped; the final state is printed
/// on its own line.
pub struct Reporter {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
    started: Instant,
    human: bool,
}

impl Reporter {
    pub fn start(out: &'static Output, human: bool) -> Reporter {
        let started = Instant::now();
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("barqsync-progress".into())
            .spawn(move || {
                let (lock, cvar) = &*stop2;
                let mut stopped = lock.lock().unwrap();
                while !*stopped {
                    out.progress(status(started, human, false).as_bytes(), false);
                    stopped = cvar.wait_timeout(stopped, Duration::from_millis(500)).unwrap().0;
                }
            })
            .ok();
        Reporter { stop, thread, started, human }
    }

    /// Stop redrawing and leave the final status on screen.
    pub fn finish(mut self, out: &Output) {
        self.halt();
        out.progress(status(self.started, self.human, true).as_bytes(), true);
    }

    fn halt(&mut self) {
        *self.stop.0.lock().unwrap() = true;
        self.stop.1.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        self.halt();
    }
}

#[cfg(test)]
mod tests {
    use super::grouped;

    #[test]
    fn thousands_separators() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(123456789), "123,456,789");
    }
}
