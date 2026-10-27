//! Wire protocol between a sender and a receiver over one duplex byte stream (the ssh
//! pipe). Frames are length-prefixed; control messages are postcard-encoded, file data
//! travels in raw `Data` frames. With `-z` each frame body is zstd-compressed. A message
//! larger than a frame may be (64 MiB) is cut into several, up to 1 GiB in all.
//!
//! A transfer, after the handshake:
//!
//! ```text
//! sender                                   receiver
//!   Roots ------------------------------------>   plan destination
//!   DirBatch (one per directory, as scanned) ->   diff against local dirs
//!   IoError (once, on a source read error) --->   delete nothing more
//!   ...                                  <-----   Need (one per DirBatch: files + basis signatures)
//!   SmallPack / FileBegin+Data+FileEnd /
//!   DeltaBegin+Data+FileEnd -------------------->   write files in parallel
//!   Xattrs (one per entry asked for in a Need) ->   write attribute values
//!   ScanDone, then SenderDone ----------------->
//!                                        <-----   ReceiverDone (stats)
//!   RemovalsDone (pull + --remove-source-files) ->
//! ```
//!
//! Each side runs a dedicated reader thread and writer thread, so neither side can
//! deadlock waiting for the other to drain the pipe.
//!
//! With `--streams N`, file data (`FileBegin`/`DeltaBegin`/`Data`/`FileEnd`,
//! `SmallPack`) moves to N extra TLS connections while control messages stay on the
//! ssh pipe. All frames of one file use the same stream, so they stay in order; each
//! stream ends with `StreamEnd`, and the receiver waits for every one before finishing.

use crate::checksum::Sum;
use crate::cli::Options;
use crate::ids::Names;
use crate::select::Sel;
use crate::delta::cdc::CdcSig;
use crate::delta::refine::{GapReq, GapSig, Grain};
use crate::delta::{BlockSig, ChunkSig, Op};
use crate::entry::{DirEnt, Meta};
use crate::xattrs::Xattr;
use crate::output::StatsSnapshot;
use crate::remote::adapt::Compression;
use crossbeam_channel::{Receiver, Sender, bounded};

pub mod async_io;
pub mod wire;

pub use wire::Wire;
use serde::{Deserialize, Serialize};
use std::io::{self, BufWriter, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const MAGIC: [u8; 8] = *b"BARQSYNC";
/// 9: `-X`/`-A` (attributes and ACLs in listings, roots and file data; `Xattrs`).
/// 10: `RemovalsDone` (a pull with `--remove-source-files` reports the server's removals);
///     `Synced` names entries only; `IoError` (source read errors stop deletions).
/// 11: `--filter`/`-F`/`-C` rules, `--max-delete`, `-m`, `--mkpath` and `--timeout` in the
///     options; `KeepAlive`; vanished files and stopped deletions in the counters. Also
///     `--bwlimit`, `--usermap`/`--groupmap` (`--chown`) and `--append(-verify)` in the
///     options, `Sig::Append`, and id 0's name in `IdNames`; `-T`, `--delay-updates`,
///     `--force`, `--ignore-errors`, the missing-args options and the symlink options in
///     the options, and missing entries (`DirEnt::missing`) in listings; `-E`, `-U`,
///     `--open-noatime` and `-N` in the options, and access and creation times in `Meta`.
pub const VERSION: u32 = 11;
/// Upper bound on a frame body, and on what a compressed one decompresses to; anything
/// larger is a corrupt stream.
pub const MAX_FRAME: usize = 64 << 20;
/// Upper bound on one message. One larger than a frame (the listing of a huge directory,
/// signatures of many big files) goes as several frames (see `KIND_PART`).
pub const MAX_MESSAGE: usize = 1 << 30;
/// File data is sent in pieces of this size.
pub const DATA_CHUNK: usize = 1 << 20;
/// Files up to this size are packed many-per-frame instead of streamed one by one.
pub const SMALL_FILE: u64 = 128 << 10;
/// Target size of a small-file pack.
pub const PACK_BYTES: usize = 4 << 20;

/// Bytes this process wrote to and read from its connections (rsync's "sent N bytes
/// received M bytes").
pub static BYTES_SENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static BYTES_RECEIVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn count(counter: &std::sync::atomic::AtomicU64, n: usize) {
    counter.fetch_add(n as u64, Ordering::Relaxed);
}

/// Milliseconds since the first call: the time base of `--timeout`'s idle checks.
pub(crate) fn clock() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// When bytes last arrived on a connection (see [`clock`]).
static LAST_READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bytes arrived: the connection is alive even while a frame is still on its way (one
/// can take longer than `--timeout` to arrive whole under `--bwlimit`).
fn note_read() {
    LAST_READ.store(clock(), Ordering::Relaxed);
}

/// When bytes last arrived on any connection (see [`clock`]).
pub(crate) fn last_read() -> u64 {
    LAST_READ.load(Ordering::Relaxed)
}

/// File id on the wire: directory id in the high half, index in its listing in the low half.
pub fn file_id(dir: u32, idx: u32) -> u64 {
    (dir as u64) << 32 | idx as u64
}

pub fn split_id(id: u64) -> (u32, u32) {
    ((id >> 32) as u32, id as u32)
}

/// One source argument as the sender sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct RootInfo {
    /// `src/`: the directory's contents are transferred (its own metadata is in `meta`).
    /// Otherwise the entries in `items` are transferred by name.
    pub contents: bool,
    pub meta: Option<Meta>,
    pub items: Vec<DirEnt>,
    /// `-R`/`--files-from`: the selected paths of this root. Both sides use it to
    /// decide which directories are descended into and pruned.
    pub sel: Option<Sel>,
    /// `-X`/`-A`: the `src/` directory's own attributes (values in full) and ACLs.
    pub extra: Option<Box<crate::entry::Extra>>,
}

/// The receiver's coarse basis-file signature, chosen by the receiver: aligned blocks when
/// the file kept its size (in-place edits), content-defined chunks when it changed size.
/// What the coarse delta leaves literal can then be refined (`Refine`, `Refined`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum Sig {
    Blocks(BlockSig),
    Chunks(ChunkSig),
    /// `--delta=cdc`: FastCDC chunks with xxh3 and BLAKE3 hashes.
    Cdc(CdcSig),
    /// `--append`: the receiver's file is the first `len` bytes of this one; only the
    /// rest is wanted. `--append-verify` sends the checksum of those bytes: if the
    /// sender's start differs, the file comes whole.
    Append { len: u64, sum: Option<Sum> },
}

/// Server → client: where to open parallel data streams and how to authenticate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct StreamOffer {
    /// `ip:port` the server listens on (the server end of the ssh connection).
    pub addr: String,
    /// BLAKE3 of the server's ephemeral certificate; the client accepts nothing else.
    pub fingerprint: [u8; 32],
    /// Shared secret the client presents inside TLS on every stream.
    pub token: [u8; 32],
}

