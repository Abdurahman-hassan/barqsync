//! Filesystem primitives: directory reading, metadata, temp files, copying and attributes.
//! Platform-specific fast paths live in `macos.rs` / `linux.rs`.

pub mod cache;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub mod uring;
#[cfg(target_os = "macos")]
mod macos;
pub mod xattr;
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
    #[cfg(target_os = "macos")]
    let crtime = st.st_birthtime as i64;
    #[cfg(not(target_os = "macos"))]
    let crtime = 0;
    let (atime, crtime) = Meta::extra_times(kind, st.st_atime as i64, crtime);
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
        nlink: st.st_nlink as u64,
        atime,
        crtime,
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

    #[cfg(target_os = "macos")]
    pub(crate) fn raw_fd(&self) -> std::os::fd::RawFd {
        self.fd.as_raw_fd()
    }

    pub fn lstat(&self, name: &OsStr) -> io::Result<Meta> {
        let c = CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains NUL byte"))?;
        #[cfg(target_os = "linux")]
        {
            platform::statx_at(self.fd.as_raw_fd(), &c)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            cvt(unsafe {
                libc::fstatat(self.fd.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW)
            })?;
            Ok(meta_from_stat(&st))
        }
    }

    pub fn readlink(&self, name: &OsStr) -> io::Result<Vec<u8>> {
        let c = CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains NUL byte"))?;
        let mut buf = vec![0u8; 1024];
        loop {
            let n = unsafe {
                libc::readlinkat(self.fd.as_raw_fd(), c.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if (n as usize) < buf.len() {
                buf.truncate(n as usize);
                return Ok(buf);
            }
            buf.resize(buf.len() * 4, 0);
        }
    }
}

/// Symlink target of `path` as raw bytes.
pub fn read_link_bytes(path: &Path) -> io::Result<Vec<u8>> {
    Ok(fs::read_link(path)?.into_os_string().into_vec())
}

pub fn lstat(path: &Path) -> io::Result<Meta> {
    Ok(Meta::from_metadata(&fs::symlink_metadata(path)?))
}

pub fn stat(path: &Path) -> io::Result<Meta> {
    Ok(Meta::from_metadata(&fs::metadata(path)?))
}

/// Whether two entries are one file: the same name, a hard link, or another path to it.
pub fn same_file(a: &Meta, b: &Meta) -> bool {
    (a.dev, a.ino) == (b.dev, b.ino)
}

/// [`same_file`] for two open files.
pub fn same_open_file(a: &File, b: &File) -> io::Result<bool> {
    Ok(same_file(&Meta::from_metadata(&a.metadata()?), &Meta::from_metadata(&b.metadata()?)))
}

/// Tests: with `BARQSYNC_TEST_VANISH=NAME`, a source file named NAME is removed just before
/// it is opened to be sent, as if it was deleted after the scan; with
/// `BARQSYNC_TEST_SHRINK=NAME`, it is cut to half its length instead.
pub fn test_vanish(path: &Path) {
    static NAMES: std::sync::LazyLock<[Option<std::ffi::OsString>; 2]> = std::sync::LazyLock::new(|| {
        [std::env::var_os("BARQSYNC_TEST_VANISH"), std::env::var_os("BARQSYNC_TEST_SHRINK")]
    });
    let named = |i: usize| NAMES[i].as_deref().is_some_and(|n| path.file_name() == Some(n));
    if named(0) {
        let _ = fs::remove_file(path);
    }
    if named(1)
        && let Ok(f) = fs::OpenOptions::new().write(true).open(path)
        && let Ok(m) = f.metadata()
    {
        let _ = f.set_len(m.len() / 2);
    }
}

/// `--mkpath`: create the directories of `path` that don't exist yet, `path` included
/// (rsync's `make_path`). A dry run only counts them. Returns how many there were.
pub fn make_path(path: &Path, dry_run: bool) -> io::Result<usize> {
    let mut missing = Vec::new();
    let mut p = path;
    loop {
        match fs::metadata(p) {
            Ok(m) if m.is_dir() => break,
            Ok(_) => return Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing.push(p);
                match p.parent() {
                    Some(q) if !q.as_os_str().is_empty() => p = q,
                    _ => break,
                }
            }
            Err(e) => return Err(e),
        }
    }
    if !dry_run {
        for d in missing.iter().rev() {
            match fs::create_dir(d) {
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
                _ => {}
            }
        }
    }
    Ok(missing.len())
}

