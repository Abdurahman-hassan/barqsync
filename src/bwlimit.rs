//! `--bwlimit=RATE`: at most RATE bytes per second, paced like rsync's
//! `sleep_for_bwlimit`. There is one budget per process: in a remote transfer every
//! connection and data stream takes what it writes from it, in a local copy every file
//! written does (rsync's local copies go through a pipe that the limit applies to).
//! Writes go out in pieces of an eighth of a second's worth, and a writer waits only
//! once the writes so far are more than [`SLACK`] ahead of the rate.

use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

thread_local! {
    /// This thread is copying file data for a local transfer (see [`paced`]).
    static PACED: Cell<bool> = const { Cell::new(false) };
}

/// Bytes per second on the connections (0: no limit).
static CONNECTIONS: AtomicU64 = AtomicU64::new(0);
/// Bytes per second of file data written by a local copy (0: no limit).
static COPIES: AtomicU64 = AtomicU64::new(0);
/// When everything written so far is paid for at the rate.
static DUE: Mutex<Option<Instant>> = Mutex::new(None);

/// How far writes may run ahead of the rate before one waits (rsync's tenth of a second).
const SLACK: Duration = Duration::from_millis(100);
/// The longest single wait, so an interrupt is noticed.
pub(crate) const NAP: Duration = Duration::from_millis(50);

/// Limit what this process writes to its connections (a remote transfer).
pub fn limit_connections(bytes_per_sec: u64) {
    CONNECTIONS.store(bytes_per_sec, Relaxed);
}

/// Limit the file data this process writes (a local copy).
pub fn limit_copies(bytes_per_sec: u64) {
    COPIES.store(bytes_per_sec, Relaxed);
}

/// The connection limit, if any.
pub fn connections() -> Option<u64> {
    Some(CONNECTIONS.load(Relaxed)).filter(|&r| r > 0)
}

/// The local copy limit, if any.
pub fn copies() -> Option<u64> {
    Some(COPIES.load(Relaxed)).filter(|&r| r > 0)
}

/// The most to write at once at `rate`: an eighth of a second's worth, and at least 512
/// bytes (rsync's `bwlimit_writemax`).
pub fn piece(rate: u64) -> usize {
    (rate / 8).max(512) as usize
}

/// Count `n` bytes about to be written at `rate`: how long to wait before writing them,
/// if the writes so far are ahead of the rate.
pub fn reserve(rate: u64, n: usize) -> Option<Duration> {
    let mut due = DUE.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let from = due.filter(|&d| d > now).unwrap_or(now);
    let until = from + Duration::from_secs_f64(n as f64 / rate as f64);
    *due = Some(until);
    until.saturating_duration_since(now).checked_sub(SLACK).filter(|w| !w.is_zero())
}

/// Wait out `wait` in short naps, giving up early on an interrupt.
pub fn pause(wait: Duration) {
    let end = Instant::now() + wait;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() || crate::fsops::interrupted() {
            return;
        }
        std::thread::sleep(left.min(NAP));
    }
}

/// Run `f`, which writes a file's data for a local transfer: with a local `--bwlimit`,
/// the copies it makes are paced (see [`pacing`]). Other local copies (backups,
/// `--copy-dest`) are not: rsync makes those on the receiving side, not through the
/// connection the limit applies to.
pub fn paced<T>(f: impl FnOnce() -> T) -> T {
    if copies().is_none() {
        return f();
    }
    let before = PACED.replace(true);
    let r = f();
    PACED.set(before);
    r
}

/// The local copy limit, when the calling thread copies file data for a transfer.
pub fn pacing() -> Option<u64> {
    copies().filter(|_| PACED.get())
}

/// Under [`pacing`], a copy is about to write `n` bytes: wait while the copies so far
/// are ahead of the limit. Large amounts are paid for a piece at a time.
pub fn copying(n: u64) {
    let Some(rate) = pacing() else { return };
    let mut left = n;
    while left > 0 && !crate::fsops::interrupted() {
        let step = left.min(piece(rate) as u64);
        if let Some(w) = reserve(rate, step as usize) {
            pause(w);
        }
        left -= step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_paced_to_the_rate() {
        // 64 KiB/s: a quarter second's worth goes in one burst plus slack, the rest waits.
        let rate = 64 << 10;
        assert_eq!(piece(rate), 8 << 10);
        assert_eq!(piece(1024), 512);
        let start = Instant::now();
        let mut waited = Duration::ZERO;
        for _ in 0..8 {
            if let Some(w) = reserve(rate, piece(rate)) {
                waited = waited.max(w);
                pause(w);
            }
        }
        // 64 KiB at 64 KiB/s: about a second, less the slack.
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(700) && took < Duration::from_secs(3), "{took:?}");
        assert!(waited <= Duration::from_millis(200), "one piece never waits long: {waited:?}");
    }
}