/// `--remove-source-files`: paths (relative to the root) of entries the receiver now has.
pub type SyncedFiles = Vec<Vec<u8>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct NeedReq {
    pub idx: u32,
    pub sig: Option<Sig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum Msg {
    /// Client → server, first frame. `opts.paths` holds the server-side paths.
    Hello { magic: [u8; 8], version: u32, server_is_sender: bool, opts: Box<Options> },
    HelloAck { version: u32 },
    /// Parallel data streams (`--streams`): the server's offer, or why there is none.
    StreamOffer(StreamOffer),
    StreamUnavailable { reason: String },
    /// Client → server: how many streams it established (0 = stay on the ssh pipe).
    StreamsReady { count: u16 },
    /// `--transport quic`: where to connect and how to authenticate (sent over ssh).
    QuicOffer(StreamOffer),
    /// Adaptive strategy: the sender's link probe (see `remote::adapt`).
    Ping,
    Pong,
    ProbeAck,
    LinkInfo(crate::remote::adapt::LinkInfo),
    /// Last frame on a data stream.
    StreamEnd,
    /// The peer is giving up; the message says why.
    Fatal { msg: String },
    /// The peer was interrupted (Ctrl-C) and is stopping: this side stops too, as if
    /// interrupted itself (exit 20).
    Interrupted,
    /// Output produced on the server, printed by the client. `escape`: the text may
    /// contain file names and is escaped with the client's charset before printing.
    Log { stderr: bool, escape: bool, text: Vec<u8> },

    // sender → receiver
    Roots(Vec<RootInfo>),
    /// `-c`: `sums[i]` is the checksum of regular file `entries[i]` (empty otherwise).
    DirBatch { root: u16, dir: u32, rel: Vec<u8>, entries: Vec<DirEnt>, sums: Vec<Option<Sum>> },
    /// User and group names for ids in the listings that follow (no `--numeric-ids`).
    IdNames { users: Names, groups: Names },
    ScanDone { dirs: u32 },
    /// The sender could not read part of its source (rsync's `io_error`): listings sent
    /// from now on may lack entries, so the receiver deletes nothing more. Sent before
    /// the listing the error concerns.
    IoError,
    /// `xattrs`: `-X` attribute values in full, when the listing only had checksums of some.
    FileBegin { id: u64, size: u64, xattrs: Option<Vec<Xattr>> },
    /// Start a delta-encoded file: `copies` come from the receiver's basis, the
    /// remaining ranges arrive as `Data` frames.
    DeltaBegin { id: u64, size: u64, copies: Vec<Op>, xattrs: Option<Vec<Xattr>> },
    /// Sender → receiver, before `DeltaBegin`: fine signatures wanted for the basis
    /// stretches the coarse delta left literal (see `delta::refine`).
    Refine { id: u64, grain: Grain, gaps: Vec<GapReq> },
    /// Receiver → sender: one fine signature per requested stretch, or none at all if the
    /// receiver can't refine (the sender then keeps the coarse delta).
    Refined { id: u64, gaps: Vec<GapSig> },
    FileEnd { id: u64, ok: bool },
    /// Several complete small files: `(id, len)` pairs, contents concatenated in `blob`;
    /// full attribute values for the files that need them (see `FileBegin`).
    SmallPack { files: Vec<(u64, u32)>, blob: Vec<u8>, xattrs: Vec<(u64, Vec<Xattr>)> },
    /// The answer to a `Need`'s `xattrs`: one entry's attributes with their values in
    /// full (`None`: the sender couldn't read them, and said why).
    Xattrs { id: u64, xattrs: Option<Vec<Xattr>> },
    /// `vanished`: files that were gone when they were to be sent (rsync's exit 24).
    SenderDone { errors: u64, vanished: u64 },
    /// A pull with `--remove-source-files` (see `remote::reports_removals`): the sender's
    /// last frame, after `ReceiverDone`, once every `Synced` entry is dealt with and its
    /// output sent. `errors`: failures since `SenderDone`.
    RemovalsDone { errors: u64 },

    // receiver → sender
    /// The answer to one `DirBatch`: which of its files the receiver wants, and (`-X`)
    /// which entries' attribute values it needs in full to update them in place.
    Need { dir: u32, files: Vec<NeedReq>, xattrs: Vec<u32> },
    /// `--remove-source-files`: these entries are on the receiver now. The sender removes
    /// only entries it listed itself, unchanged since by its own record of them.
    Synced { root: u16, files: SyncedFiles },
    ReceiverDone { stats: StatsSnapshot },

    // either way
    /// `--timeout`: sent by a side with nothing else to send for a while, so the other
    /// side can tell an idle connection from a dead one.
    KeepAlive,
}

#[derive(Debug)]
pub enum Frame {
    Msg(Msg),
    Data { id: u64, off: u64, bytes: Vec<u8> },
}

/// A postcard-encoded message.
const KIND_MSG: u8 = 0;
const KIND_DATA: u8 = 1;
/// An rkyv-encoded message (`--wire=rkyv`).
const KIND_RKYV: u8 = 2;
/// A piece of a message larger than [`MAX_FRAME`]: its frames follow one another, each
/// piece but the last of this kind, the last one of the message's own kind.
const KIND_PART: u8 = 3;
const FLAG_ZSTD: u8 = 0x80;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Encode a frame: `u32 LE body length`, `u8 kind`, body. Compressed bodies are
/// `u32 LE raw length` + zstd stream. `Hello` is always postcard, so a peer of any
/// version can read it before the session's [`Wire`] is agreed. A message larger than
/// [`MAX_FRAME`] becomes several frames, each piece compressed on its own.
pub fn encode(frame: &Frame, level: Option<i32>, wire: Wire) -> io::Result<Vec<u8>> {
    let (kind, body) = match frame {
        Frame::Msg(m) if wire == Wire::Rkyv && !matches!(m, Msg::Hello { .. }) => {
            (KIND_RKYV, rkyv::to_bytes::<rkyv::rancor::Error>(m).map_err(|e| invalid(e.to_string()))?.into_vec())
        }
        Frame::Msg(m) => (KIND_MSG, postcard::to_stdvec(m).map_err(|e| invalid(e.to_string()))?),
        Frame::Data { id, off, bytes } => {
            let mut b = Vec::with_capacity(16 + bytes.len());
            b.extend_from_slice(&id.to_le_bytes());
            b.extend_from_slice(&off.to_le_bytes());
            b.extend_from_slice(bytes);
            (KIND_DATA, b)
        }
    };
    if body.len() > MAX_MESSAGE {
        return Err(invalid(format!("message too large to send: {} MiB (the limit is {} MiB)", body.len() >> 20, MAX_MESSAGE >> 20)));
    }
    let piece = piece_size();
    let mut out = Vec::with_capacity(body.len() + 5 * body.len().div_ceil(piece).max(1));
    let mut pieces = body.chunks(piece).peekable();
    if body.is_empty() {
        push_frame(&mut out, kind, &body, level)?;
    }
    while let Some(piece) = pieces.next() {
        let k = if pieces.peek().is_some() { KIND_PART } else { kind };
        push_frame(&mut out, k, piece, level)?;
    }
    Ok(out)
}

/// The size messages are cut into frames at: [`MAX_FRAME`], or `BARQSYNC_TEST_FRAME_PIECE`
/// bytes (tests: small messages then cross as several frames too).
fn piece_size() -> usize {
    static PIECE: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        let n = std::env::var("BARQSYNC_TEST_FRAME_PIECE").ok().and_then(|v| v.parse().ok());
        n.map_or(MAX_FRAME, |n: usize| n.clamp(1, MAX_FRAME))
    });
    *PIECE
}

/// How long the control stream's reader holds a peer's `Interrupted` before passing it
/// on: `BARQSYNC_TEST_NOTICE_LAG_MS` (tests: a control path slower than the data streams,
/// whose end then arrives first), else not at all.
fn notice_lag(f: &Frame) -> Option<Duration> {
    static LAG: std::sync::LazyLock<Option<Duration>> = std::sync::LazyLock::new(|| {
        std::env::var("BARQSYNC_TEST_NOTICE_LAG_MS").ok().and_then(|v| v.parse().ok()).map(Duration::from_millis)
    });
    if matches!(f, Frame::Msg(Msg::Interrupted)) { *LAG } else { None }
}

/// Append one frame of at most [`MAX_FRAME`] bytes, compressed when that makes it smaller.
fn push_frame(out: &mut Vec<u8>, kind: u8, body: &[u8], level: Option<i32>) -> io::Result<()> {
    let z = match level.filter(|_| body.len() >= 256) {
        Some(level) => Some(zstd::bulk::compress(body, level)?).filter(|z| z.len() + 4 < body.len()),
        None => None,
    };
    match z {
        Some(z) => {
            out.extend_from_slice(&((4 + z.len()) as u32).to_le_bytes());
            out.push(kind | FLAG_ZSTD);
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&z);
        }
        None => {
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.push(kind);
            out.extend_from_slice(body);
        }
    }
    Ok(())
}

