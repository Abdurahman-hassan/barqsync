//! Filesystem primitives: directory reading, metadata, temp files, copying and attributes.
//! Platform-specific fast paths live in `macos.rs` / `linux.rs`.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;

use crate::entry::{DirEnt, Kind, Meta};
use rayon::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex};

/// Directories with more entries than this get their `lstat`s fanned out across threads.
const PARALLEL_STAT_THRESHOLD: usize = 256;

pub fn cstr(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r == -1 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

#[allow(clippy::unnecessary_cast)] // field types differ between macOS and Linux
pub fn meta_from_stat(st: &libc::stat) -> Meta {
    let fmt = st.st_mode as u32 & libc::S_IFMT as u32;
    let kind = if fmt == libc::S_IFREG as u32 {
        Kind::File
    } else if fmt == libc::S_IFDIR as u32 {
        Kind::Dir
    } else if fmt == libc::S_IFLNK as u32 {
        Kind::Symlink
    } else if fmt == libc::S_IFIFO as u32 {
        Kind::Fifo
    } else if fmt == libc::S_IFCHR as u32 {
        Kind::CharDev
    } else if fmt == libc::S_IFBLK as u32 {
        Kind::BlockDev
    } else {
        Kind::Socket
    };
    Meta {
        kind,
        size: st.st_size as u64,
        mtime: st.st_mtime as i64,
        mtime_ns: st.st_mtime_nsec as u32,
        mode: st.st_mode as u32 & 0o7777,
        uid: st.st_uid,
        gid: st.st_gid,
        dev: st.st_dev as u64,
        ino: st.st_ino as u64,
        rdev: st.st_rdev as u64,
    }
}

/// An open directory handle used for `*at` syscalls (cheaper than resolving full paths).
pub struct DirHandle {
    fd: OwnedFd,
}

impl DirHandle {
    pub fn open(path: &Path) -> io::Result<DirHandle> {
        let c = cstr(path)?;
        let fd = cvt(unsafe {
            libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC)
        })?;
        Ok(DirHandle { fd: unsafe { OwnedFd::from_raw_fd(fd) } })
    }

    pub fn lstat(&self, name: &OsStr) -> io::Result<Meta> {
        let c = CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains NUL byte"))?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        cvt(unsafe {
            libc::fstatat(self.fd.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW)
        })?;
        Ok(meta_from_stat(&st))
    }
}

pub fn lstat(path: &Path) -> io::Result<Meta> {
    Ok(Meta::from_metadata(&fs::symlink_metadata(path)?))
}

pub fn stat(path: &Path) -> io::Result<Meta> {
    Ok(Meta::from_metadata(&fs::metadata(path)?))
}

/// Read a directory and `lstat` every child. Entries that vanish between `readdir`
/// and `lstat` are skipped, matching rsync's behaviour for files deleted mid-scan.
pub fn read_dir_meta(dir: &Path) -> io::Result<Vec<DirEnt>> {
    let names: Vec<OsString> =
        fs::read_dir(dir)?.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
    let handle = DirHandle::open(dir)?;
    let stat_one = |name: OsString| -> Option<io::Result<DirEnt>> {
        match handle.lstat(&name) {
            Ok(meta) => Some(Ok(DirEnt { name, meta })),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => Some(Err(e)),
        }
    };
    let res: io::Result<Vec<DirEnt>> = if names.len() > PARALLEL_STAT_THRESHOLD {
        names.into_par_iter().filter_map(stat_one).collect()
    } else {
        names.into_iter().filter_map(stat_one).collect()
    };
    let mut ents = res?;
    ents.sort_unstable_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    Ok(ents)
}

