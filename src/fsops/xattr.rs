//! Extended-attribute syscalls on a path, never following a final symlink (macOS
//! `XATTR_NOFOLLOW`, Linux `l*xattr`). Names are raw bytes without the trailing NUL.

use super::cstr;
use std::ffi::CString;
use std::io;
use std::path::Path;

fn cname(name: &[u8]) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "xattr name contains NUL byte"))
}

/// The attribute does not exist (`ENOATTR` on macOS, `ENODATA` on Linux).
pub fn is_missing(e: &io::Error) -> bool {
    #[cfg(target_os = "macos")]
    let missing = libc::ENOATTR;
    #[cfg(target_os = "linux")]
    let missing = libc::ENODATA;
    e.raw_os_error() == Some(missing)
}

/// The filesystem has no extended attributes at all.
pub fn is_unsupported(e: &io::Error) -> bool {
    // The same number on Linux, two on macOS.
    let code = e.raw_os_error();
    code == Some(libc::ENOTSUP) || code == Some(libc::EOPNOTSUPP)
}

#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::CStr;
    const NOFOLLOW: libc::c_int = libc::XATTR_NOFOLLOW;

    pub unsafe fn list(p: &CStr, buf: *mut libc::c_char, len: usize) -> isize {
        unsafe { libc::listxattr(p.as_ptr(), buf, len, NOFOLLOW) }
    }
    pub unsafe fn get(p: &CStr, n: &CStr, buf: *mut libc::c_void, len: usize, pos: u32) -> isize {
        unsafe { libc::getxattr(p.as_ptr(), n.as_ptr(), buf, len, pos, NOFOLLOW) }
    }
    pub unsafe fn set(p: &CStr, n: &CStr, buf: *const libc::c_void, len: usize) -> libc::c_int {
        unsafe { libc::setxattr(p.as_ptr(), n.as_ptr(), buf, len, 0, NOFOLLOW) }
    }
    pub unsafe fn remove(p: &CStr, n: &CStr) -> libc::c_int {
        unsafe { libc::removexattr(p.as_ptr(), n.as_ptr(), NOFOLLOW) }
    }
}

#[cfg(target_os = "linux")]
mod sys {
    use std::ffi::CStr;

    pub unsafe fn list(p: &CStr, buf: *mut libc::c_char, len: usize) -> isize {
        unsafe { libc::llistxattr(p.as_ptr(), buf, len) }
    }
    pub unsafe fn get(p: &CStr, n: &CStr, buf: *mut libc::c_void, len: usize, _pos: u32) -> isize {
        unsafe { libc::lgetxattr(p.as_ptr(), n.as_ptr(), buf, len) }
    }
    pub unsafe fn set(p: &CStr, n: &CStr, buf: *const libc::c_void, len: usize) -> libc::c_int {
        unsafe { libc::lsetxattr(p.as_ptr(), n.as_ptr(), buf, len, 0) }
    }
    pub unsafe fn remove(p: &CStr, n: &CStr) -> libc::c_int {
        unsafe { libc::lremovexattr(p.as_ptr(), n.as_ptr()) }
    }
}

thread_local! {
    static NAMES: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    static VALUE: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Values up to this size are read in one call (see [`get`]).
const VALUE_BUF: usize = 16 << 10;

/// How many threads may be in extended-attribute calls at once (0: no limit).
///
/// On APFS each call costs ~10 µs of kernel time and they contend: a no-op `-X` sync of
/// 500,000 files took 20.5 s with all 20 threads in them, 12.8 s with 3 (13.3 s with 4,
/// 14.6 s with 2, 16.7 s with 8). ACL reads go through it too. Linux doesn't need it.
/// `BARQSYNC_XATTR_LANES=N` overrides (benchmarking aid).
fn lanes() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("BARQSYNC_XATTR_LANES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(if cfg!(target_os = "macos") { DEFAULT_LANES } else { 0 })
    })
}

const DEFAULT_LANES: usize = 3;

static BUSY: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
static FREED: std::sync::Condvar = std::sync::Condvar::new();

/// Held while making one entry's extended-attribute calls (see [`lanes`]).
pub struct Lane(bool);

pub fn lane() -> Lane {
    let n = lanes();
    if n == 0 {
        return Lane(false);
    }
    let mut busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
    while *busy >= n {
        busy = FREED.wait(busy).unwrap_or_else(|e| e.into_inner());
    }
    *busy += 1;
    Lane(true)
}

impl Drop for Lane {
    fn drop(&mut self) {
        if self.0 {
            *BUSY.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
            FREED.notify_one();
        }
    }
}