/// `path` as rsync names files in its messages: absolute, its directory resolved the way
/// rsync's working directory is.
pub fn full_path(path: &Path) -> PathBuf {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    match (std::fs::canonicalize(dir), path.file_name()) {
        (Ok(d), Some(name)) => d.join(name),
        _ => std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
    }
}

/// What a source file that is no longer the one its listing described is refused with.
pub const CHANGED_SINCE_LISTED: &str = "changed since it was listed";

/// Open the source file at `path`, listed as `listed`, to read its data. It must still be
/// that file: a regular file with the listed device and inode. Without `follow` it is
/// opened without following a symlink. A source swapped for a symlink, a FIFO or another
/// file between the scan and the read is refused, never read (rsync's CVE-2024-12747).
/// `follow` (`-L`): the listing followed a symlink to `listed`, so `path` may be one, but
/// it must still lead to that file.
pub fn open_listed(path: &Path, listed: &Meta, follow: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NONBLOCK: a FIFO put in its place would otherwise block the open until it has a
    // writer. It changes nothing for a regular file.
    let flags = libc::O_NONBLOCK | read_flags() | if follow { 0 } else { libc::O_NOFOLLOW };
    let changed = || io::Error::other(CHANGED_SINCE_LISTED);
    let f = fs::OpenOptions::new().read(true).custom_flags(flags).open(path).map_err(|e| match e.raw_os_error() {
        // O_NOFOLLOW met a symlink.
        Some(libc::ELOOP) if !follow => changed(),
        _ => e,
    })?;
    let now = Meta::from_metadata(&f.metadata()?);
    if now.kind != Kind::File || !same_file(&now, listed) {
        return Err(changed());
    }
    Ok(f)
}

static OPEN_NOATIME: AtomicBool = AtomicBool::new(false);

/// `--open-noatime` (`-UU`): files are read without updating their access time
/// (`O_NOATIME`, which Linux has; elsewhere, as with rsync, nothing changes). Like
/// rsync, a file this user may not open that way (not its owner) isn't read.
pub fn set_open_noatime(on: bool) {
    OPEN_NOATIME.store(on, Ordering::Relaxed);
}

/// The `open` flags [`set_open_noatime`] asks for, to read a file with.
pub fn read_flags() -> libc::c_int {
    #[cfg(target_os = "linux")]
    if OPEN_NOATIME.load(Ordering::Relaxed) {
        return libc::O_NOATIME;
    }
    0
}

/// Read a directory and `lstat` every child. Entries that vanish between `readdir`
/// and `lstat` are skipped, matching rsync's behaviour for files deleted mid-scan.
///
/// On macOS the names and attributes come from `getattrlistbulk` (many entries per
/// syscall) when the filesystem supports it.
pub fn read_dir_meta(dir: &Path) -> io::Result<Vec<DirEnt>> {
    #[cfg(target_os = "macos")]
    if !bulk_disabled() {
        let handle = DirHandle::open(dir)?;
        if let Some(mut ents) = platform::read_dir_bulk(&handle)? {
            ents.sort_unstable_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
            return Ok(ents);
        }
    }
    read_dir_stat(dir)
}

/// `BARQSYNC_NO_BULK=1` forces the per-entry `fstatat` path (for benchmarking).
#[cfg(target_os = "macos")]
fn bulk_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var_os("BARQSYNC_NO_BULK").is_some())
}

