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
//!   ScanDone, then SenderDone ----------------->
//!                                        <-----   ReceiverDone (stats)
//! ```
//!
//! Each side runs a dedicated reader thread and writer thread, so neither side can
//! deadlock waiting for the other to drain the pipe.

use crate::cli::Options;
use crate::delta::{BlockSig, ChunkSig, Op};
use crate::entry::{DirEnt, Meta};
use crate::output::StatsSnapshot;
use crossbeam_channel::{Receiver, Sender, bounded};
use serde::{Deserialize, Serialize};
use std::io::{self, BufWriter, Read, Write};
use std::thread::JoinHandle;

pub const MAGIC: [u8; 8] = *b"FASTRSYN";
pub const VERSION: u32 = 1;
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
pub struct RootInfo {
    /// `src/`: the directory's contents are transferred (its own metadata is in `meta`).
    /// Otherwise the entries in `items` are transferred by name.
    pub contents: bool,
    pub meta: Option<Meta>,
    pub items: Vec<DirEnt>,
}

/// The receiver's basis-file signature, chosen by the receiver: aligned blocks when the
/// file kept its size (in-place edits), content-defined chunks when it changed size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Sig {
    Blocks(BlockSig),
    Chunks(ChunkSig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeedReq {
    pub idx: u32,
    pub sig: Option<Sig>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Msg {
    /// Client → server, first frame. `opts.paths` holds the server-side paths.
    Hello { magic: [u8; 8], version: u32, server_is_sender: bool, opts: Box<Options> },
    HelloAck { version: u32 },
    /// The peer is giving up; the message says why.
    Fatal { msg: String },
    /// Output produced on the server, printed by the client. `escape`: the text may
    /// contain file names and is escaped with the client's charset before printing.
    Log { stderr: bool, escape: bool, text: Vec<u8> },

    // sender → receiver
    Roots(Vec<RootInfo>),
    DirBatch { root: u16, dir: u32, rel: Vec<u8>, entries: Vec<DirEnt> },
    ScanDone { dirs: u32 },
    FileBegin { id: u64, size: u64 },
    /// Start a delta-encoded file: `copies` come from the receiver's basis, the
    /// remaining ranges arrive as `Data` frames.
    DeltaBegin { id: u64, size: u64, copies: Vec<Op> },
    FileEnd { id: u64, ok: bool },
    /// Several complete small files: `(id, len)` pairs, contents concatenated in `blob`.
    SmallPack { files: Vec<(u64, u32)>, blob: Vec<u8> },
    SenderDone { errors: u64 },

    // receiver → sender
    /// The answer to one `DirBatch`: which of its files the receiver wants.
    Need { dir: u32, files: Vec<NeedReq> },
    ReceiverDone { stats: StatsSnapshot },
}

#[derive(Debug)]
pub enum Frame {
    Msg(Msg),
    Data { id: u64, off: u64, bytes: Vec<u8> },
}

const KIND_MSG: u8 = 0;
const KIND_DATA: u8 = 1;
const FLAG_ZSTD: u8 = 0x80;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Encode a frame: `u32 LE body length`, `u8 kind`, body. Compressed bodies are
/// `u32 LE raw length` + zstd stream.
pub fn encode(frame: &Frame, compress: bool) -> io::Result<Vec<u8>> {
    let (kind, body) = match frame {
        Frame::Msg(m) => (KIND_MSG, postcard::to_stdvec(m).map_err(|e| invalid(e.to_string()))?),
        Frame::Data { id, off, bytes } => {
            let mut b = Vec::with_capacity(16 + bytes.len());
            b.extend_from_slice(&id.to_le_bytes());
            b.extend_from_slice(&off.to_le_bytes());
            b.extend_from_slice(bytes);
            (KIND_DATA, b)
        }
    };
    let (kind, body) = if compress && body.len() >= 256 {
        let z = zstd::bulk::compress(&body, 1)?;
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
    let len = u32::from_le_bytes(hdr[..4].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(invalid(format!("frame of {len} bytes exceeds the limit")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let kind = hdr[4];
    if kind & FLAG_ZSTD != 0 {
        let raw = u32::from_le_bytes(body.get(..4).ok_or_else(|| invalid("short frame"))?.try_into().unwrap());
        if raw as usize > MAX_FRAME {
            return Err(invalid("compressed frame too large"));
        }
        body = zstd::bulk::decompress(&body[4..], raw as usize)?;
    }
    match kind & !FLAG_ZSTD {
        KIND_MSG => Ok(Some(Frame::Msg(postcard::from_bytes(&body).map_err(|e| invalid(e.to_string()))?))),
        KIND_DATA => {
            if body.len() < 16 {
                return Err(invalid("short data frame"));
            }
            let id = u64::from_le_bytes(body[..8].try_into().unwrap());
            let off = u64::from_le_bytes(body[8..16].try_into().unwrap());
            body.drain(..16);
            Ok(Some(Frame::Data { id, off, bytes: body }))
        }
        k => Err(invalid(format!("unknown frame kind {k}"))),
    }
}

/// Cloneable handle for sending frames. Frames are encoded (and compressed) on the
/// calling thread, so many tasks can compress in parallel; the writer thread only writes.
#[derive(Clone)]
pub struct FrameSender {
    tx: Sender<Vec<u8>>,
    compress: bool,
}

impl FrameSender {
    /// Encode and queue a frame. Fails when the connection is gone.
    pub fn send(&self, frame: Frame) -> io::Result<()> {
        let buf = encode(&frame, self.compress)?;
        self.tx.send(buf).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"))
    }

    pub fn msg(&self, m: Msg) -> io::Result<()> {
        self.send(Frame::Msg(m))
    }
}

/// A running connection: a dedicated writer thread drains queued frames into the
/// stream, a dedicated reader thread decodes incoming frames.
pub struct Conn {
    pub out: FrameSender,
    pub rx: Receiver<io::Result<Frame>>,
    writer: Option<JoinHandle<io::Result<()>>>,
}

impl Conn {
    pub fn start(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static, compress: bool) -> Conn {
        // Frames are up to ~4 MiB, so these bound memory to a few tens of MiB per side.
        let (tx, wrx) = bounded::<Vec<u8>>(16);
        let (rtx, rx) = bounded::<io::Result<Frame>>(16);
        let writer = std::thread::Builder::new()
            .name("fastrsync-writer".into())
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
            .expect("spawn writer thread");
        std::thread::Builder::new()
            .name("fastrsync-reader".into())
            .spawn(move || {
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
        Conn { out: FrameSender { tx, compress }, rx, writer: Some(writer) }
    }

    /// Turn compression on or off for frames sent from now on (before handing out clones).
    pub fn set_compress(&mut self, on: bool) {
        self.out.compress = on;
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
        let _ = self.out.tx.send(Vec::new());
        match self.writer.take() {
            Some(h) => h.join().unwrap_or_else(|_| Err(io::Error::other("writer thread panicked"))),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(f: Frame, compress: bool) -> Frame {
        let buf = encode(&f, compress).unwrap();
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
        let buf = encode(&Frame::Data { id: 1, off: 0, bytes: bytes.clone() }, true).unwrap();
        assert!(buf.len() < bytes.len() / 10, "repetitive data should compress");
    }

    #[test]
    fn messages_roundtrip() {
        let m = Msg::Need { dir: 7, files: vec![NeedReq { idx: 2, sig: None }] };
        match roundtrip(Frame::Msg(m), true) {
            Frame::Msg(Msg::Need { dir, files }) => assert_eq!((dir, files.len(), files[0].idx), (7, 1, 2)),
            other => panic!("{other:?}"),
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