/// Read one frame. `Ok(None)` on a clean end of stream between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut pieces = Pieces::default();
    loop {
        let mut hdr = [0u8; 5];
        let mut got = 0;
        while got < hdr.len() {
            match r.read(&mut hdr[got..]) {
                Ok(0) if got == 0 && !pieces.pending() => return Ok(None),
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    got += n;
                    note_read();
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let mut body = vec![0u8; body_len(&hdr)?];
        let mut got = 0;
        while got < body.len() {
            match r.read(&mut body[got..]) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    got += n;
                    note_read();
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        count(&BYTES_RECEIVED, hdr.len() + body.len());
        if let Some(f) = pieces.add(hdr[4], body)? {
            return Ok(Some(f));
        }
    }
}

/// The pieces of a message that arrives as several frames, collected until its last one.
#[derive(Default)]
pub(crate) struct Pieces {
    buf: Vec<u8>,
    pending: bool,
}

impl Pieces {
    /// Take one frame's body. Returns the frame once its message is complete.
    pub(crate) fn add(&mut self, kind: u8, body: Vec<u8>) -> io::Result<Option<Frame>> {
        let body = unpack(kind, body)?;
        if self.buf.len() + body.len() > MAX_MESSAGE {
            return Err(invalid(format!("message exceeds the limit of {} MiB", MAX_MESSAGE >> 20)));
        }
        if kind & !FLAG_ZSTD == KIND_PART {
            self.buf.extend_from_slice(&body);
            self.pending = true;
            return Ok(None);
        }
        let body = if self.pending {
            self.pending = false;
            let mut all = std::mem::take(&mut self.buf);
            all.extend_from_slice(&body);
            all
        } else {
            body
        };
        decode(kind & !FLAG_ZSTD, body).map(Some)
    }

    /// Part of a message has arrived: the stream must not end here.
    pub(crate) fn pending(&self) -> bool {
        self.pending
    }
}

/// Body length from a frame header, checked against [`MAX_FRAME`].
pub(crate) fn body_len(hdr: &[u8; 5]) -> io::Result<usize> {
    let len = u32::from_le_bytes(hdr[..4].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(invalid(format!("frame of {len} bytes exceeds the limit")));
    }
    Ok(len)
}

/// A frame body of the given kind, decompressed.
fn unpack(kind: u8, body: Vec<u8>) -> io::Result<Vec<u8>> {
    if kind & FLAG_ZSTD == 0 {
        return Ok(body);
    }
    let raw = u32::from_le_bytes(body.get(..4).ok_or_else(|| invalid("short frame"))?.try_into().unwrap());
    if raw as usize > MAX_FRAME {
        return Err(invalid("compressed frame too large"));
    }
    zstd::bulk::decompress(&body[4..], raw as usize)
}

/// Decode a message body of the given kind.
fn decode(kind: u8, mut body: Vec<u8>) -> io::Result<Frame> {
    match kind {
        KIND_MSG => Ok(Frame::Msg(postcard::from_bytes(&body).map_err(|e| invalid(e.to_string()))?)),
        KIND_RKYV => {
            // Validated (bytecheck) before use; the archive needs aligned storage.
            let mut aligned = rkyv::util::AlignedVec::<16>::with_capacity(body.len());
            aligned.extend_from_slice(&body);
            let m = rkyv::from_bytes::<Msg, rkyv::rancor::Error>(&aligned).map_err(|e| invalid(format!("bad message: {e}")))?;
            Ok(Frame::Msg(m))
        }
        KIND_DATA => {
            if body.len() < 16 {
                return Err(invalid("short data frame"));
            }
            let id = u64::from_le_bytes(body[..8].try_into().unwrap());
            let off = u64::from_le_bytes(body[8..16].try_into().unwrap());
            body.drain(..16);
            Ok(Frame::Data { id, off, bytes: body })
        }
        k => Err(invalid(format!("unknown frame kind {k}"))),
    }
}

/// A data stream's failure or early end, or the end of the ssh pipe under a QUIC
/// session, told apart from the control stream's end, which alone means the peer is
/// gone at once (see [`Conn::recv_timeout`]).
#[derive(Debug)]
struct StreamLost(io::Error);

impl std::fmt::Display for StreamLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for StreamLost {}

/// `e`, from a data stream's reader.
pub(crate) fn stream_lost(e: io::Error) -> io::Error {
    io::Error::new(e.kind(), StreamLost(e))
}

/// Whether `e` is a data stream's failure or early end. A malformed frame is the peer's
/// error, decided at once.
fn is_stream_lost(e: &io::Error) -> bool {
    e.kind() != io::ErrorKind::InvalidData && e.get_ref().is_some_and(|e| e.is::<StreamLost>())
}

/// Frames that travel on data streams when there are any: dropped once the transfer
/// can't complete (see [`Conn::recv_timeout`]).
fn carries_file_data(f: &Frame) -> bool {
    data_key(f).is_some() || matches!(f, Frame::Msg(Msg::StreamEnd))
}

/// Where encoded frames are queued: a writer thread's channel, or a writer task's.
#[derive(Clone)]
enum Tx {
    Thread(Sender<Vec<u8>>),
    Task(tokio::sync::mpsc::Sender<Vec<u8>>),
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "connection closed")
}

fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "the transfer was stopped")
}

impl Tx {
    /// Queue `buf`, waiting for room unless the connection is stopped meanwhile (see
    /// [`FrameSender::stopped`]): a stopped run must not wait for a peer that no longer
    /// reads.
    fn send(&self, buf: Vec<u8>, stop: &Stopper) -> io::Result<()> {
        if stop.is_set() {
            return Err(stopped());
        }
        match self {
            Tx::Thread(t) => crossbeam_channel::select! {
                send(t, buf) -> r => r.map_err(|_| closed()),
                recv(stop.wake) -> _ => Err(stopped()),
                recv(crate::fsops::interrupt_wake()) -> _ => Err(stopped()),
            },
            Tx::Task(t) => match t.try_send(buf) {
                Ok(()) => Ok(()),
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(closed()),
                Err(tokio::sync::mpsc::error::TrySendError::Full(buf)) => {
                    use std::future::Future;
                    use std::task::Poll;
                    let mut send = std::pin::pin!(t.send(buf));
                    let mut ours = std::pin::pin!(stop.token.cancelled());
                    let mut all = std::pin::pin!(crate::fsops::interrupt_token().cancelled());
                    async_io::rt().block_on(std::future::poll_fn(|cx| {
                        if let Poll::Ready(r) = send.as_mut().poll(cx) {
                            return Poll::Ready(r.map_err(|_| closed()));
                        }
                        if ours.as_mut().poll(cx).is_ready() || all.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(Err(stopped()));
                        }
                        Poll::Pending
                    }))
                }
            },
        }
    }

    /// Queue `buf` only if there is room now.
    fn try_send(&self, buf: Vec<u8>) -> bool {
        match self {
            Tx::Thread(t) => t.try_send(buf).is_ok(),
            Tx::Task(t) => t.try_send(buf).is_ok(),
        }
    }

    /// Queue the close request (an empty buffer), waiting for room until `deadline`.
    fn close(&self, deadline: Instant) {
        match self {
            Tx::Thread(t) => {
                let _ = t.send_deadline(Vec::new(), deadline);
            }
            Tx::Task(t) => {
                let left = deadline.saturating_duration_since(Instant::now());
                // The timer must be made inside the runtime.
                let _ = async_io::rt().block_on(async { tokio::time::timeout(left, t.send(Vec::new())).await });
            }
        }
    }
}