/// `readdir` + one `fstatat` per entry.
pub fn read_dir_stat(dir: &Path) -> io::Result<Vec<DirEnt>> {
    // An entry that can't be read fails the listing: a listing missing entries would
    // have their destination copies deleted.
    let names: Vec<OsString> = fs::read_dir(dir)?.map(|e| e.map(|e| e.file_name())).collect::<io::Result<_>>()?;
    let handle = DirHandle::open(dir)?;
    let stat_one = |name: OsString| -> Option<io::Result<DirEnt>> {
        let meta = match handle.lstat(&name) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => return Some(Err(e)),
        };
        let link = match meta.kind {
            Kind::Symlink => match handle.readlink(&name) {
                Ok(t) => Some(t),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
                Err(e) => return Some(Err(e)),
            },
            _ => None,
        };
        Some(Ok(DirEnt::new(name, meta, link)))
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

/// A channel nothing is sent on, disconnected by the first interrupt, so blocking waits
/// (a send to a peer that no longer reads) can select on it and give up;
/// [`interrupt_token`] is the same for tokio.
struct Wake {
    tx: Mutex<Option<crossbeam_channel::Sender<()>>>,
    rx: crossbeam_channel::Receiver<()>,
}

static WAKE: LazyLock<Wake> = LazyLock::new(|| {
    let (tx, rx) = crossbeam_channel::bounded(0);
    Wake { tx: Mutex::new(Some(tx)), rx }
});
static WAKE_TOKEN: LazyLock<tokio_util::sync::CancellationToken> = LazyLock::new(Default::default);

/// Stop the run: raise the flag the workers check and wake whatever waits on
/// [`interrupt_wake`] or [`interrupt_token`]. Ctrl-C does this, and so does the other
/// side of a remote transfer saying it was interrupted.
pub fn raise_interrupt() {
    INTERRUPTED.store(true, Ordering::SeqCst);
    WAKE.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
    WAKE_TOKEN.cancel();
}

/// Ready (disconnected) once the run is interrupted.
pub fn interrupt_wake() -> &'static crossbeam_channel::Receiver<()> {
    &WAKE.rx
}

/// Cancelled once the run is interrupted.
pub fn interrupt_token() -> &'static tokio_util::sync::CancellationToken {
    &WAKE_TOKEN
}

/// Run [`on_interrupt`] on SIGINT, SIGTERM and SIGHUP (`ctrlc`'s `termination` feature).
/// A SIGTERM or SIGHUP ignored when barqsync started (under `nohup`, or by a parent that
/// ignores it) stays ignored, as Unix tools leave it: whoever started us asked for the
/// transfer to survive it.
pub fn handle_interrupts(code: i32) {
    let ignored: Vec<i32> = [libc::SIGTERM, libc::SIGHUP]
        .into_iter()
        .filter(|&sig| unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(sig, std::ptr::null(), &mut old) == 0 && old.sa_sigaction == libc::SIG_IGN
        })
        .collect();
    let _ = ctrlc::set_handler(move || on_interrupt(code));
    for sig in ignored {
        unsafe { libc::signal(sig, libc::SIG_IGN) };
    }
}

