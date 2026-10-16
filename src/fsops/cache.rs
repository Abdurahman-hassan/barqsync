//! Page-cache hints for big files, so syncing a large file doesn't push other cached
//! data (and the memory of running applications) out of RAM.
//!
//! - macOS: `F_NOCACHE` on the descriptor. Pages already in memory are still used;
//!   everything else moves between the disk and our buffers without being cached.
//! - Linux: `POSIX_FADV_SEQUENTIAL` for reads and `POSIX_FADV_DONTNEED` on each range
//!   once it has been read. Writes get a bounded write-behind: each window starts its
//!   writeback when it is full, and the window before it is waited for and dropped,
//!   so at most two windows per file stay in the page cache.
//!
//! Only files of at least [`MIN_SIZE`] get hints: small files are cheap to cache and
//! often read again soon. Hints never change what is written, only what stays cached.

use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};

/// Files smaller than this are left to the page cache.
pub const MIN_SIZE: u64 = 8 << 20;

/// Linux: unit of dropping and write-behind for sequential I/O.
const WINDOW: u64 = 8 << 20;

static ENABLED: AtomicBool = AtomicBool::new(true);

/// Turn the hints on or off for this process (`--no-drop-cache`).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether a file of `size` bytes gets hints.
pub fn wanted(size: u64) -> bool {
    size >= MIN_SIZE && ENABLED.load(Ordering::Relaxed)
}

/// `f` (of `size` bytes) is about to be read once, in any order. Returns whether hints
/// are in effect; if so, pass each range read to [`drop_read`].
pub fn read_once(f: &File, size: u64) -> bool {
    if !wanted(size) {
        return false;
    }
    #[cfg(target_os = "macos")]
    nocache(f);
    #[cfg(target_os = "linux")]
    fadvise(f, 0, 0, libc::POSIX_FADV_SEQUENTIAL);
    true
}

/// A range of a [`read_once`] file has been read and is not needed again soon.
pub fn drop_read(f: &File, off: u64, len: u64) {
    #[cfg(target_os = "linux")]
    fadvise(f, off, len, libc::POSIX_FADV_DONTNEED);
    #[cfg(not(target_os = "linux"))]
    let _ = (f, off, len);
}

/// Hints for a big file read front to back.
pub struct Reader {
    on: bool,
    dropped: u64,
}

impl Reader {
    pub fn new(f: &File, size: u64) -> Reader {
        Reader { on: read_once(f, size), dropped: 0 }
    }

    /// Everything before `end` has been read.
    pub fn read_to(&mut self, f: &File, end: u64) {
        if self.on && end >= self.dropped + WINDOW {
            drop_read(f, self.dropped, end - self.dropped);
            self.dropped = end;
        }
    }
}

/// Hints for a big file written front to back (gaps filled later are fine: they just
/// stay cached like any other write).
pub struct Writer {
    on: bool,
    /// Writeback has been started for everything before this offset.
    queued: u64,
    /// The window whose writeback was started last; it's dropped one window later.
    pending: Option<(u64, u64)>,
}

impl Writer {
    pub fn new(f: &File, size: u64) -> Writer {
        let on = wanted(size);
        #[cfg(target_os = "macos")]
        if on {
            nocache(f);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = f;
        Writer { on, queued: 0, pending: None }
    }

    /// Everything before `end` has been written.
    pub fn written_to(&mut self, f: &File, end: u64) {
        if !self.on || end < self.queued + WINDOW {
            return;
        }
        start_writeback(f, self.queued, end - self.queued);
        if let Some((off, len)) = self.pending {
            drop_written(f, off, len);
        }
        self.pending = Some((self.queued, end - self.queued));
        self.queued = end;
    }
}

#[cfg(target_os = "macos")]
fn nocache(f: &File) {
    use std::os::fd::AsRawFd;
    unsafe { libc::fcntl(f.as_raw_fd(), libc::F_NOCACHE, 1) };
}

#[cfg(target_os = "linux")]
fn fadvise(f: &File, off: u64, len: u64, advice: libc::c_int) {
    use std::os::fd::AsRawFd;
    unsafe { libc::posix_fadvise(f.as_raw_fd(), off as libc::off_t, len as libc::off_t, advice) };
}

/// Linux: start writing a range back to disk without waiting for it.
fn start_writeback(f: &File, off: u64, len: u64) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        unsafe { libc::sync_file_range(f.as_raw_fd(), off as i64, len as i64, libc::SYNC_FILE_RANGE_WRITE) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (f, off, len);
}

/// Linux: wait until a range is on disk, then drop it from the page cache (dirty
/// pages can't be dropped, so the wait has to come first).
fn drop_written(f: &File, off: u64, len: u64) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let flags = libc::SYNC_FILE_RANGE_WAIT_BEFORE | libc::SYNC_FILE_RANGE_WRITE | libc::SYNC_FILE_RANGE_WAIT_AFTER;
        unsafe { libc::sync_file_range(f.as_raw_fd(), off as i64, len as i64, flags) };
        fadvise(f, off, len, libc::POSIX_FADV_DONTNEED);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (f, off, len);
}