/// Stops a connection's sends (see [`FrameSender::cancel`]), and keeps the first
/// writer failure (see [`FrameSender::write_failure`]).
pub(crate) struct Stopper {
    token: tokio_util::sync::CancellationToken,
    /// Disconnected when stopped, for `select!`.
    wake_tx: std::sync::Mutex<Option<Sender<()>>>,
    wake: Receiver<()>,
    failed: std::sync::Mutex<Option<(io::ErrorKind, String)>>,
}

impl Stopper {
    fn new() -> Arc<Stopper> {
        let (tx, rx) = bounded(0);
        Arc::new(Stopper { token: Default::default(), wake_tx: std::sync::Mutex::new(Some(tx)), wake: rx, failed: Default::default() })
    }

    /// A writer failed: the peer can't be reached any more. Not news once stopping.
    fn fail(&self, e: &io::Error) {
        if !self.is_set() {
            self.failed.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert((e.kind(), format!("writing to the peer failed: {e}")));
        }
    }

    fn stop(&self) {
        self.token.cancel();
        self.wake_tx.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    fn is_set(&self) -> bool {
        self.token.is_cancelled() || crate::fsops::interrupted()
    }
}

/// A writer thread or task, joined when the connection finishes.
enum Joiner {
    Thread(JoinHandle<io::Result<()>>),
    Task(tokio::task::JoinHandle<io::Result<()>>),
}

impl Joiner {
    /// Wait for the writer until `deadline`. One stuck in a write to a peer that no longer
    /// reads is left behind (it ends with the process).
    fn join_until(self, deadline: Instant) -> io::Result<()> {
        let late = || io::Error::new(io::ErrorKind::TimedOut, "the peer stopped reading");
        match self {
            Joiner::Thread(h) => {
                while !h.is_finished() {
                    if Instant::now() >= deadline {
                        return Err(late());
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                h.join().unwrap_or_else(|_| Err(io::Error::other("writer thread panicked")))
            }
            Joiner::Task(mut h) => {
                let left = deadline.saturating_duration_since(Instant::now());
                match async_io::rt().block_on(async { tokio::time::timeout(left, &mut h).await }) {
                    Ok(r) => r.unwrap_or_else(|e| Err(io::Error::other(e))),
                    Err(_) => {
                        h.abort();
                        Err(late())
                    }
                }
            }
        }
    }
}

/// How long closing a connection waits for its writers to deliver what is queued.
/// After a complete transfer only a few small frames are left; a peer that stops
/// reading must not keep this side forever.
const CLOSE_WAIT: Duration = Duration::from_secs(30);
/// The same for a run that was stopped (interrupted, or failed): what is left is
/// best-effort.
const STOPPED_CLOSE_WAIT: Duration = Duration::from_secs(2);
/// How long a data stream's early end, the ssh pipe's end under QUIC, or a failed write,
/// waits for the control stream to say why (see [`Conn::recv_timeout`]).
const SETTLE: Duration = Duration::from_secs(2);
/// How long closing a QUIC session waits for the peer to end (see [`Conn::finish`]).
const PEER_WAIT: Duration = Duration::from_secs(5);
/// When closing a stopped run, a peer from which nothing at all arrives for this long, or
/// three round trips if longer, needs nothing more resent (see [`Conn::wait_for_peer`]).
/// A running peer acknowledges what it receives within a round trip, and QUIC probes
/// again sooner than this when a packet or its acknowledgement is lost.
const SILENT: Duration = Duration::from_secs(1);

/// What a connection runs over and must outlive it: a QUIC session.
pub trait Carrier: std::any::Any + Send + Sync {
    /// Datagrams received from the peer so far.
    fn received(&self) -> u64;
    /// The round trip to the peer, as measured so far.
    fn rtt(&self) -> Duration;
}

/// Cloneable handle for sending frames. Frames are encoded (and compressed) on the
/// calling thread, so many tasks can compress in parallel; writer threads only write.
#[derive(Clone)]
pub struct FrameSender {
    tx: Tx,
    /// Parallel data streams, when attached: file data goes here instead of `tx`.
    data: Option<Arc<Vec<Tx>>>,
    comp: Arc<Compression>,
    wire: Wire,
    stop: Arc<Stopper>,
    /// A one-frame lane to the control stream's writer, which takes it before anything
    /// queued: `Interrupted` gets out even when `tx` is full.
    urgent: Tx,
    /// `Interrupted` was queued for the peer.
    told: Arc<AtomicBool>,
}

/// Which data stream a frame belongs on (`None`: control, stays on the main stream).
/// Keyed by file id so every frame of one file travels the same, ordered, stream.
fn data_key(f: &Frame) -> Option<u64> {
    let id = match f {
        Frame::Data { id, .. } => *id,
        Frame::Msg(Msg::FileBegin { id, .. } | Msg::DeltaBegin { id, .. } | Msg::FileEnd { id, .. }) => *id,
        Frame::Msg(Msg::SmallPack { files, .. }) => files.first()?.0,
        _ => return None,
    };
    // Mix the bits: ids are `dir << 32 | index`, so low bits alone spread poorly.
    Some(id.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32)
}

impl FrameSender {
    fn new(tx: Tx, urgent: Tx, compress: bool, stop: Arc<Stopper>) -> FrameSender {
        FrameSender {
            tx,
            data: None,
            comp: Arc::new(Compression::new(compress, false, None)),
            wire: Wire::Postcard,
            stop,
            urgent,
            told: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Encode and queue a frame. Fails when the connection is gone or stopped.
    pub fn send(&self, frame: Frame) -> io::Result<()> {
        let level = self.comp.level(matches!(frame, Frame::Msg(Msg::SmallPack { .. })));
        let t = std::time::Instant::now();
        let buf = encode(&frame, level, self.wire)?;
        if level.is_some()
            && let Frame::Msg(Msg::SmallPack { blob, .. }) | Frame::Data { bytes: blob, .. } = &frame
        {
            self.comp.observe(blob.len(), t.elapsed());
        }
        let tx = match (&self.data, data_key(&frame)) {
            (Some(streams), Some(key)) if !streams.is_empty() => &streams[(key % streams.len() as u64) as usize],
            _ => &self.tx,
        };
        tx.send(buf, &self.stop)
    }

    /// Close every data stream with `StreamEnd`, after all file data has been queued.
    pub fn end_data_streams(&self) -> io::Result<()> {
        for tx in self.data.iter().flat_map(|d| d.iter()) {
            let buf = encode(&Frame::Msg(Msg::StreamEnd), None, self.wire)?;
            tx.send(buf, &self.stop)?;
        }
        Ok(())
    }

    pub fn msg(&self, m: Msg) -> io::Result<()> {
        self.send(Frame::Msg(m))
    }

    /// Queue a control message only if there is room right now (a keepalive: a full
    /// queue means data is moving anyway).
    pub fn try_msg(&self, m: Msg) -> bool {
        !self.stop.is_set() && encode(&Frame::Msg(m), None, self.wire).is_ok_and(|buf| self.tx.try_send(buf))
    }

    /// The run is stopping early (see [`FrameSender::stopped`]): sends fail from now on,
    /// and sends waiting for room give up, so no task waits for a peer that may no
    /// longer read.
    pub fn cancel(&self) {
        self.stop.stop();
    }

    /// Sends fail at once: the connection was cancelled or the run interrupted. Their
    /// errors are expected then, not worth reporting.
    pub fn stopped(&self) -> bool {
        self.stop.is_set()
    }

    /// Why a writer of this connection failed, if one did: the peer can't be reached, so
    /// waiting for its answers would be forever.
    pub fn write_failure(&self) -> Option<io::Error> {
        let f = self.stop.failed.lock().unwrap_or_else(|e| e.into_inner());
        f.as_ref().map(|(kind, msg)| io::Error::new(*kind, msg.clone()))
    }

    /// Tell the peer this side was interrupted (`Interrupted`), once: the peer then stops
    /// too instead of waiting. It goes on its own lane, written right after the frame
    /// being written however many are queued, so it never waits for room.
    pub fn tell_interrupted(&self) {
        if !self.told.load(Ordering::SeqCst)
            && let Ok(buf) = encode(&Frame::Msg(Msg::Interrupted), None, self.wire)
            && self.urgent.try_send(buf)
        {
            self.told.store(true, Ordering::SeqCst);
        }
    }

    /// The link is measured: compression levels follow it from now on.
    pub fn set_link(&self, link: crate::remote::adapt::LinkInfo) {
        self.comp.set_link(link);
    }
}

/// A running connection: a dedicated writer thread drains queued frames into the
/// stream, a dedicated reader thread decodes incoming frames.
pub struct Conn {
    pub out: FrameSender,
    pub rx: Receiver<io::Result<Frame>>,
    /// A spare sender for attaching data-stream readers. Shared with the main reader
    /// thread, which clears it when the main stream ends, so a spare never keeps the
    /// channel open after the peer is gone. [`Conn::seal`] clears it too.
    rtx: Arc<std::sync::Mutex<Option<Sender<io::Result<Frame>>>>>,
    writer: Option<Joiner>,
    data_writers: Vec<Joiner>,
    data_in: usize,
    /// What this connection runs over and must outlive it (a QUIC session).
    session: Option<Box<dyn Carrier>>,
    /// The connection this one replaced (the ssh pipe, once control moved to QUIC):
    /// idle, but kept open until this one finishes. Its end ends this one too (see
    /// [`Conn::recv_timeout`]).
    under: Option<Box<Conn>>,
    /// The ssh pipe in `under` has ended (it is no longer watched).
    under_ended: AtomicBool,
    /// The first data stream failure or failed write, and until when the control stream
    /// may still say why (see [`Conn::recv_timeout`]).
    lost: std::sync::Mutex<Option<(io::Error, Instant)>>,
}

/// Clears the spare sender when the main reader thread exits, however it exits.
struct ClearOnDrop(Arc<std::sync::Mutex<Option<Sender<io::Result<Frame>>>>>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        if let Ok(mut g) = self.0.lock() {
            g.take();
        }
    }
}

/// The next frame to write: one on the urgent lane (see [`FrameSender::tell_interrupted`])
/// before anything queued. `None`: every sender is gone.
fn next_frame(wrx: &Receiver<Vec<u8>>, urgent: Option<&Receiver<Vec<u8>>>) -> Option<Vec<u8>> {
    let Some(u) = urgent else { return wrx.recv().ok() };
    if let Ok(buf) = u.try_recv() {
        return Some(buf);
    }
    crossbeam_channel::select_biased! {
        recv(u) -> buf => buf.ok().or_else(|| wrx.recv().ok()),
        recv(wrx) -> buf => buf.ok(),
    }
}

fn spawn_writer(
    name: String,
    writer: impl Write + Send + 'static,
    wrx: Receiver<Vec<u8>>,
    urgent: Option<Receiver<Vec<u8>>>,
    stop: Arc<Stopper>,
) -> JoinHandle<io::Result<()>> {
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let mut w = BufWriter::with_capacity(256 << 10, writer);
            let r = (|| {
                // An empty buffer is the close request from `Conn::finish`.
                while let Some(buf) = next_frame(&wrx, urgent.as_ref()) {
                    if buf.is_empty() {
                        break;
                    }
                    match crate::bwlimit::connections() {
                        // --bwlimit: paced pieces, each sent before the next waits.
                        Some(rate) => {
                            for piece in buf.chunks(crate::bwlimit::piece(rate)) {
                                if let Some(wait) = crate::bwlimit::reserve(rate, piece.len()) {
                                    crate::bwlimit::pause(wait);
                                }
                                w.write_all(piece)?;
                                w.flush()?;
                            }
                        }
                        None => w.write_all(&buf)?,
                    }
                    count(&BYTES_SENT, buf.len());
                    if wrx.is_empty() && urgent.as_ref().is_none_or(Receiver::is_empty) {
                        w.flush()?;
                    }
                }
                w.flush()
            })();
            if let Err(e) = &r {
                stop.fail(e);
            }
            r
        })
        .expect("spawn writer thread")
}

impl Conn {
    pub fn start(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static, compress: bool) -> Conn {
        Conn::start_with(reader, writer, compress, Stopper::new())
    }