/// Handler for Ctrl-C and the termination signals (SIGINT, SIGTERM, SIGHUP; see
/// [`handle_interrupts`]). The first signal only raises a flag: workers notice
/// it between write chunks and before every rename, delete their own unfinished file and
/// stop, so nothing is left half-written. A second signal forces an immediate exit.
pub fn on_interrupt(code: i32) {
    if SIGNALS.fetch_add(1, Ordering::SeqCst) == 0 {
        raise_interrupt();
        eprintln!("\nbarqsync: interrupted, finishing in-flight files (signal again to force)");
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
    temp_name_with(final_name, true)
}

/// [`temp_name`], without the leading `.` when `dot` is false (a `-T` directory's temp
/// files, as rsync names them there).
pub fn temp_name_with(final_name: &OsStr, dot: bool) -> OsString {
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
    if dot {
        out.push(b'.');
    }
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
    create: impl FnMut(&Path) -> io::Result<T>,
) -> io::Result<(PathBuf, T)> {
    with_temp_in(dir, final_name, true, create)
}

/// [`with_temp`], the names without their leading `.` when `dot` is false.
pub fn with_temp_in<T>(
    dir: &Path,
    final_name: &OsStr,
    dot: bool,
    mut create: impl FnMut(&Path) -> io::Result<T>,
) -> io::Result<(PathBuf, T)> {
    for _ in 0..64 {
        let tmp = dir.join(temp_name_with(final_name, dot));
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

/// Rename `from` to `to`. Across filesystems (a `-T` directory on another one) that
/// can't work, so, as rsync does, the data is copied instead ([`copy_into_place`]) and
/// `from` removed. Returns the copy, still open, when it was made: it then needs its
/// attributes again.
pub fn rename_or_copy(from: &Path, to: &Path) -> io::Result<Option<File>> {
    match fs::rename(from, to) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let out = copy_into_place(from, to)?;
            fs::remove_file(from)?;
            Ok(Some(out))
        }
        r => r.map(|()| None),
    }
}

/// Copy file `from` to `to`, on another filesystem: into a new file next to `to`, which
/// then takes its place, so what was at `to` (a symlink, or a file with other names) is
/// replaced, never written through, as rsync unlinks it before it copies. The copy is
/// returned open, so its attributes can be set whatever its mode allows this user.
fn copy_into_place(from: &Path, to: &Path) -> io::Result<File> {
    use std::os::unix::fs::PermissionsExt;
    let src = match File::open(from) {
        // A temp file of ours that got an existing file's mode, one its owner can't read
        // (0200): it goes once copied, so let it be read.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let mode = fs::symlink_metadata(from)?.permissions().mode();
            fs::set_permissions(from, fs::Permissions::from_mode(mode | 0o400))?;
            File::open(from)?
        }
        r => r?,
    };
    let md = src.metadata()?;
    let dir = to.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let (tmp, out) = with_temp(dir, to.file_name().unwrap_or_default(), |tmp| create_new(tmp, md.permissions().mode()))?;
    match copy_buffered(&src, &out, md.len()).and_then(|_| fs::rename(&tmp, to)) {
        Ok(()) => {
            unregister_temp(&tmp);
            Ok(out)
        }
        Err(e) => {
            drop_temp(&tmp);
            Err(e)
        }
    }
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
    copy_from(&File::open(src)?, tmp, size, allow_fast, mode)
}

/// [`copy_new`] from a file already open (a source opened with [`open_listed`]).
pub fn copy_from(src: &File, tmp: &Path, size: u64, allow_fast: bool, mode: u32) -> io::Result<Copied> {
    if !allow_fast {
        let dst = create_new(tmp, mode)?;
        copy_buffered(src, &dst, size)?;
        return Ok(Copied::Written(dst));
    }
    platform::copy_new(src, tmp, size, mode)
}

/// Create `tmp` as a copy-on-write clone of `basis` when the filesystem supports it
/// (APFS, btrfs, XFS). `Ok(false)`: not supported, nothing was created.
pub fn clone_into_new(basis: &File, tmp: &Path) -> io::Result<bool> {
    platform::clone_into_new(basis, tmp)
}

pub(crate) fn create_new(path: &Path, mode: u32) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new().write(true).create_new(true).mode(mode & 0o777).open(path)
}

