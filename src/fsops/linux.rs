//! Linux: `copy_file_range` keeps data in the kernel and reflinks automatically on
//! btrfs/XFS; falls back to a buffered copy where the kernel refuses (old kernels, some FUSE/NFS).

use super::{Copied, cache, copy_buffered, create_new};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

/// `_IOW(0x94, 9, int)`: share `src`'s extents with the destination (btrfs, XFS, ...).
const FICLONE: libc::c_ulong = 0x4004_9409;

/// Reflink `src` into the new path `tmp`. `Ok(false)` means the filesystem can't
/// (ext4, cross-device, ...) and nothing is left behind.
pub fn clone_into_new(src: &File, tmp: &Path) -> io::Result<bool> {
    let dst = create_new(tmp, 0o600)?;
    if unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    drop(dst);
    let _ = std::fs::remove_file(tmp);
    match err.raw_os_error() {
        Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::ENOTTY | libc::EPERM | libc::ENOSYS) => Ok(false),
        _ => Err(err),
    }
}

/// Bytes requested per `copy_file_range` call.
const CHUNK: usize = 64 << 20;

pub fn copy_new(src: &File, tmp: &Path, size: u64, mode: u32) -> io::Result<Copied> {
    let dst = create_new(tmp, mode)?;
    // Without a reflink the kernel copies through the page cache like a buffered copy.
    let mut reader = cache::Reader::new(src, size);
    let mut writer = cache::Writer::new(&dst, size);
    let mut copied: u64 = 0;
    loop {
        super::check_interrupt()?;
        let n = unsafe {
            libc::copy_file_range(
                src.as_raw_fd(),
                std::ptr::null_mut(),
                dst.as_raw_fd(),
                std::ptr::null_mut(),
                CHUNK,
                0,
            )
        };
        if n == 0 {
            return Ok(Copied::Written(dst));
        }
        if n > 0 {
            copied += n as u64;
            crate::progress::copied(n as u64);
            reader.read_to(src, copied);
            writer.written_to(&dst, copied);
            // A short copy means EOF; once the expected size is in, skip the extra call that
            // would only return 0 (one syscall per file saved). A file that grew keeps looping.
            if (n as usize) < CHUNK && copied >= size {
                return Ok(Copied::Written(dst));
            }
            continue;
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EXDEV | libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP | libc::EPERM)
                if copied == 0 =>
            {
                copy_buffered(src, &dst, size)?;
                return Ok(Copied::Written(dst));
            }
            _ => return Err(err),
        }
    }
}

/// `fstatat` replacement: `statx` asks only for the fields sync decisions use and
/// never forces a network filesystem to resynchronise attributes.
pub fn statx_at(dirfd: std::os::fd::RawFd, name: &std::ffi::CStr) -> io::Result<crate::entry::Meta> {
    use crate::entry::{Kind, Meta};
    let mut sx: libc::statx = unsafe { std::mem::zeroed() };
    let mask = libc::STATX_TYPE
        | libc::STATX_MODE
        | libc::STATX_NLINK
        | libc::STATX_UID
        | libc::STATX_GID
        | libc::STATX_MTIME
        | libc::STATX_INO
        | libc::STATX_SIZE;
    let flags = libc::AT_SYMLINK_NOFOLLOW | libc::AT_STATX_DONT_SYNC;
    if unsafe { libc::statx(dirfd, name.as_ptr(), flags, mask, &mut sx) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let fmt = sx.stx_mode as u32 & libc::S_IFMT;
    let kind = match fmt {
        libc::S_IFREG => Kind::File,
        libc::S_IFDIR => Kind::Dir,
        libc::S_IFLNK => Kind::Symlink,
        libc::S_IFIFO => Kind::Fifo,
        libc::S_IFCHR => Kind::CharDev,
        libc::S_IFBLK => Kind::BlockDev,
        _ => Kind::Socket,
    };
    Ok(Meta {
        kind,
        size: sx.stx_size,
        mtime: sx.stx_mtime.tv_sec,
        mtime_ns: sx.stx_mtime.tv_nsec,
        mode: sx.stx_mode as u32 & 0o7777,
        uid: sx.stx_uid,
        gid: sx.stx_gid,
        dev: libc::makedev(sx.stx_dev_major, sx.stx_dev_minor),
        ino: sx.stx_ino,
        rdev: libc::makedev(sx.stx_rdev_major, sx.stx_rdev_minor),
        nlink: sx.stx_nlink as u64,
    })
}
