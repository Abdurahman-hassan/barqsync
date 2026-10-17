//! io_uring batches for small-file writes (Linux, `--io-uring`; the original plan's
//! Phase 4.1).
//!
//! A remote receiver writes each small-file pack as three ring submissions: create every
//! temp file (`openat`), write every file's data, close every file. That replaces three
//! syscalls per file with three per pack (in batches of [`ENTRIES`]). Owner, mode, times
//! and the rename stay ordinary syscalls, as before.
//!
//! The plan named tokio-uring; this uses the `io-uring` crate it is built on, because
//! batching is the point and tokio-uring submits per future, not per batch. Each worker
//! thread keeps its own ring. If the kernel refuses io_uring (old kernel, seccomp), the
//! normal path is used.

use io_uring::{IoUring, opcode, squeue, types};
use std::cell::RefCell;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::LazyLock;

/// Submission queue size: operations per `io_uring_enter`.
pub const ENTRIES: u32 = 256;

static AVAILABLE: LazyLock<bool> = LazyLock::new(|| IoUring::new(8).is_ok());

/// Whether this kernel (and sandbox) allows io_uring.
pub fn available() -> bool {
    *AVAILABLE
}

thread_local! {
    static RING: RefCell<Option<IoUring>> = const { RefCell::new(None) };
}

/// Run `ops` in batches of [`ENTRIES`]; each op's raw result (`-errno` on failure), in order.
///
/// # Safety
/// Every buffer and path an op points to must stay valid until this returns (it waits
/// for all of them).
unsafe fn run(ops: Vec<squeue::Entry>) -> io::Result<Vec<i32>> {
    RING.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(IoUring::new(ENTRIES)?);
        }
        let ring = slot.as_mut().expect("ring");
        let mut results = vec![0i32; ops.len()];
        let mut base = 0usize;
        for chunk in ops.chunks(ENTRIES as usize) {
            {
                let mut sq = ring.submission();
                for (i, op) in chunk.iter().enumerate() {
                    let op = op.clone().user_data((base + i) as u64);
                    // SAFETY: the caller keeps the buffers alive; the queue has room
                    // for a whole chunk.
                    unsafe { sq.push(&op) }.map_err(|_| io::Error::other("io_uring queue full"))?;
                }
            }
            ring.submit_and_wait(chunk.len())?;
            let mut done = 0;
            while done < chunk.len() {
                for cqe in ring.completion() {
                    results[cqe.user_data() as usize] = cqe.result();
                    done += 1;
                }
                if done < chunk.len() {
                    ring.submit_and_wait(chunk.len() - done)?;
                }
            }
            base += chunk.len();
        }
        Ok(results)
    })
}

fn result(r: i32) -> io::Result<i32> {
    if r < 0 { Err(io::Error::from_raw_os_error(-r)) } else { Ok(r) }
}

/// Create each new file (`O_EXCL`) with its mode, in one batch.
pub fn create_new(paths: &[CString], modes: &[u32]) -> io::Result<Vec<io::Result<OwnedFd>>> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC;
    let ops = paths
        .iter()
        .zip(modes)
        .map(|(p, &m)| opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), p.as_ptr()).flags(flags).mode(m).build())
        .collect();
    // SAFETY: `paths` outlives the call.
    let res = unsafe { run(ops)? };
    Ok(res.into_iter().map(|r| result(r).map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })).collect())
}

/// Write each buffer at offset 0 of its file, in one batch. A short write is finished
/// with ordinary `pwrite`s.
pub fn write_all(files: &[(RawFd, &[u8])]) -> io::Result<Vec<io::Result<()>>> {
    let ops = files
        .iter()
        .map(|&(fd, buf)| opcode::Write::new(types::Fd(fd), buf.as_ptr(), buf.len() as u32).offset(0).build())
        .collect();
    // SAFETY: `files` (and so every buffer) outlives the call.
    let res = unsafe { run(ops)? };
    Ok(files
        .iter()
        .zip(res)
        .map(|(&(fd, buf), r)| {
            let n = result(r)? as usize;
            if n < buf.len() {
                // SAFETY: borrowed only for this call; the caller keeps owning `fd`.
                let f = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
                std::os::unix::fs::FileExt::write_all_at(&*f, &buf[n..], n as u64)?;
            }
            Ok(())
        })
        .collect())
}

/// Close every file, in one batch.
pub fn close_all(fds: Vec<OwnedFd>) -> io::Result<Vec<io::Result<()>>> {
    let raw: Vec<RawFd> = fds.into_iter().map(IntoRawFd::into_raw_fd).collect();
    let ops = raw.iter().map(|&fd| opcode::Close::new(types::Fd(fd)).build()).collect();
    // SAFETY: closes take no buffers.
    let res = unsafe { run(ops)? };
    Ok(res.into_iter().map(|r| result(r).map(drop)).collect())
}

/// A file's raw descriptor, for building batches.
pub fn raw(f: &OwnedFd) -> RawFd {
    f.as_raw_fd()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn batches_create_write_and_close() {
        if !available() {
            return eprintln!("SKIP: no io_uring here");
        }
        let dir = tempfile::tempdir().unwrap();
        let n = ENTRIES as usize + 7; // more than one submission
        let paths: Vec<CString> =
            (0..n).map(|i| CString::new(dir.path().join(format!("f{i}")).as_os_str().as_bytes()).unwrap()).collect();
        let data: Vec<Vec<u8>> = (0..n).map(|i| vec![i as u8; 100 + i]).collect();
        let fds: Vec<OwnedFd> = create_new(&paths, &vec![0o640; n]).unwrap().into_iter().map(Result::unwrap).collect();
        // O_EXCL: a second create fails per file, not as a batch.
        assert!(create_new(&paths[..1], &[0o640]).unwrap()[0].as_ref().is_err());
        let batch: Vec<(RawFd, &[u8])> = fds.iter().zip(&data).map(|(f, d)| (raw(f), d.as_slice())).collect();
        write_all(&batch).unwrap().into_iter().for_each(Result::unwrap);
        close_all(fds).unwrap().into_iter().for_each(Result::unwrap);
        for (i, d) in data.iter().enumerate() {
            assert_eq!(&std::fs::read(dir.path().join(format!("f{i}"))).unwrap(), d);
        }
    }
}