    fn start_with(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static, compress: bool, stop: Arc<Stopper>) -> Conn {
        // Frames are up to ~4 MiB, so these bound memory to a few tens of MiB per side.
        let (tx, wrx) = bounded::<Vec<u8>>(16);
        let (utx, urx) = bounded::<Vec<u8>>(1);
        let (rtx, rx) = bounded::<io::Result<Frame>>(16);
        let writer = spawn_writer("barqsync-writer".into(), writer, wrx, Some(urx), stop.clone());
        let spare = Arc::new(std::sync::Mutex::new(Some(rtx.clone())));
        let spare2 = spare.clone();
        std::thread::Builder::new()
            .name("barqsync-reader".into())
            .spawn(move || {
                let _clear = ClearOnDrop(spare2);
                let mut r = io::BufReader::with_capacity(256 << 10, reader);
                loop {
                    match read_frame(&mut r) {
                        Ok(Some(f)) => {
                            if let Some(lag) = notice_lag(&f) {
                                std::thread::sleep(lag);
                            }
                            if rtx.send(Ok(f)).is_err() {
                                return;
                            }
                        }
                        // Said even while data streams keep the channel open: a closed
                        // control stream means the peer is gone.
                        Ok(None) => {
                            let _ = rtx.send(Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed by peer")));
                            return;
                        }
                        Err(e) => {
                            let _ = rtx.send(Err(e));
                            return;
                        }
                    }
                }
            })
            .expect("spawn reader thread");
        Conn {
            out: FrameSender::new(Tx::Thread(tx), Tx::Thread(utx), compress, stop),
            rx,
            rtx: spare,
            writer: Some(Joiner::Thread(writer)),
            data_writers: Vec::new(),
            data_in: 0,
            session: None,
            under: None,
            under_ended: AtomicBool::new(false),
            lost: Default::default(),
        }
    }

    /// [`Conn::start`] with tokio tasks instead of threads (`BARQSYNC_NET_IO=tokio`).
    pub fn start_async(
        reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
        writer: impl tokio::io::AsyncWrite + Unpin + Send + 'static,
        compress: bool,
    ) -> Conn {
        let stop = Stopper::new();
        let (utx, urx) = tokio::sync::mpsc::channel(1);
        let (tx, task) = async_io::spawn_writer(writer, 16, Some(urx), stop.clone());
        let (rtx, rx) = bounded::<io::Result<Frame>>(16);
        let spare = Arc::new(std::sync::Mutex::new(Some(rtx.clone())));
        let clear = ClearOnDrop(spare.clone());
        async_io::spawn_reader(reader, rtx, false, Some(Box::new(move || drop(clear))));
        Conn {
            out: FrameSender::new(Tx::Task(tx), Tx::Task(utx), compress, stop),
            rx,
            rtx: spare,
            writer: Some(Joiner::Task(task)),
            data_writers: Vec::new(),
            data_in: 0,
            session: None,
            under: None,
            under_ended: AtomicBool::new(false),
            lost: Default::default(),
        }
    }

    /// [`Conn::attach_data_out`] for tokio streams.
    pub fn attach_data_out_async(&mut self, streams: Vec<Box<dyn tokio::io::AsyncWrite + Unpin + Send>>) {
        let mut senders = Vec::with_capacity(streams.len());
        for s in streams {
            let (tx, task) = async_io::spawn_writer(s, 4, None, self.out.stop.clone());
            self.data_writers.push(Joiner::Task(task));
            senders.push(Tx::Task(tx));
        }
        self.out.data = Some(Arc::new(senders));
    }

    /// [`Conn::attach_data_in`] for tokio streams.
    pub fn attach_data_in_async(&mut self, streams: Vec<Box<dyn tokio::io::AsyncRead + Unpin + Send>>) {
        let Some(rtx) = self.rtx.lock().unwrap().clone() else { return };
        for s in streams {
            self.data_in += 1;
            async_io::spawn_reader(s, rtx.clone(), true, None);
        }
    }

    /// Carry this connection on new streams from now on (control moving to QUIC). The
    /// current one stays open, idle, until [`Conn::finish`]; `session` is closed after
    /// everything sent on the new streams has been delivered.
    pub fn switch(&mut self, reader: impl Read + Send + 'static, writer: impl Write + Send + 'static, session: Box<dyn Carrier>) {
        let mut new = Conn::start_with(reader, writer, false, self.out.stop.clone());
        new.out.wire = self.out.wire;
        new.out.comp = self.out.comp.clone();
        new.out.told = self.out.told.clone();
        new.session = Some(session);
        let old = std::mem::replace(self, new);
        self.under = Some(Box::new(old));
    }

    /// Send file data over these streams from now on (the sending side).
    pub fn attach_data_out(&mut self, streams: Vec<Box<dyn Write + Send>>) {
        let mut senders = Vec::with_capacity(streams.len());
        for (i, s) in streams.into_iter().enumerate() {
            // Shallow queues: N streams × depth × ~4 MiB frames is the memory bound.
            let (tx, wrx) = bounded::<Vec<u8>>(4);
            self.data_writers.push(Joiner::Thread(spawn_writer(format!("barqsync-data-{i}"), s, wrx, None, self.out.stop.clone())));
            senders.push(Tx::Thread(tx));
        }
        self.out.data = Some(Arc::new(senders));
    }

    /// Also receive frames from these streams (the receiving side). Each ends with `StreamEnd`.
    pub fn attach_data_in(&mut self, streams: Vec<Box<dyn Read + Send>>) {
        let Some(rtx) = self.rtx.lock().unwrap().clone() else { return };
        for (i, s) in streams.into_iter().enumerate() {
            let rtx = rtx.clone();
            self.data_in += 1;
            std::thread::Builder::new()
                .name(format!("barqsync-data-{i}"))
                .spawn(move || {
                    let mut r = io::BufReader::with_capacity(256 << 10, s);
                    loop {
                        let f = match read_frame(&mut r) {
                            Ok(Some(f)) => f,
                            Ok(None) => {
                                let eof = io::Error::new(io::ErrorKind::UnexpectedEof, "data stream closed early");
                                let _ = rtx.send(Err(stream_lost(eof)));
                                return;
                            }
                            Err(e) => {
                                let _ = rtx.send(Err(stream_lost(e)));
                                return;
                            }
                        };
                        let end = matches!(f, Frame::Msg(Msg::StreamEnd));
                        if rtx.send(Ok(f)).is_err() || end {
                            return;
                        }
                    }
                })
                .expect("spawn data reader thread");
        }
    }

    /// The session this connection runs over, if it is a `T` (e.g. a QUIC session).
    pub fn session<T: 'static>(&self) -> Option<&T> {
        let session: &dyn std::any::Any = self.session.as_deref()?;
        session.downcast_ref()
    }