/// Portable fallback: read/write through a reusable 1 MiB per-thread buffer.
/// `size` is the expected length; big files go through [`copy_uncached`] instead.
/// Under a local `--bwlimit` (see [`crate::bwlimit::paced`]) the copy goes in paced pieces.
pub(crate) fn copy_buffered(mut src: &File, mut dst: &File, size: u64) -> io::Result<u64> {
    let pace = crate::bwlimit::pacing();
    if cache::wanted(size) && pace.is_none() {
        return copy_uncached(src, dst, size);
    }
    BUF.with(|b| {
        let mut buf = b.borrow_mut();
        if buf.len() < (1 << 20) {
            buf.resize(1 << 20, 0);
        }
        let want = pace.map_or(buf.len(), |rate| crate::bwlimit::piece(rate).min(buf.len()));
        let mut total = 0u64;
        loop {
            check_interrupt()?;
            let n = match src.read(&mut buf[..want]) {
                Ok(0) => return Ok(total),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            crate::bwlimit::copying(n as u64);
            dst.write_all(&buf[..n])?;
            total += n as u64;
            crate::progress::copied(n as u64);
        }
    })
}

/// Copy a big file with page-cache hints. Uncached I/O on macOS is synchronous (no
/// read-ahead, no write-behind), so the next chunk is read while the current one is
/// written. Buffers this large come page-aligned from the allocator, which uncached
/// I/O needs to bypass the cache.
fn copy_uncached(src: &File, dst: &File, size: u64) -> io::Result<u64> {
    const CHUNK: usize = 8 << 20;
    let mut reader = cache::Reader::new(src, size);
    let mut writer = cache::Writer::new(dst, size);
    let (mut cur, mut next) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    let mut n = read_full(src, &mut cur)?;
    let (mut read, mut written) = (n as u64, 0u64);
    while n > 0 {
        check_interrupt()?;
        reader.read_to(src, read);
        let write = || {
            let mut d = dst;
            d.write_all(&cur[..n])
        };
        let (w, r) = rayon::join(write, || read_full(src, &mut next));
        w?;
        written += n as u64;
        crate::progress::copied(n as u64);
        writer.written_to(dst, written);
        n = r?;
        read += n as u64;
        std::mem::swap(&mut cur, &mut next);
    }
    Ok(written)
}

/// `--append`: copy `src` from offset `from` to its end into `dst` at the same offsets,
/// paced by a local `--bwlimit`. Returns where the data ends.
pub fn copy_tail(src: &File, dst: &File, from: u64) -> io::Result<u64> {
    use std::os::unix::fs::FileExt;
    let pace = crate::bwlimit::pacing();
    BUF.with(|b| {
        let mut buf = b.borrow_mut();
        if buf.len() < (1 << 20) {
            buf.resize(1 << 20, 0);
        }
        let want = pace.map_or(buf.len(), |rate| crate::bwlimit::piece(rate).min(buf.len()));
        let mut off = from;
        loop {
            check_interrupt()?;
            let n = match src.read_at(&mut buf[..want], off) {
                Ok(0) => return Ok(off),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            crate::bwlimit::copying(n as u64);
            dst.write_all_at(&buf[..n], off)?;
            off += n as u64;
            crate::progress::copied(n as u64);
        }
    })
}

/// Fill `buf` from `f` unless the file ends first; returns the bytes read.
fn read_full(mut f: &File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(k) => filled += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Overwrite an existing file in place with `src`, expected to be `size` bytes (for
/// `--inplace`), then cut it to the length copied. When both are the same file (`f f`,
/// or hard links), nothing is written: it already holds the data, and writing into it
/// would destroy the source.
pub fn copy_into_existing(src: &File, dst: &Path, size: u64) -> io::Result<()> {
    let d = fs::OpenOptions::new().write(true).open(dst)?;
    if same_open_file(src, &d)? {
        return Ok(());
    }
    let n = copy_buffered(src, &d, size)?;
    d.set_len(n)?;
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
    cvt(unsafe { libc::futimens(f.as_raw_fd(), [omit(), ts(sec, nsec)].as_ptr()) }).map(drop)
}

pub fn set_fd_mtime_now(f: &File) -> io::Result<()> {
    let now = libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW as _ };
    cvt(unsafe { libc::futimens(f.as_raw_fd(), [omit(), now].as_ptr()) }).map(drop)
}

pub fn lchown(path: &Path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }
    std::os::unix::fs::lchown(path, uid, gid)
}

/// Path-based `utimensat` of `[atime, mtime]`: one syscall, and it never opens the file
/// (opening a FIFO would block).
fn utimens(path: &Path, times: [libc::timespec; 2], flags: libc::c_int) -> io::Result<()> {
    let c = cstr(path)?;
    cvt(unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), flags) }).map(drop)
}

