//! Linux: `copy_file_range` keeps data in the kernel and reflinks automatically on
//! btrfs/XFS; falls back to a buffered copy where the kernel refuses (old kernels, some FUSE/NFS).

use super::{Copied, copy_buffered, create_new};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

pub fn copy_new(src: &File, tmp: &Path, _size: u64, mode: u32) -> io::Result<Copied> {
    let dst = create_new(tmp, mode)?;
    let mut copied: u64 = 0;
    loop {
        super::check_interrupt()?;
        let n = unsafe {
            libc::copy_file_range(
                src.as_raw_fd(),
                std::ptr::null_mut(),
                dst.as_raw_fd(),
                std::ptr::null_mut(),
                64 << 20,
                0,
            )
        };
        if n == 0 {
            return Ok(Copied::Written(dst));
        }
        if n > 0 {
            copied += n as u64;
            continue;
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EXDEV | libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP | libc::EPERM)
                if copied == 0 =>
            {
                copy_buffered(src, &dst)?;
                return Ok(Copied::Written(dst));
            }
            _ => return Err(err),
        }
    }
}