    /// Number of attached incoming data streams.
    pub fn data_streams_in(&self) -> usize {
        self.data_in
    }

    /// No more streams will be attached: from now on the peer closing the main stream
    /// is seen as a disconnect.
    pub fn seal(&mut self) {
        self.rtx.lock().unwrap().take();
    }

    /// How frames sent from now on are compressed (before handing out clones).
    pub fn set_compression(&mut self, comp: Compression) {
        self.out.comp = Arc::new(comp);
    }

    /// Encode messages sent from now on with `wire` (before handing out clones).
    pub fn set_wire(&mut self, wire: Wire) {
        self.out.wire = wire;
    }

    pub fn send(&self, frame: Frame) -> io::Result<()> {
        self.out.send(frame)
    }

    pub fn msg(&self, m: Msg) -> io::Result<()> {
        self.out.msg(m)
    }

    /// The next frame, waiting at most `wait` (`Ok(None)`: none yet), or why the
    /// connection ended.
    ///
    /// A peer that stops ends its data streams too, and their end can arrive before the
    /// reason it sends on the control stream (`Interrupted`, `Fatal`), which travels
    /// another path. So a data stream that fails or ends early, or a write to the peer
    /// that fails, ends the connection only once the control stream has had [`SETTLE`]
    /// to deliver that reason or its own end. File data arriving meanwhile is dropped:
    /// the transfer can't complete any more, and the frames behind it may be the reason.
    ///
    /// Over QUIC, the end of the ssh pipe underneath counts the same way: a peer that
    /// dies ends its ssh pipe at once, while QUIC would notice only after its idle
    /// timeout. Like rsync, which dies with its ssh, the transfer then stops even if the
    /// QUIC session itself is still up.
    pub fn recv_timeout(&self, wait: Duration) -> io::Result<Option<Frame>> {
        let lost = || self.lost.lock().unwrap_or_else(|e| e.into_inner());
        let settling = lost().as_ref().map(|&(_, until)| until);
        let wait = match settling {
            Some(until) if Instant::now() >= until => return Err(lost().take().expect("lost").0),
            Some(until) => wait.min(until.saturating_duration_since(Instant::now())),
            None => wait,
        };
        // The connection ended on the control stream: the first failure says how.
        let ended = |e: io::Error| lost().take().map_or(e, |(first, _)| first);
        match self.next(wait) {
            Ok(Ok(f)) if settling.is_some() && carries_file_data(&f) => Ok(None),
            Ok(Ok(f)) => Ok(Some(f)),
            Ok(Err(e)) if is_stream_lost(&e) => {
                lost().get_or_insert((e, Instant::now() + SETTLE));
                Ok(None)
            }
            Ok(Err(e)) => Err(ended(e)),
            // Nothing pending: a peer this side can no longer write to won't answer.
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if let Some(e) = self.out.write_failure() {
                    lost().get_or_insert((e, Instant::now() + SETTLE));
                }
                Ok(None)
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                Err(ended(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed by peer")))
            }
        }
    }

    /// The next item from this connection or, until it ends, from the ssh pipe underneath,
    /// waiting at most `wait`. The pipe's end comes as a [`stream_lost`] error, once.
    fn next(&self, wait: Duration) -> Result<io::Result<Frame>, crossbeam_channel::RecvTimeoutError> {
        let Some(under) = self.under.as_ref().filter(|_| !self.under_ended.load(Ordering::SeqCst)) else {
            return self.rx.recv_timeout(wait);
        };
        crossbeam_channel::select_biased! {
            recv(self.rx) -> r => r.map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected),
            recv(under.rx) -> r => {
                let e = match r {
                    // Nothing more is sent there once control has moved, but a server whose
                    // move failed says so there.
                    Ok(Ok(f)) => return Ok(Ok(f)),
                    Ok(Err(e)) => e,
                    Err(_) => io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed by peer"),
                };
                self.under_ended.store(true, Ordering::SeqCst);
                Ok(Err(stream_lost(e)))
            },
            default(wait) => Err(crossbeam_channel::RecvTimeoutError::Timeout),
        }
    }

    /// Receive the next frame; a closed stream is an error (callers expect a reply).
    pub fn recv(&self) -> io::Result<Frame> {
        match self.rx.recv() {
            Ok(r) => r,
            Err(_) => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed by peer")),
        }
    }

