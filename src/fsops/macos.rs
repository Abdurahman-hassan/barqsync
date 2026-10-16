//! macOS: APFS copy-on-write clones via `fclonefileat`, with a buffered fallback for
//! cross-volume copies and filesystems without clone support (HFS+, exFAT, SMB, ...).

use super::{Copied, copy_buffered, create_new, cstr};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

const CLONE_NOFOLLOW: u32 = 0x0001;
const CLONE_NOOWNERCOPY: u32 = 0x0002;

/// Below this size a plain copy beats `fclonefileat` on APFS: cloning costs extra
/// metadata work per file, while small copies are a single read and write.
/// Measured on an M-series Mac: copies win by ~20% at 16-256 KiB, clones win 2x at 1 MiB.
const CLONE_MIN_SIZE: u64 = 512 * 1024;

pub fn copy_new(src: &File, tmp: &Path, size: u64, mode: u32) -> io::Result<Copied> {
    if size >= CLONE_MIN_SIZE && clone_into_new(src, tmp)? {
        return Ok(Copied::Clone);
    }
    let dst = create_new(tmp, mode)?;
    copy_buffered(src, &dst, size)?;
    Ok(Copied::Written(dst))
}

/// Clone `src` to the new path `tmp` with `fclonefileat`. `Ok(false)` means the
/// filesystem can't clone (non-APFS, cross-volume) and nothing was created.
pub fn clone_into_new(src: &File, tmp: &Path) -> io::Result<bool> {
    let c = cstr(tmp)?;
    let r = unsafe {
        libc::fclonefileat(src.as_raw_fd(), libc::AT_FDCWD, c.as_ptr(), CLONE_NOFOLLOW | CLONE_NOOWNERCOPY)
    };
    if r == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOTSUP | libc::EXDEV | libc::EINVAL | libc::ENOSYS | libc::EPERM) => Ok(false),
        _ => Err(err),
    }
}

// ---------------------------------------------------------------------------
// getattrlistbulk: a directory's names and attributes, many entries per syscall
// ---------------------------------------------------------------------------

use super::DirHandle;
use crate::entry::{DirEnt, Kind, Meta};
use std::cell::RefCell;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

/// Not exported by the libc crate.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;

thread_local! {
    static BULK_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Little cursor over one packed attribute record. Values are only 4-byte aligned.
struct Rec<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Rec<'_> {
    fn bytes<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let b = self.buf.get(self.pos..self.pos + N).ok_or_else(|| io::Error::other("short attribute record"))?;
        self.pos += N;
        Ok(b.try_into().unwrap())
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_ne_bytes(self.bytes()?))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_ne_bytes(self.bytes()?))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_ne_bytes(self.bytes()?))
    }
    fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_ne_bytes(self.bytes()?))
    }
}

fn kind_of(objtype: u32) -> Kind {
    // enum vtype from <sys/vnode.h>
    match objtype {
        1 => Kind::File,
        2 => Kind::Dir,
        3 => Kind::BlockDev,
        4 => Kind::CharDev,
        5 => Kind::Symlink,
        7 => Kind::Fifo,
        _ => Kind::Socket,
    }
}

/// All entries of the directory open in `dir`, or `None` if the filesystem doesn't
/// support `getattrlistbulk`. Directories (whose device number must be exact for `-x`
/// at mount points) and entries the kernel flags with an error get a regular `fstatat`;
/// symlink targets are read with `readlinkat`.
pub fn read_dir_bulk(dir: &DirHandle) -> io::Result<Option<Vec<DirEnt>>> {
    let mut al = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_OWNERID
            | libc::ATTR_CMN_GRPID
            | libc::ATTR_CMN_ACCESSMASK
            | libc::ATTR_CMN_FILEID
            | ATTR_CMN_ERROR,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_DEVTYPE | libc::ATTR_FILE_DATALENGTH,
        forkattr: 0,
    };
    let mut raw: Vec<(OsString, Option<Meta>)> = Vec::new();
    let r = BULK_BUF.with(|b| -> io::Result<bool> {
        let mut buf = b.borrow_mut();
        if buf.len() < 256 << 10 {
            buf.resize(256 << 10, 0);
        }
        loop {
            let n = unsafe {
                libc::getattrlistbulk(
                    dir.raw_fd(),
                    (&mut al as *mut libc::attrlist).cast(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::FSOPT_PACK_INVAL_ATTRS as u64,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if raw.is_empty() && matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL | libc::ENOSYS)) {
                    return Ok(false);
                }
                return Err(e);
            }
            if n == 0 {
                return Ok(true);
            }
            let mut off = 0usize;
            for _ in 0..n {
                let len = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
                raw.push(parse_record(&buf[off..off + len])?);
                off += len;
            }
        }
    })?;
    if !r {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(raw.len());
    for (name, meta) in raw {
        let meta = match meta {
            Some(m) if m.kind != Kind::Dir => m,
            _ => match dir.lstat(&name) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            },
        };
        let link = match meta.kind {
            Kind::Symlink => match dir.readlink(&name) {
                Ok(t) => Some(t),
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            },
            _ => None,
        };
        out.push(DirEnt { name, meta, link });
    }
    Ok(Some(out))
}

/// One record, as measured on APFS (macOS 26): `u32 length`; the returned-attributes
/// set; `ATTR_CMN_ERROR` (placed right after that set, not in bit order); the other
/// common attributes in bit order; then the file attributes, which are present only
/// for non-directories even with `FSOPT_PACK_INVAL_ATTRS`. Values are 4-byte aligned.
fn parse_record(rec: &[u8]) -> io::Result<(OsString, Option<Meta>)> {
    let mut r = Rec { buf: rec, pos: 4 };
    let _common_returned = r.u32()?;
    let _vol = r.u32()?;
    let _dir = r.u32()?;
    let file_returned = r.u32()?;
    let _fork = r.u32()?;
    let error = r.u32()?;
    let name_at = r.pos;
    let name_off = r.i32()?;
    let name_len = r.u32()? as usize;
    let start = (name_at as isize + name_off as isize) as usize;
    let name = rec.get(start..start + name_len.saturating_sub(1)).ok_or_else(|| io::Error::other("bad name"))?;
    let name = OsString::from_vec(name.to_vec());
    let dev = r.i32()? as u32 as u64;
    let objtype = r.u32()?;
    let mtime = r.i64()?;
    let mtime_ns = r.i64()? as u32;
    let uid = r.u32()?;
    let gid = r.u32()?;
    let mode = r.u32()? & 0o7777;
    let ino = r.u64()?;
    if error != 0 || file_returned == 0 {
        // Directories (no file attributes) and failed entries get a real fstatat.
        return Ok((name, None));
    }
    let nlink = r.u32()? as u64;
    let rdev = r.u32()? as u64;
    let size = r.i64()? as u64;
    let kind = kind_of(objtype);
    Ok((name, Some(Meta { kind, size, mtime, mtime_ns, mode, uid, gid, dev, ino, rdev, nlink })))
}