fn ts(sec: i64, nsec: u32) -> libc::timespec {
    libc::timespec { tv_sec: sec as libc::time_t, tv_nsec: nsec as _ }
}

/// A time `utimensat` leaves as it is.
fn omit() -> libc::timespec {
    libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT as _ }
}

pub fn set_mtime(path: &Path, sec: i64, nsec: u32) -> io::Result<()> {
    utimens(path, [omit(), ts(sec, nsec)], 0)
}

pub fn set_symlink_mtime(path: &Path, sec: i64, nsec: u32) -> io::Result<()> {
    utimens(path, [omit(), ts(sec, nsec)], libc::AT_SYMLINK_NOFOLLOW)
}

pub fn touch_now(path: &Path) -> io::Result<()> {
    utimens(path, [omit(), libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW as _ }], 0)
}

/// `-U`: the access time, in whole seconds as rsync sets it; `symlink`: of the link
/// itself.
pub fn set_atime(path: &Path, sec: i64, symlink: bool) -> io::Result<()> {
    utimens(path, [ts(sec, 0), omit()], if symlink { libc::AT_SYMLINK_NOFOLLOW } else { 0 })
}

/// [`set_atime`] through a descriptor.
pub fn set_fd_atime(f: &File, sec: i64) -> io::Result<()> {
    cvt(unsafe { libc::futimens(f.as_raw_fd(), [ts(sec, 0), omit()].as_ptr()) }).map(drop)
}

