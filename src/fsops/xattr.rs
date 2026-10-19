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
    matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::EOPNOTSUPP))
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
        let big = vec![7u8; if cfg!(target_os = "macos") { 100_000 } else { 2000 }];
        match set(&p, name, &big) {
            Err(e) if is_unsupported(&e) => return, // e.g. tmpfs without user xattrs
            r => r.unwrap(),
        }
        assert_eq!(get(&p, name).unwrap().as_deref(), Some(&big[..]));
        assert!(names(&p).unwrap().iter().any(|n| n == name));
        remove(&p, name).unwrap();
        remove(&p, name).unwrap();
        assert_eq!(get(&p, name).unwrap(), None);
        assert!(!names(&p).unwrap().iter().any(|n| n == name));
    }
}