    /// Flush everything queued and close our side of the stream. Clones of the sender
    /// may still exist (e.g. in a forwarding `Output`), so closing is an explicit request.
    ///
    /// Over QUIC, everything this side sent must reach the peer's application before the
    /// session closes, and QUIC resends lost packets only while this process lives. So
    /// this waits (bounded) until the peer has ended too: the peer ends only once it has
    /// read all it needs. It ends its control stream or closes the session, or, when its
    /// QUIC close is lost or it exits first, the idle ssh pipe reaches its end. A run that
    /// stopped also gives up on a peer that has gone silent (see [`Conn::wait_for_peer`]).
    pub fn finish(self) -> io::Result<()> {
        self.close(false)
    }

    /// [`Conn::finish`] once the peer has said it has everything this side sent (the
    /// sender, after `ReceiverDone`; the receiver of a pull with `--remove-source-files`,
    /// after `RemovalsDone`): nothing left to deliver, so close without waiting.
    /// The peer, waiting for this side to end, learns a round trip sooner.
    pub fn finish_confirmed(self) -> io::Result<()> {
        self.close(true)
    }

    fn close(mut self, confirmed: bool) -> io::Result<()> {
        use crate::timing::mark;
        let quic = self.session.is_some();
        if quic && confirmed {
            // Stops the writers too: whatever they still hold, the peer no longer reads.
            drop(self.session.take());
            mark("finish: session closed");
        }
        let stopped = self.out.stopped();
        let start = Instant::now();
        // A stopped run waits for the peer only while it answers (see `wait_for_peer`).
        let heard = self.session.as_deref().filter(|_| stopped).map(|s| (s.received(), start));
        let deadline = start + if stopped { STOPPED_CLOSE_WAIT } else { CLOSE_WAIT };
        // Closing: a writer failing from now on (a peer that already left) is no news.
        self.out.cancel();
        // A run that stopped has told the peer why on the control stream (`Interrupted`,
        // `Fatal`): that goes out before the data streams end, which the peer would
        // otherwise see first.
        let control = stopped.then(|| self.close_control(deadline));
        for tx in self.out.data.iter().flat_map(|d| d.iter()) {
            tx.close(deadline);
        }
        for h in self.data_writers.drain(..) {
            let _ = h.join_until(deadline);
        }
        let r = control.unwrap_or_else(|| self.close_control(deadline));
        // The writers of a closed session fail, as expected.
        let r = if quic && confirmed { Ok(()) } else { r };
        if self.session.is_some() {
            mark("finish: streams ended");
            let how = self.wait_for_peer(Instant::now() + PEER_WAIT, heard);
            mark(format_args!("finish: peer ended ({how})"));
        }
        drop(self.session.take());
        if let Some(under) = self.under.take() {
            let _ = under.finish();
            mark("finish: ssh pipe closed");
        }
        r
    }

    /// Deliver what is queued on the control stream and end it.
    fn close_control(&mut self, deadline: Instant) -> io::Result<()> {
        self.out.tx.close(deadline);
        self.writer.take().map_or(Ok(()), |w| w.join_until(deadline))
    }

    /// Wait until the peer has ended: its streams on this connection, or the ssh pipe
    /// underneath. Frames still arriving are dropped; the caller is done reading.
    ///
    /// `heard`, for a run that stopped: the datagrams received from the peer when closing
    /// began, and when. The wait still lets QUIC resend why this side stopped
    /// (`Interrupted`, `Fatal`) until the peer has it, but nothing at all arriving from the
    /// peer for [`SILENT`] means there is nothing left to resend: a running peer
    /// acknowledges what is outstanding within a round trip, so it either has everything
    /// or it no longer runs (stopped, or gone without closing). The wait ends there
    /// instead of at the deadline.
    fn wait_for_peer(&self, deadline: Instant, mut heard: Option<(u64, Instant)>) -> &'static str {
        let under = self.under.as_ref().map(|u| &u.rx);
        let session = self.session.as_deref();
        loop {
            let mut until = deadline;
            if let (Some((count, since)), Some(s)) = (heard.as_mut(), session) {
                let now = Instant::now();
                let received = s.received();
                if received != *count {
                    (*count, *since) = (received, now);
                }
                if now >= *since + SILENT.max(3 * s.rtt()) {
                    return "silent";
                }
                until = until.min(now + Duration::from_millis(50));
            }
            let mut sel = crossbeam_channel::Select::new();
            let main = sel.recv(&self.rx);
            if let Some(u) = under {
                sel.recv(u);
            }
            let Ok(op) = sel.select_deadline(until) else {
                if Instant::now() >= deadline {
                    return "timed out";
                }
                continue;
            };
            if op.index() == main {
                if op.recv(&self.rx).is_err() {
                    return "quic";
                }
            } else if op.recv(under.expect("ssh pipe")).is_err() {
                return "ssh";
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(f: Frame, compress: bool) -> Frame {
        let buf = encode(&f, compress.then_some(1), Wire::Postcard).unwrap();
        read_frame(&mut &buf[..]).unwrap().unwrap()
    }

    #[test]
    fn data_frames_roundtrip_with_and_without_compression() {
        let bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 7) as u8).collect();
        for compress in [false, true] {
            match roundtrip(Frame::Data { id: file_id(3, 9), off: 12345, bytes: bytes.clone() }, compress) {
                Frame::Data { id, off, bytes: b } => {
                    assert_eq!((id, off), (file_id(3, 9), 12345));
                    assert_eq!(b, bytes);
                }
                other => panic!("{other:?}"),
            }
        }
        let buf = encode(&Frame::Data { id: 1, off: 0, bytes: bytes.clone() }, Some(1), Wire::Postcard).unwrap();
        assert!(buf.len() < bytes.len() / 10, "repetitive data should compress");
    }

    #[test]
    fn messages_roundtrip() {
        let m = Msg::Need { dir: 7, files: vec![NeedReq { idx: 2, sig: None }], xattrs: vec![4] };
        match roundtrip(Frame::Msg(m), true) {
            Frame::Msg(Msg::Need { dir, files, xattrs }) => {
                assert_eq!((dir, files.len(), files[0].idx, xattrs), (7, 1, 2, vec![4]))
            }
            other => panic!("{other:?}"),
        }
    }