/// `-N`: the creation time of `path` itself (not of a symlink's target), in seconds.
/// Only macOS can set one, as rsync's `--crtimes` needs.
pub fn set_crtime(path: &Path, sec: i64) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    return platform::set_crtime(path, sec);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (path, sec);
        Err(io::ErrorKind::Unsupported.into())
    }
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

    /// The bulk / statx scanner must agree field-for-field with plain `fstatat`.
    #[test]
    fn fast_scanner_matches_fstatat() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        fs::write(d.join("file"), b"hello").unwrap();
        fs::write(d.join("big"), vec![1u8; 300_000]).unwrap();
        fs::create_dir(d.join("sub")).unwrap();
        std::os::unix::fs::symlink("file", d.join("link")).unwrap();
        fs::hard_link(d.join("file"), d.join("hard")).unwrap();
        let c = cstr(&d.join("fifo")).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o640) }, 0);
        let c = cstr(&d.join("file")).unwrap();
        unsafe { libc::chmod(c.as_ptr(), 0o4751) };
        set_mtime(&d.join("big"), 1_600_000_123, 456_789_000).unwrap();
        for i in 0..2000 {
            fs::write(d.join(format!("many-{i:04}")), i.to_string()).unwrap();
        }
        let fast = read_dir_meta(d).unwrap();
        let plain: Vec<DirEnt> = {
            let mut v: Vec<DirEnt> = fs::read_dir(d)
                .unwrap()
                .map(|e| {
                    let name = e.unwrap().file_name();
                    let p = d.join(&name);
                    let meta = lstat(&p).unwrap();
                    let link = (meta.kind == Kind::Symlink).then(|| read_link_bytes(&p).unwrap());
                    DirEnt::new(name, meta, link)
                })
                .collect();
            v.sort_unstable_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
            v
        };
        assert_eq!(fast.len(), plain.len());
        for (a, b) in fast.iter().zip(&plain) {
            assert_eq!((&a.name, &a.meta, &a.link), (&b.name, &b.meta, &b.link));
        }
    }

    /// A copy across filesystems replaces what is at the destination: a symlink's target
    /// and a hard link's other names are left alone, a read-only copy still takes its
    /// attributes through the descriptor returned, and a file its owner can't read (an
    /// existing file's mode kept) is still copied.
    #[test]
    fn copies_into_place_replace_links() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        fs::write(d.join("outside"), "keep").unwrap();
        fs::write(d.join("new"), "new data").unwrap();
        fs::set_permissions(d.join("new"), fs::Permissions::from_mode(0o444)).unwrap();
        std::os::unix::fs::symlink(d.join("outside"), d.join("link")).unwrap();
        fs::write(d.join("old"), "old").unwrap();
        fs::hard_link(d.join("old"), d.join("backup")).unwrap();
        for to in ["link", "old", "fresh", "unreadable"] {
            if to == "unreadable" {
                fs::set_permissions(d.join("new"), fs::Permissions::from_mode(0o200)).unwrap();
            }
            let f = copy_into_place(&d.join("new"), &d.join(to)).unwrap();
            fchmod(&f, 0o400).unwrap();
            set_fd_mtime(&f, 1_600_000_000, 0).unwrap();
            let m = fs::symlink_metadata(d.join(to)).unwrap();
            assert!(m.is_file() && m.permissions().mode() & 0o777 == 0o400, "{to}");
            assert_eq!(m.mtime(), 1_600_000_000, "{to}");
            assert_eq!(fs::read(d.join(to)).unwrap(), b"new data", "{to}");
        }
        assert_eq!(fs::read(d.join("outside")).unwrap(), b"keep");
        assert_eq!(fs::read(d.join("backup")).unwrap(), b"old");
        let left: Vec<_> = fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name()).filter(|n| n.as_bytes().starts_with(b".")).collect();
        assert!(left.is_empty(), "temp files left: {left:?}");
    }

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
    fn open_listed_refuses_a_swapped_source() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        fs::write(d.join("f"), b"the source file").unwrap();
        let listed = lstat(&d.join("f")).unwrap();
        // Hold the original file open for the whole test. Its inode number then stays
        // allocated after the name is removed, so no replacement below can be handed the
        // same inode -- which on an inode-recycling filesystem (ext4) would otherwise make
        // a swapped-in regular file look like the one that was listed and fail the test.
        let _original = fs::File::open(d.join("f")).unwrap();

        // The listed file itself: read normally.
        let mut s = String::new();
        open_listed(&d.join("f"), &listed, false).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "the source file");

        // Swapped for a symlink to a secret: refused, and the secret is never opened.
        fs::write(d.join("secret"), b"SECRET").unwrap();
        let secret = lstat(&d.join("secret")).unwrap();
        fs::remove_file(d.join("f")).unwrap();
        std::os::unix::fs::symlink(d.join("secret"), d.join("f")).unwrap();
        let e = open_listed(&d.join("f"), &listed, false).unwrap_err();
        assert_eq!(e.to_string(), CHANGED_SINCE_LISTED);
        // Even with -L (follow) it must still be the file that was listed, not the secret.
        assert_eq!(open_listed(&d.join("f"), &listed, true).unwrap_err().to_string(), CHANGED_SINCE_LISTED);
        // Following the symlink to the secret's own identity is fine (that is what -L listed).
        let mut s = String::new();
        open_listed(&d.join("f"), &secret, true).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "SECRET");

        // Swapped for a different regular file (new inode): refused.
        fs::remove_file(d.join("f")).unwrap();
        fs::write(d.join("f"), b"the source file").unwrap();
        assert_eq!(open_listed(&d.join("f"), &listed, false).unwrap_err().to_string(), CHANGED_SINCE_LISTED);

        // Swapped for a FIFO: refused without blocking on a reader.
        fs::remove_file(d.join("f")).unwrap();
        mknod(&d.join("f"), Kind::Fifo, 0o644, 0).unwrap();
        assert_eq!(open_listed(&d.join("f"), &listed, false).unwrap_err().to_string(), CHANGED_SINCE_LISTED);
    }

    #[test]
    fn non_utf8_names_stay_distinct() {
        let a = OsStr::from_bytes(b"\xff\xfe-a");
        let b = OsStr::from_bytes(b"\xff\xfe-b");
        assert!(temp_name(a).as_bytes().starts_with(b".\xff\xfe-a."));
        assert!(temp_name(b).as_bytes().starts_with(b".\xff\xfe-b."));
    }
}