/// Names of `path`'s extended attributes. A filesystem without them has none.
pub fn names(path: &Path) -> io::Result<Vec<Vec<u8>>> {
    let c = cstr(path)?;
    NAMES.with(|b| {
        let mut buf = b.borrow_mut();
        if buf.len() < 1024 {
            buf.resize(1024, 0);
        }
        loop {
            let n = unsafe { sys::list(&c, buf.as_mut_ptr().cast(), buf.len()) };
            if n >= 0 {
                let list = &buf[..n as usize];
                return Ok(list.split(|&b| b == 0).filter(|n| !n.is_empty()).map(<[u8]>::to_vec).collect());
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ERANGE) => {
                    // Ask for the size, then retry (the list may grow in between).
                    let need = unsafe { sys::list(&c, std::ptr::null_mut(), 0) };
                    if need < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let len = (need as usize).max(buf.len() * 2);
                    buf.resize(len, 0);
                }
                _ if is_unsupported(&e) => return Ok(Vec::new()),
                _ => return Err(e),
            }
        }
    })
}

/// The value of attribute `name`, or `None` if it does not exist (any more).
pub fn get(path: &Path, name: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let (c, n) = (cstr(path)?, cname(name)?);
    // Most values are short: one call into a reusable buffer (each of these calls is ~10
    // µs of kernel time on APFS, so asking for the size first would double the cost).
    let first = VALUE.with(|b| {
        let mut buf = b.borrow_mut();
        if buf.len() < VALUE_BUF {
            buf.resize(VALUE_BUF, 0);
        }
        let r = unsafe { sys::get(&c, &n, buf.as_mut_ptr().cast(), buf.len(), 0) };
        // A full buffer may be cut short: macOS hands out a resource fork in pieces
        // rather than failing with ERANGE. Read those the sized way below.
        if r >= 0 && (r as usize) < buf.len() {
            return Ok(Some(buf[..r as usize].to_vec()));
        }
        Err(if r >= 0 { io::Error::from_raw_os_error(libc::ERANGE) } else { io::Error::last_os_error() })
    });
    match first {
        Ok(v) => return Ok(v),
        Err(e) if is_missing(&e) => return Ok(None),
        Err(e) if e.raw_os_error() != Some(libc::ERANGE) => return Err(e),
        Err(_) => {}
    }
    loop {
        let size = unsafe { sys::get(&c, &n, std::ptr::null_mut(), 0, 0) };
        if size < 0 {
            let e = io::Error::last_os_error();
            return if is_missing(&e) { Ok(None) } else { Err(e) };
        }
        let mut buf = vec![0u8; size as usize];
        let mut got = 0usize;
        // macOS returns at most 64 MiB of a resource fork per call: read on from the
        // position reached (other attributes come back whole).
        loop {
            let r = unsafe { sys::get(&c, &n, buf[got..].as_mut_ptr().cast(), buf.len() - got, got as u32) };
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::ERANGE) {
                    break; // grew since it was sized: start over
                }
                return if is_missing(&e) { Ok(None) } else { Err(e) };
            }
            got += r as usize;
            if r == 0 || got == buf.len() || cfg!(not(target_os = "macos")) {
                buf.truncate(got);
                return Ok(Some(buf));
            }
        }
    }
}

pub fn set(path: &Path, name: &[u8], value: &[u8]) -> io::Result<()> {
    let (c, n) = (cstr(path)?, cname(name)?);
    if unsafe { sys::set(&c, &n, value.as_ptr().cast(), value.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Remove attribute `name`; one that is already gone is fine.
pub fn remove(path: &Path, name: &[u8]) -> io::Result<()> {
    let (c, n) = (cstr(path)?, cname(name)?);
    if unsafe { sys::remove(&c, &n) } != 0 {
        let e = io::Error::last_os_error();
        if !is_missing(&e) {
            return Err(e);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_list_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f");
        std::fs::write(&p, b"x").unwrap();
        let name: &[u8] = b"user.barqsync.test";
        // Linux filesystems keep values within a block; macOS takes any size.
        let big: Vec<u8> = (0..if cfg!(target_os = "macos") { 100_000u32 } else { 2000 }).map(|i| i as u8).collect();
        match set(&p, name, &big) {
            Err(e) if is_unsupported(&e) => return, // e.g. tmpfs without user xattrs
            r => r.unwrap(),
        }
        assert_eq!(get(&p, name).unwrap().as_deref(), Some(&big[..]));
        assert!(names(&p).unwrap().iter().any(|n| n == name));
        #[cfg(target_os = "macos")]
        {
            // Resource forks come in pieces when the buffer is too small (no ERANGE).
            let fork: Vec<u8> = (0..(VALUE_BUF as u32) * 5).map(|i| (i % 251) as u8).collect();
            set(&p, b"com.apple.ResourceFork", &fork).unwrap();
            assert_eq!(get(&p, b"com.apple.ResourceFork").unwrap().as_deref(), Some(&fork[..]));
        }
        remove(&p, name).unwrap();
        remove(&p, name).unwrap();
        assert_eq!(get(&p, name).unwrap(), None);
        assert!(!names(&p).unwrap().iter().any(|n| n == name));
    }
}