    /// Messages with every awkward field type, for both wire formats.
    fn rich_messages() -> Vec<Msg> {
        use crate::entry::Kind;
        use std::os::unix::ffi::OsStringExt;
        let meta = Meta { kind: Kind::File, size: 7, mtime: -3, mtime_ns: 9, mode: 0o644, uid: 1, gid: 2, dev: 3, ino: 4, rdev: 0, nlink: 1, atime: 5, crtime: 6 };
        let name = std::ffi::OsString::from_vec(vec![b'a', 0xff, b'\n']);
        let mut sel = Sel { named: true, ..Sel::default() };
        sel.children.insert(b"sub".to_vec(), Sel { contents: true, ..Sel::default() });
        let opts = crate::cli::Options::parse_from(["barqsync", "-a", "--partial-dir=.p", "src/", "dst"]).unwrap();
        let sig = crate::delta::cdc::signature(&[5u8; 300_000], &crate::delta::Params::DEFAULT);
        let data: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let p = crate::delta::Params::for_size(data.len() as u64);
        let chunks = crate::delta::chunk_signature(&data, data.len() as u64, &crate::delta::Params::DEFAULT).unwrap();
        let reqs = vec![GapReq { off: 4_000, len: 70_000, chunks: true }, GapReq { off: 200_000, len: 9_000, chunks: false }];
        let fine = crate::delta::refine::basis_sigs(&data, data.len() as u64, &p, &reqs).unwrap();
        // -X/-A data with non-UTF-8 names, both value forms and both ACL flavors.
        use crate::acls::{AclName, Acls, Flavor, RsyncAcl};
        use crate::xattrs::XValue;
        let xattrs = vec![
            Xattr { name: vec![b'u', 0xfe, b'.'], value: XValue::Full(vec![0, 1, 0xff]) },
            Xattr { name: b"user.big".to_vec(), value: XValue::new(vec![9; 1000], false) },
        ];
        let acl = Acls {
            flavor: Flavor::Posix,
            access: RsyncAcl { group_obj: Some(5), mask_obj: Some(7), names: vec![AclName { id: 1000, user: true, access: 6 }], ..RsyncAcl::default() },
            default: RsyncAcl { user_obj: Some(7), group_obj: Some(5), mask_obj: None, other_obj: Some(0), names: Vec::new() },
        };
        let mac = Acls { flavor: Flavor::Darwin, access: RsyncAcl { names: vec![AclName { id: 20, user: false, access: 1 << 31 | 3 }], ..RsyncAcl::default() }, default: RsyncAcl::default() };
        let extra = crate::entry::Extra { xattrs: xattrs.clone(), acl: Some(acl) };
        let mut ent = DirEnt::new(name, meta, Some(b"t".to_vec()));
        ent.extra = Some(Box::new(extra.clone()));
        let plain = DirEnt::new(std::ffi::OsString::from("p"), meta, None);
        let mut mac_ent = plain.clone();
        mac_ent.extra = Some(Box::new(crate::entry::Extra { xattrs: Vec::new(), acl: Some(mac) }));
        vec![
            Msg::Hello { magic: MAGIC, version: VERSION, server_is_sender: false, opts: Box::new(opts) },
            Msg::Roots(vec![RootInfo { contents: true, meta: Some(meta), items: Vec::new(), sel: Some(sel), extra: Some(Box::new(extra)) }]),
            Msg::DirBatch { root: 1, dir: 2, rel: b"x/y".to_vec(), entries: vec![ent, plain, mac_ent], sums: vec![Some([1; 16])] },
            Msg::Need { dir: 2, files: vec![NeedReq { idx: 0, sig: Some(Sig::Cdc(sig)) }], xattrs: vec![1, 5] },
            Msg::Need { dir: 3, files: vec![NeedReq { idx: 1, sig: Some(Sig::Chunks(chunks)) }], xattrs: Vec::new() },
            Msg::Refine { id: file_id(3, 1), grain: Grain::of(&p), gaps: reqs },
            Msg::Refined { id: file_id(3, 1), gaps: fine },
            Msg::SmallPack { files: vec![(file_id(2, 0), 3)], blob: b"abc".to_vec(), xattrs: vec![(file_id(2, 0), xattrs.clone())] },
            Msg::FileBegin { id: 3, size: 9, xattrs: Some(xattrs.clone()) },
            Msg::Xattrs { id: file_id(2, 1), xattrs: Some(xattrs) },
            Msg::Xattrs { id: 5, xattrs: None },
            Msg::IoError,
            Msg::Interrupted,
        ]
    }

    #[test]
    fn rich_messages_roundtrip_in_both_wire_formats() {
        for wire in [Wire::Postcard, Wire::Rkyv] {
            for m in rich_messages() {
                let want = format!("{m:?}");
                let buf = encode(&Frame::Msg(m), None, wire).unwrap();
                match read_frame(&mut &buf[..]).unwrap().unwrap() {
                    Frame::Msg(got) => assert_eq!(format!("{got:?}"), want, "{wire:?}"),
                    other => panic!("{other:?}"),
                }
            }
        }
        // Hello stays postcard so any version can read it.
        let hello = rich_messages().remove(0);
        assert_eq!(encode(&Frame::Msg(hello), None, Wire::Rkyv).unwrap()[4], KIND_MSG);
    }

    #[test]
    fn corrupt_rkyv_messages_are_rejected() {
        for m in rich_messages().into_iter().skip(1) {
            let buf = encode(&Frame::Msg(m), None, Wire::Rkyv).unwrap();
            let body = buf.len() - 5;
            // Truncated archives, and archives with bytes flipped near the root (where
            // rkyv keeps relative pointers and lengths), must fail validation, not panic.
            // (A cut at an aligned offset can land on a valid shorter archive by chance,
            // so the long cut is odd; frames are length-prefixed on a reliable stream.)
            for cut in [1, 4, (body / 2) | 1] {
                let mut b = buf[..buf.len() - cut].to_vec();
                b[..4].copy_from_slice(&((body - cut) as u32).to_le_bytes());
                let got = read_frame(&mut &b[..]);
                assert!(got.is_err() || cut == 0, "cut {cut} of {body}: {got:?}");
            }
            for back in 1..=12.min(body) {
                let mut b = buf.clone();
                let i = b.len() - back;
                b[i] ^= 0xA5;
                let _ = read_frame(&mut &b[..]);
            }
        }
    }

    #[test]
    fn messages_larger_than_a_frame_go_in_pieces() {
        let bytes: Vec<u8> = (0..MAX_FRAME as u32 + (5 << 20)).map(|i| (i % 251) as u8).collect();
        for level in [None, Some(1)] {
            let buf = encode(&Frame::Data { id: 7, off: 9, bytes: bytes.clone() }, level, Wire::Postcard).unwrap();
            // Two frames: a part of at most MAX_FRAME, then the rest with the real kind.
            let first = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
            assert!(first <= MAX_FRAME && buf[4] & !FLAG_ZSTD == KIND_PART, "{level:?}");
            assert_eq!(buf[5 + first + 4] & !FLAG_ZSTD, KIND_DATA, "{level:?}");
            let mut r = &buf[..];
            match read_frame(&mut r).unwrap().unwrap() {
                Frame::Data { id, off, bytes: b } => assert!((id, off) == (7, 9) && b == bytes, "{level:?}"),
                other => panic!("{other:?}"),
            }
            assert!(read_frame(&mut r).unwrap().is_none());
            // A stream that ends after a part is cut short, not cleanly closed.
            assert_eq!(read_frame(&mut &buf[..5 + first]).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        }
    }

    /// `Interrupted` reaches a peer that is behind even with the control queue full: it
    /// goes out right after the frame being written, not after everything queued.
    #[test]
    fn the_interrupt_notice_overtakes_a_full_queue() {
        use std::os::unix::net::UnixStream;
        for tokio in [false, true] {
            let (ours, peer) = UnixStream::pair().unwrap();
            let conn = if tokio {
                let r = async_io::FdIo::new(ours.try_clone().unwrap().into()).unwrap();
                Conn::start_async(r, async_io::FdIo::new(ours.into()).unwrap(), false)
            } else {
                Conn::start(ours.try_clone().unwrap(), ours, false)
            };
            // The peer reads nothing yet: the writer blocks and the queue fills up.
            let frame = encode(&Frame::Data { id: 1, off: 0, bytes: vec![7; 1 << 20] }, None, Wire::Postcard).unwrap();
            while conn.out.tx.try_send(frame.clone()) {}
            conn.out.tell_interrupted();
            let mut r = io::BufReader::new(peer);
            for read in 0.. {
                assert!(read < 16, "tokio={tokio}: the notice waited behind the queue");
                match read_frame(&mut r).unwrap() {
                    Some(Frame::Msg(Msg::Interrupted)) => break,
                    Some(Frame::Data { .. }) => {}
                    other => panic!("tokio={tokio}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn clean_eof_and_garbage() {
        assert!(read_frame(&mut &[][..]).unwrap().is_none());
        assert!(read_frame(&mut &[1u8, 0][..]).is_err());
        let huge = [0xff, 0xff, 0xff, 0x7f, 0];
        assert!(read_frame(&mut &huge[..]).is_err());
    }

    #[test]
    fn ids_pack_and_unpack() {
        assert_eq!(split_id(file_id(u32::MAX, 5)), (u32::MAX, 5));
    }
}
