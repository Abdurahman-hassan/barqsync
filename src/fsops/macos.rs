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
    if size < CLONE_MIN_SIZE {
        let dst = create_new(tmp, mode)?;
        copy_buffered(src, &dst)?;
        return Ok(Copied::Written(dst));
    }
    let c = cstr(tmp)?;
    let r = unsafe {
        libc::fclonefileat(
            src.as_raw_fd(),
            libc::AT_FDCWD,
            c.as_ptr(),
            CLONE_NOFOLLOW | CLONE_NOOWNERCOPY,
        )
    };
    if r == 0 {
        return Ok(Copied::Clone);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EEXIST) => Err(err),
        Some(libc::ENOTSUP | libc::EXDEV | libc::EINVAL | libc::ENOSYS | libc::EPERM) => {
            let dst = create_new(tmp, mode)?;
            copy_buffered(src, &dst)?;
            Ok(Copied::Written(dst))
        }
        _ => Err(err),
    }
}