/// Read a directory's child names and types (from `d_type`, no per-entry `stat`).
pub fn read_dir_kinds(dir: &Path) -> io::Result<HashMap<OsString, Kind>> {
    let mut map = HashMap::new();
    for e in fs::read_dir(dir)? {
        let e = e?;
        let kind = match e.file_type() {
            Ok(ft) => Kind::from_file_type(ft),
            Err(_) => continue,
        };
        map.insert(e.file_name(), kind);
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Temp files and interrupt cleanup
// ---------------------------------------------------------------------------

pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static TEMPS: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

pub fn register_temp(p: &Path) {
    TEMPS.lock().unwrap().insert(p.to_path_buf());
}

pub fn unregister_temp(p: &Path) {
    TEMPS.lock().unwrap().remove(p);
}

static SIGNALS: AtomicU32 = AtomicU32::new(0);

/// Ctrl-C handler. The first signal only raises a flag: workers notice it between
/// write chunks and before every rename, delete their own unfinished file and stop,
/// so nothing is left half-written. A second signal forces an immediate exit.
pub fn on_interrupt(code: i32) {
    if SIGNALS.fetch_add(1, Ordering::SeqCst) == 0 {
        INTERRUPTED.store(true, Ordering::SeqCst);
        eprintln!("\nfastrsync: interrupted, finishing in-flight files (Ctrl-C again to force)");
    } else {
        cleanup_temps();
        std::process::exit(code);
    }
}

/// Remove every registered temp (or in-progress `--inplace`) file. Holding the lock
/// keeps workers from registering new ones while we sweep.
pub fn cleanup_temps() {
    let mut temps = TEMPS.lock().unwrap_or_else(|e| e.into_inner());
    for p in temps.drain() {
        let _ = fs::remove_file(p);
    }
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

/// Error out of a copy when an interrupt arrived, so the caller discards its file.
pub fn check_interrupt() -> io::Result<()> {
    if interrupted() { Err(io::Error::other("interrupted")) } else { Ok(()) }
}

thread_local! {
    static RNG: Cell<u64> = Cell::new(seed());
    static BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn seed() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let local = 0u8;
    let addr = &local as *const u8 as u64;
    (t ^ addr.rotate_left(32) ^ (std::process::id() as u64) << 16) | 1
}

fn next_rand() -> u64 {
    RNG.with(|r| {
        // xorshift64*
        let mut x = r.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        r.set(x);
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    })
}

const MAX_NAME: usize = 255;
const TEMP_SUFFIX_LEN: usize = 7; // ".XXXXXX"

/// `.<name>.XXXXXX` in the same directory, truncated to fit NAME_MAX. Works on raw
/// bytes so non-UTF-8 names never collapse onto a shared temp name.
pub fn temp_name(final_name: &OsStr) -> OsString {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut name = final_name.as_bytes();
    let budget = MAX_NAME - 1 - TEMP_SUFFIX_LEN;
    if name.len() > budget {
        let mut cut = budget;
        if let Ok(s) = std::str::from_utf8(name) {
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
        }
        name = &name[..cut];
    }
    let mut out = Vec::with_capacity(1 + name.len() + TEMP_SUFFIX_LEN);
    out.push(b'.');
    out.extend_from_slice(name);
    out.push(b'.');
    let mut r = next_rand();
    for _ in 0..6 {
        out.push(ALPHABET[(r % ALPHABET.len() as u64) as usize]);
        r /= ALPHABET.len() as u64;
    }
    OsString::from_vec(out)
}

use std::os::unix::ffi::OsStringExt;

/// Run `create` against fresh temp names in `dir` until one doesn't collide.
/// The temp path is registered for interrupt cleanup before creation.
pub fn with_temp<T>(
    dir: &Path,
    final_name: &OsStr,
    mut create: impl FnMut(&Path) -> io::Result<T>,
) -> io::Result<(PathBuf, T)> {
    for _ in 0..64 {
        let tmp = dir.join(temp_name(final_name));
        register_temp(&tmp);
        match create(&tmp) {
            Ok(v) => return Ok((tmp, v)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => unregister_temp(&tmp),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                unregister_temp(&tmp);
                return Err(e);
            }
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "could not create a unique temp file"))
}

/// Discard a temp file after a failure.
pub fn drop_temp(tmp: &Path) {
    let _ = fs::remove_file(tmp);
    unregister_temp(tmp);
}

// ---------------------------------------------------------------------------
// Copying
// ---------------------------------------------------------------------------

/// How the data ended up in the new file.
pub enum Copied {
    /// A copy-on-write clone: shares blocks with the source and inherits its mode and
    /// timestamps. There is no open handle, so attributes are applied by path.
    Clone,
    /// Data was written through this still-open handle (kernel copy or buffered), so
    /// attributes can be applied by descriptor without another path lookup.
    Written(File),
}

/// Copy `src` (expected to be `size` bytes) into a brand-new file at `tmp`, which must
/// not exist, created with permission bits `mode` (subject to the umask). With
/// `allow_fast` false, data always goes through a userspace buffer.
pub fn copy_new(src: &Path, tmp: &Path, size: u64, allow_fast: bool, mode: u32) -> io::Result<Copied> {
    let src_file = File::open(src)?;
    if !allow_fast {
        let dst = create_new(tmp, mode)?;
        copy_buffered(&src_file, &dst)?;
        return Ok(Copied::Written(dst));
    }
    platform::copy_new(&src_file, tmp, size, mode)
}

pub(crate) fn create_new(path: &Path, mode: u32) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new().write(true).create_new(true).mode(mode & 0o777).open(path)
}

/// Portable fallback: read/write through a reusable 1 MiB per-thread buffer.
pub(crate) fn copy_buffered(mut src: &File, mut dst: &File) -> io::Result<u64> {
    BUF.with(|b| {
        let mut buf = b.borrow_mut();
        if buf.len() < (1 << 20) {
            buf.resize(1 << 20, 0);
        }
        let mut total = 0u64;
        loop {
            check_interrupt()?;
            let n = match src.read(&mut buf) {
                Ok(0) => return Ok(total),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            dst.write_all(&buf[..n])?;
            total += n as u64;
        }
    })
}

/// Overwrite an existing file in place (for `--inplace`).
pub fn copy_into_existing(src: &Path, dst: &Path) -> io::Result<()> {
    let s = File::open(src)?;
    let d = fs::OpenOptions::new().write(true).truncate(true).open(dst)?;
    copy_buffered(&s, &d)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Attributes
// ---------------------------------------------------------------------------

pub fn chmod(path: &Path, mode: u32) -> io::Result<()> {
    let c = cstr(path)?;
    cvt(unsafe { libc::chmod(c.as_ptr(), mode as libc::mode_t) }).map(drop)
}

pub fn fchmod(f: &File, mode: u32) -> io::Result<()> {
    cvt(unsafe { libc::fchmod(f.as_raw_fd(), mode as libc::mode_t) }).map(drop)
}

pub fn fchown(f: &File, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }
    std::os::unix::fs::fchown(f, uid, gid)
}

pub fn set_fd_mtime(f: &File, sec: i64, nsec: u32) -> io::Result<()> {
    let omit = libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT as _ };
    let times = [omit, ts(sec, nsec)];
    cvt(unsafe { libc::futimens(f.as_raw_fd(), times.as_ptr()) }).map(drop)
}

pub fn lchown(path: &Path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }
    std::os::unix::fs::lchown(path, uid, gid)
}

/// Path-based `utimensat`: one syscall, and it never opens the file (opening a FIFO would block).
fn utimens(path: &Path, mtime: libc::timespec, flags: libc::c_int) -> io::Result<()> {
    let c = cstr(path)?;
    let omit = libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT as _ };
    let times = [omit, mtime];
    cvt(unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), flags) }).map(drop)
}

