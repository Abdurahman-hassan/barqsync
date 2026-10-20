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
//! As in rsync, a running line shows the speed of the last few seconds and the time left;
//! the final line shows the average speed and the total time.

use crate::output::{Output, human_num};
use std::cell::Cell;
use std::collections::VecDeque;
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

thread_local! {
    /// Bytes counted so far for the file [`tracking`] writes on this thread.
    static TRACKED: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Run `f`, which writes one planned file on this thread, counting the bytes the copy
/// loops report with [`copied`] as they go; returns `f`'s result and that count. Other
/// copies (backups, `--link-dest` copies) aren't tracked, so they never count.
pub fn tracking<T>(f: impl FnOnce() -> T) -> (T, u64) {
    TRACKED.with(|t| t.set(Some(0)));
    let r = f();
    (r, TRACKED.with(|t| t.take()).unwrap_or(0))
}

/// A copy loop wrote `n` more bytes (counted only inside [`tracking`]).
pub fn copied(n: u64) {
    TRACKED.with(|t| {
        if let Some(c) = t.get() {
            t.set(Some(c + n));
            DONE_BYTES.fetch_add(n, Relaxed);
        }
    });
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

/// Speed over a sliding window of recent samples, so a burst at the start (data the
/// other side already had, say) doesn't hide what the link does now.
#[derive(Default)]
struct Speed {
    samples: VecDeque<(Instant, u64)>,
}

impl Speed {
    const WINDOW: Duration = Duration::from_secs(5);

    /// Record `done` bytes at `now`; the bytes per second since the oldest sample kept.
    fn update(&mut self, now: Instant, done: u64) -> f64 {
        self.samples.push_back((now, done));
        // Keep one sample at least WINDOW old, once there is one.
        while self.samples.len() > 2 && now - self.samples[1].0 >= Self::WINDOW {
            self.samples.pop_front();
        }
        let (t0, b0) = self.samples[0];
        let secs = (now - t0).as_secs_f64();
        if secs > 0.0 { done.saturating_sub(b0) as f64 / secs } else { 0.0 }
    }
}

/// `h:mm:ss`, right-aligned like rsync's time column.
fn clock(secs: u64) -> String {
    format!("{:>4}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// `recent`: while running, the speed of the last few seconds, shown with the time left;
/// `None` for the final line, with the average speed and the total time. The scan is
/// over by then, so the file count is final (`to-chk`); before that it is still growing
/// (`ir-chk`, as rsync labels its incremental file list).
fn status(started: Instant, human: bool, recent: Option<f64>) -> String {
    let last = recent.is_none();
    let (pb, pf) = (PLANNED_BYTES.load(Relaxed), PLANNED_FILES.load(Relaxed));
    let (db, df) = (DONE_BYTES.load(Relaxed), DONE_FILES.load(Relaxed));
    let secs = started.elapsed().as_secs_f64();
    // Nothing planned yet reads 0%, not 100%, until the scan is over.
    let all_done = pf == df && (last || pf > 0);
    let pct = (db * 100).checked_div(pb).map_or(if all_done { 100 } else { 0 }, |p| p.min(100));
    let (rate, time) = match recent {
        Some(rate) if rate >= 1.0 => (rate, clock((pb.saturating_sub(db) as f64 / rate) as u64)),
        Some(_) => (0.0, format!("{:>10}", "--:--:--")),
        None => (if secs > 0.0 { db as f64 / secs } else { 0.0 }, clock(secs as u64)),
    };
    let bytes = if human { human_num(db) } else { grouped(db) };
    format!(
        "{bytes:>15} {pct:>3}% {:>10}B/s {time} (xfr#{df}, {}={}/{pf})",
        human_num(rate as u64),
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
                let mut speed = Speed::default();
                while !*stopped {
                    let recent = speed.update(Instant::now(), DONE_BYTES.load(Relaxed));
                    out.progress(status(started, human, Some(recent)).as_bytes(), false);
                    stopped = cvar.wait_timeout(stopped, Duration::from_millis(500)).unwrap().0;
                }
            })
            .ok();
        Reporter { stop, thread, started, human }
    }

    /// Stop redrawing and leave the final status on screen.
    pub fn finish(mut self, out: &Output) {
        self.halt();
        out.progress(status(self.started, self.human, None).as_bytes(), true);
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
    use super::{Speed, clock, copied, grouped, tracking};
    use std::time::{Duration, Instant};

    #[test]
    fn only_tracked_copies_count() {
        copied(5);
        let ((), n) = tracking(|| {
            copied(64 << 20);
            copied(36 << 20);
        });
        assert_eq!(n, 100 << 20);
        // The next tracked file starts from zero.
        assert_eq!(tracking(|| copied(1)).1, 1);
    }

    #[test]
    fn a_big_local_copy_counts_as_it_goes() {
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        std::fs::write(&src, vec![7u8; 3 << 20]).unwrap();
        // Through a buffer, as to a disk without clones.
        let (r, n) = tracking(|| crate::fsops::copy_new(&src, &dst, 3 << 20, false, 0o644));
        r.unwrap();
        assert_eq!(n, 3 << 20);
    }

    #[test]
    fn speed_follows_the_last_seconds() {
        let t = Instant::now();
        let at = |s: u64| t + Duration::from_secs(s);
        let mut speed = Speed::default();
        assert_eq!(speed.update(at(0), 0), 0.0);
        // 1 GB at once, then 10 MB a second: the start no longer counts after 5 s.
        assert_eq!(speed.update(at(1), 1_000_000_000), 1e9);
        for s in 2..=20 {
            speed.update(at(s), 1_000_000_000 + (s - 1) * 10_000_000);
        }
        assert_eq!(speed.update(at(21), 1_200_000_000), 10_000_000.0);
        // A stall reads 0.
        for s in 22..=27 {
            speed.update(at(s), 1_200_000_000);
        }
        assert_eq!(speed.update(at(28), 1_200_000_000), 0.0);
    }

    #[test]
    fn clock_format() {
        assert_eq!(clock(0), "   0:00:00");
        assert_eq!(clock(3 * 3600 + 25 * 60 + 7), "   3:25:07");
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(123456789), "123,456,789");
    }
}
