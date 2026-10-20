//! Wire protocol between a sender and a receiver over one duplex byte stream (the ssh
//! pipe). Frames are length-prefixed; control messages are postcard-encoded, file data
//! travels in raw `Data` frames. With `-z` each frame body is zstd-compressed.
//!
//! A transfer, after the handshake:
//!
//! ```text
//! sender                                   receiver
//!   Roots ------------------------------------>   plan destination
//!   DirBatch (one per directory, as scanned) ->   diff against local dirs
//!   ...                                  <-----   Need (one per DirBatch: files + basis signatures)
//!   SmallPack / FileBegin+Data+FileEnd /
//!   DeltaBegin+Data+FileEnd -------------------->   write files in parallel
//!   Xattrs (one per entry asked for in a Need) ->   write attribute values
//!   ScanDone, then SenderDone ----------------->
//!                                        <-----   ReceiverDone (stats)
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
use std::thread::JoinHandle;

pub const MAGIC: [u8; 8] = *b"BARQSYNC";
/// 9: `-X`/`-A` (attributes and ACLs in listings, roots and file data; `Xattrs`).
pub const VERSION: u32 = 9;
/// Upper bound on a decoded frame body; anything larger is a corrupt stream.
pub const MAX_FRAME: usize = 64 << 20;
/// File data is sent in pieces of this size.
pub const DATA_CHUNK: usize = 1 << 20;
/// Files up to this size are packed many-per-frame instead of streamed one by one.
pub const SMALL_FILE: u64 = 128 << 10;
/// Target size of a small-file pack.
pub const PACK_BYTES: usize = 4 << 20;

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

