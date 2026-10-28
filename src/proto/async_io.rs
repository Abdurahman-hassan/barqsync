//! The tokio runtime that owns network I/O, and the original plan's tokio-driven
//! connection layer.
//!
//! QUIC always runs here. For the ssh pipe and `--streams` TCP the plan also had tokio
//! own the I/O; that layer is built (reader and writer tasks instead of threads, the ssh
//! pipe through non-blocking descriptors, TLS through tokio-rustls) and selected with the
//! hidden `--net-io=tokio`. The thread-based layer stays the default: it measured as
//! fast or faster (see the README).

use super::{Frame, Pieces, body_len};
use crossbeam_channel::Sender;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, ready};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter, ReadBuf};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

static RT: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("barqsync-net")
        .enable_all()
        .build()
        .expect("start the network runtime")
});

pub fn rt() -> &'static Runtime {
    &RT
}

static ENABLED: AtomicBool = AtomicBool::new(false);

/// `--net-io=tokio` (hidden; `BARQSYNC_NET_IO`): drive the ssh pipe and TCP streams from
/// tokio tasks. The client passes it on to the server's command line.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut pieces = Pieces::default();
    loop {
        let mut hdr = [0u8; 5];
        match r.read(&mut hdr[..1]).await? {
            0 if !pieces.pending() => return Ok(None),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            _ => r.read_exact(&mut hdr[1..]).await?,
        };
        super::note_read();
        // Each read counts as activity for --timeout (see `note_read`).
        let mut body = vec![0u8; body_len(&hdr)?];
        let mut got = 0;
        while got < body.len() {
            match r.read(&mut body[got..]).await? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => {
                    got += n;
                    super::note_read();
                }
            }
        }
        super::count(&super::BYTES_RECEIVED, hdr.len() + body.len());
        if let Some(f) = pieces.add(hdr[4], body)? {
            return Ok(Some(f));
        }
    }
}

/// Writer task: drains queued frames into `w`. An empty buffer is the close request.
/// `urgent`: a lane whose frames go before anything queued (see
/// [`super::FrameSender::tell_interrupted`]).
pub(crate) fn spawn_writer(
    w: impl AsyncWrite + Unpin + Send + 'static,
    depth: usize,
    mut urgent: Option<mpsc::Receiver<Vec<u8>>>,
    stop: std::sync::Arc<super::Stopper>,
) -> (mpsc::Sender<Vec<u8>>, tokio::task::JoinHandle<io::Result<()>>) {
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(depth);
    let task = rt().spawn(async move {
        let mut w = BufWriter::with_capacity(256 << 10, w);
        let r = async {
            loop {
                let next = match urgent.as_mut() {
                    Some(u) => match u.try_recv() {
                        Ok(buf) => Some(buf),
                        Err(_) => {
                            std::future::poll_fn(|cx| {
                                if let Poll::Ready(Some(buf)) = u.poll_recv(cx) {
                                    return Poll::Ready(Some(buf));
                                }
                                rx.poll_recv(cx)
                            })
                            .await
                        }
                    },
                    None => rx.recv().await,
                };
                let Some(buf) = next else { break };
                if buf.is_empty() {
                    break;
                }
                match crate::bwlimit::connections() {
                    // --bwlimit: paced pieces, each sent before the next waits.
                    Some(rate) => {
                        for piece in buf.chunks(crate::bwlimit::piece(rate)) {
                            if let Some(wait) = crate::bwlimit::reserve(rate, piece.len()) {
                                pause(wait).await;
                            }
                            w.write_all(piece).await?;
                            w.flush().await?;
                        }
                    }
                    None => w.write_all(&buf).await?,
                }
                super::count(&super::BYTES_SENT, buf.len());
                if rx.is_empty() && urgent.as_ref().is_none_or(|u| u.is_empty()) {
                    w.flush().await?;
                }
            }
            w.flush().await?;
            w.shutdown().await
        }
        .await;
        if let Err(e) = &r {
            stop.fail(e);
        }
        r
    });
    (tx, task)
}

/// [`crate::bwlimit::pause`] for a task: a wait cut short by Ctrl-C, so that the rest of
/// a frame, and the `Interrupted` notice queued behind it, go out at once.
async fn pause(wait: std::time::Duration) {
    let end = tokio::time::Instant::now() + wait;
    while !crate::fsops::interrupted() {
        let left = end.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return;
        }
        tokio::time::sleep(left.min(crate::bwlimit::NAP)).await;
    }
}

/// Reader task: decodes frames from `r` into `out`, then says how the stream ended (a
/// plain end of stream is an error too). `data_stream`: ends after `StreamEnd`.
pub(crate) fn spawn_reader(
    r: impl AsyncRead + Unpin + Send + 'static,
    out: Sender<io::Result<Frame>>,
    data_stream: bool,
    on_exit: Option<Box<dyn FnOnce() + Send>>,
) {
    rt().spawn(async move {
        let mut r = BufReader::with_capacity(256 << 10, r);
        loop {
            let f = match read_frame(&mut r).await {
                Ok(Some(f)) => f,
                Ok(None) => {
                    // The control stream too: data streams may keep the channel open.
                    let e = match data_stream {
                        true => super::stream_lost(io::Error::new(io::ErrorKind::UnexpectedEof, "data stream closed early")),
                        false => io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed by peer"),
                    };
                    let _ = tokio::task::block_in_place(|| out.send(Err(e)));
                    break;
                }
                Err(e) => {
                    let e = if data_stream { super::stream_lost(e) } else { e };
                    let _ = tokio::task::block_in_place(|| out.send(Err(e)));
                    break;
                }
            };
            if let Some(lag) = super::notice_lag(&f).filter(|_| !data_stream) {
                tokio::time::sleep(lag).await;
            }
            let end = data_stream && matches!(f, Frame::Msg(super::Msg::StreamEnd));
            // The consumer applies backpressure through this bounded channel.
            if tokio::task::block_in_place(|| out.send(Ok(f))).is_err() || end {
                break;
            }
        }
        if let Some(f) = on_exit {
            f();
        }
    });
}

/// A pipe or socket descriptor (the ssh process's stdin/stdout) driven by tokio.
pub struct FdIo {
    fd: AsyncFd<OwnedFd>,
}

impl FdIo {
    pub fn new(fd: OwnedFd) -> io::Result<FdIo> {
        let raw = fd.as_raw_fd();
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let _rt = rt().enter();
        // SAFETY: the `OwnedFd` keeps the descriptor open, and pointing at the same file
        // description, until the `AsyncFd` that owns it is dropped.
        let fd = unsafe { AsyncFd::register(fd) }?;
        Ok(FdIo { fd })
    }
}

impl AsyncRead for FdIo {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.fd.poll_read_ready(cx))?;
            let dst = buf.initialize_unfilled();
            let r = guard.try_io(|fd| {
                let n = unsafe { libc::read(fd.as_raw_fd(), dst.as_mut_ptr().cast(), dst.len()) };
                if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
            });
            match r {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for FdIo {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, src: &[u8]) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.fd.poll_write_ready(cx))?;
            let r = guard.try_io(|fd| {
                let n = unsafe { libc::write(fd.as_raw_fd(), src.as_ptr().cast(), src.len()) };
                if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
            });
            match r {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