fn ts(sec: i64, nsec: u32) -> libc::timespec {
    libc::timespec { tv_sec: sec as libc::time_t, tv_nsec: nsec as _ }
}

pub fn set_mtime(path: &Path, sec: i64, nsec: u32) -> io::Result<()> {
    utimens(path, ts(sec, nsec), 0)
}

pub fn set_symlink_mtime(path: &Path, sec: i64, nsec: u32) -> io::Result<()> {
    utimens(path, ts(sec, nsec), libc::AT_SYMLINK_NOFOLLOW)
}

pub fn touch_now(path: &Path) -> io::Result<()> {
    utimens(path, libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW as _ }, 0)
}

pub fn fsync_path(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

pub fn mknod(path: &Path, kind: Kind, mode: u32, rdev: u64) -> io::Result<()> {
    let c = cstr(path)?;
    let perm = (mode & 0o7777) as libc::mode_t;
    let r = match kind {
        Kind::Fifo => unsafe { libc::mkfifo(c.as_ptr(), perm) },
        Kind::CharDev => unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR | perm, rdev as libc::dev_t) },
        Kind::BlockDev => unsafe { libc::mknod(c.as_ptr(), libc::S_IFBLK | perm, rdev as libc::dev_t) },
        _ => return Err(io::Error::new(io::ErrorKind::Unsupported, "cannot create this file type")),
    };
    cvt(r).map(drop)
}

pub fn current_umask() -> u32 {
    // Not thread-safe: call once at startup before spawning workers.
    unsafe {
        let m = libc::umask(0o022);
        libc::umask(m);
        m as u32
    }
}

#[allow(clippy::unnecessary_cast)]
pub fn my_groups() -> Vec<u32> {
    let mut groups = vec![0 as libc::gid_t; 1024];
    let n = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
    let mut out: Vec<u32> = if n > 0 { groups[..n as usize].iter().map(|&g| g as u32).collect() } else { vec![] };
    out.push(unsafe { libc::getegid() } as u32);
    out.sort_unstable();
    out.dedup();
    out
}

pub fn am_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_names_are_unique_and_bounded() {
        let a = temp_name(OsStr::new("file.txt"));
        let b = temp_name(OsStr::new("file.txt"));
        assert_ne!(a, b);
        assert!(a.as_bytes().starts_with(b".file.txt."));
        let long = "é".repeat(200); // 400 bytes
        let t = temp_name(OsStr::new(&long));
        assert!(t.len() <= MAX_NAME);
        assert!(std::str::from_utf8(t.as_bytes()).is_ok(), "must cut on a char boundary");
    }

    #[test]
    fn non_utf8_names_stay_distinct() {
        let a = OsStr::from_bytes(b"\xff\xfe-a");
        let b = OsStr::from_bytes(b"\xff\xfe-b");
        assert!(temp_name(a).as_bytes().starts_with(b".\xff\xfe-a."));
        assert!(temp_name(b).as_bytes().starts_with(b".\xff\xfe-b."));
    }
}