/// Protocol 9 acknowledgement payload. The sender uses `rel` only as a ledger key;
/// peer-supplied `size` and `mtime` are retained for compatibility, not authority.
pub type SyncedFiles = Vec<(Vec<u8>, u64, i64)>;

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
    SenderDone { errors: u64 },

    // receiver → sender
    /// The answer to one `DirBatch`: which of its files the receiver wants, and (`-X`)
    /// which entries' attribute values it needs in full to update them in place.
    Need { dir: u32, files: Vec<NeedReq>, xattrs: Vec<u32> },
    /// `--remove-source-files`: these entries are on the receiver now. `(rel, size, mtime)`
    /// relative to the root. The sender validates its own ledger and defers removal
    /// until successful session completion.
    Synced { root: u16, files: SyncedFiles },
    ReceiverDone { stats: StatsSnapshot },
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
const FLAG_ZSTD: u8 = 0x80;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Encode a frame: `u32 LE body length`, `u8 kind`, body. Compressed bodies are
/// `u32 LE raw length` + zstd stream. `Hello` is always postcard, so a peer of any
/// version can read it before the session's [`Wire`] is agreed.
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
    let (kind, body) = if let Some(level) = level.filter(|_| body.len() >= 256) {
        let z = zstd::bulk::compress(&body, level)?;
        if z.len() + 4 < body.len() {
            let mut b = Vec::with_capacity(4 + z.len());
            b.extend_from_slice(&(body.len() as u32).to_le_bytes());
            b.extend_from_slice(&z);
            (kind | FLAG_ZSTD, b)
        } else {
            (kind, body)
        }
    } else {
        (kind, body)
    };
    if body.len() > MAX_FRAME {
        return Err(invalid("frame too large"));
    }
    let mut out = Vec::with_capacity(5 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Read one frame. `Ok(None)` on a clean end of stream between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut hdr = [0u8; 5];
    let mut got = 0;
    while got < hdr.len() {
        match r.read(&mut hdr[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let mut body = vec![0u8; body_len(&hdr)?];
    r.read_exact(&mut body)?;
    decode_body(hdr[4], body).map(Some)
}

/// Body length from a frame header, checked against [`MAX_FRAME`].
pub(crate) fn body_len(hdr: &[u8; 5]) -> io::Result<usize> {
    let len = u32::from_le_bytes(hdr[..4].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(invalid(format!("frame of {len} bytes exceeds the limit")));
    }
    Ok(len)
}

/// Decode a frame body of the given kind.
pub(crate) fn decode_body(kind: u8, mut body: Vec<u8>) -> io::Result<Frame> {
    if kind & FLAG_ZSTD != 0 {
        let raw = u32::from_le_bytes(body.get(..4).ok_or_else(|| invalid("short frame"))?.try_into().unwrap());
        if raw as usize > MAX_FRAME {
            return Err(invalid("compressed frame too large"));
        }
        body = zstd::bulk::decompress(&body[4..], raw as usize)?;
    }
    match kind & !FLAG_ZSTD {
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

/// Where encoded frames are queued: a writer thread's channel, or a writer task's.
#[derive(Clone)]
enum Tx {
    Thread(Sender<Vec<u8>>),
    Task(tokio::sync::mpsc::Sender<Vec<u8>>),
}

impl Tx {
    fn send(&self, buf: Vec<u8>) -> io::Result<()> {
        let r = match self {
            Tx::Thread(t) => t.send(buf).map_err(drop),
            Tx::Task(t) => t.blocking_send(buf).map_err(drop),
        };
        r.map_err(|()| io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"))
    }
}

/// A writer thread or task, joined when the connection finishes.
enum Joiner {
    Thread(JoinHandle<io::Result<()>>),
    Task(tokio::task::JoinHandle<io::Result<()>>),
}

impl Joiner {
    fn join(self) -> io::Result<()> {
        match self {
            Joiner::Thread(h) => h.join().unwrap_or_else(|_| Err(io::Error::other("writer thread panicked"))),
            Joiner::Task(h) => async_io::rt().block_on(h).unwrap_or_else(|e| Err(io::Error::other(e))),
        }
    }
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
    /// Encode and queue a frame. Fails when the connection is gone.
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
        tx.send(buf)
    }

    /// Close every data stream with `StreamEnd`, after all file data has been queued.
    pub fn end_data_streams(&self) -> io::Result<()> {
        for tx in self.data.iter().flat_map(|d| d.iter()) {
            let buf = encode(&Frame::Msg(Msg::StreamEnd), None, self.wire)?;
            tx.send(buf)?;
        }
        Ok(())
    }

    pub fn msg(&self, m: Msg) -> io::Result<()> {
        self.send(Frame::Msg(m))
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
    session: Option<Box<dyn std::any::Any + Send + Sync>>,
    /// The connection this one replaced (the ssh pipe, once control moved to QUIC):
    /// idle, but kept open until this one finishes.
    under: Option<Box<Conn>>,
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

fn spawn_writer(name: String, writer: impl Write + Send + 'static, wrx: Receiver<Vec<u8>>) -> JoinHandle<io::Result<()>> {
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let mut w = BufWriter::with_capacity(256 << 10, writer);
            // An empty buffer is the close request from `Conn::finish`.
            while let Ok(buf) = wrx.recv() {
                if buf.is_empty() {
                    break;
                }
                w.write_all(&buf)?;
                if wrx.is_empty() {
                    w.flush()?;
                }
            }
            w.flush()
        })
        .expect("spawn writer thread")
}

impl Conn {
    pub fn start(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static, compress: bool) -> Conn {
        // Frames are up to ~4 MiB, so these bound memory to a few tens of MiB per side.
        let (tx, wrx) = bounded::<Vec<u8>>(16);
        let (rtx, rx) = bounded::<io::Result<Frame>>(16);
        let writer = spawn_writer("barqsync-writer".into(), writer, wrx);
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
                            if rtx.send(Ok(f)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => return,
                        Err(e) => {
                            let _ = rtx.send(Err(e));
                            return;
                        }
                    }
                }
            })
            .expect("spawn reader thread");
        Conn {
            out: FrameSender { tx: Tx::Thread(tx), data: None, comp: Arc::new(Compression::new(compress, false, None)), wire: Wire::Postcard },
            rx,
            rtx: spare,
            writer: Some(Joiner::Thread(writer)),
            data_writers: Vec::new(),
            data_in: 0,
            session: None,
            under: None,
        }
    }

    /// [`Conn::start`] with tokio tasks instead of threads (`BARQSYNC_NET_IO=tokio`).
    pub fn start_async(
        reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
        writer: impl tokio::io::AsyncWrite + Unpin + Send + 'static,
        compress: bool,
    ) -> Conn {
        let (tx, task) = async_io::spawn_writer(writer, 16);
        let (rtx, rx) = bounded::<io::Result<Frame>>(16);
        let spare = Arc::new(std::sync::Mutex::new(Some(rtx.clone())));
        let clear = ClearOnDrop(spare.clone());
        async_io::spawn_reader(reader, rtx, false, Some(Box::new(move || drop(clear))));
        Conn {
            out: FrameSender { tx: Tx::Task(tx), data: None, comp: Arc::new(Compression::new(compress, false, None)), wire: Wire::Postcard },
            rx,
            rtx: spare,
            writer: Some(Joiner::Task(task)),
            data_writers: Vec::new(),
            data_in: 0,
            session: None,
            under: None,
        }
    }

    /// [`Conn::attach_data_out`] for tokio streams.
    pub fn attach_data_out_async(&mut self, streams: Vec<Box<dyn tokio::io::AsyncWrite + Unpin + Send>>) {
        let mut senders = Vec::with_capacity(streams.len());
        for s in streams {
            let (tx, task) = async_io::spawn_writer(s, 4);
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
    pub fn switch(&mut self, reader: impl Read + Send + 'static, writer: impl Write + Send + 'static, session: Box<dyn std::any::Any + Send + Sync>) {
        let mut new = Conn::start(reader, writer, false);
        new.out.wire = self.out.wire;
        new.out.comp = self.out.comp.clone();
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
            self.data_writers.push(Joiner::Thread(spawn_writer(format!("barqsync-data-{i}"), s, wrx)));
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
                                let _ = rtx.send(Err(eof));
                                return;
                            }
                            Err(e) => {
                                let _ = rtx.send(Err(e));
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

    /// Receive the next frame; a closed stream is an error (callers expect a reply).
    pub fn recv(&self) -> io::Result<Frame> {
        match self.rx.recv() {
            Ok(r) => r,
            Err(_) => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed by peer")),
        }
    }

    /// Flush everything queued and close our side of the stream. Clones of the sender
    /// may still exist (e.g. in a forwarding `Output`), so closing is an explicit request.
    pub fn finish(mut self) -> io::Result<()> {
        for tx in self.out.data.iter().flat_map(|d| d.iter()) {
            let _ = tx.send(Vec::new());
        }
        for h in self.data_writers.drain(..) {
            let _ = h.join();
        }
        let _ = self.out.tx.send(Vec::new());
        let r = self.writer.take().map_or(Ok(()), Joiner::join);
        if self.session.is_some() {
            // QUIC: wait (bounded) for the peer to end its control stream too. Once both
            // sides have read each other's end, closing can't cut anything off, and
            // neither waits on a close notice that a process exiting might never send.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while self.rx.recv_deadline(deadline).is_ok() {}
        }
        drop(self.session.take());
        if let Some(under) = self.under.take() {
            let _ = under.finish();
        }
        r
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
        let meta = Meta { kind: Kind::File, size: 7, mtime: -3, mtime_ns: 9, mode: 0o644, uid: 1, gid: 2, dev: 3, ino: 4, rdev: 0, nlink: 1 };
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
