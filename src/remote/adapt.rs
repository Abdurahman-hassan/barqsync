//! Adaptive strategy (`--strategy adaptive`, the original plan's §4).
//!
//! Before the first listing the sender measures the link: a ping for the round-trip time,
//! then a burst of incompressible probe data over the same path file data will take (all
//! data streams), timed until the receiver acknowledges it. 256 KiB first; only when that
//! looks like 500 Mbit/s or more, another 4 MiB for an accurate figure, so a slow link pays
//! for a quarter megabyte and two round trips. The result goes to the receiver as
//! `LinkInfo` and drives:
//!
//! - **routing** (the receiver, per file): up to 128 KiB packed, never delta; 128 KiB to
//!   32 MiB whole on links of 1 Gbit/s or more (hashing both copies costs more than sending
//!   the file), delta below that; above 32 MiB delta;
//! - **compression** (the sender): small-file packs are compressed even without `-z`, at
//!   zstd level 1 from 1 Gbit/s, 3 from 100 Mbit/s, 6 below, stepped down while compressing
//!   can't keep up with the link.
//!
//! `-W` / `--no-whole-file` fix the routing and skip the probe; `FASTRSYNC_TEST_LINK_BPS`
//! replaces it (tests).

use super::{Stop, recv_until};
use crate::output::Output;
use crate::proto::{Conn, Frame, Msg};
use std::io;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// Probe data travels in `Data` frames with ids from here up (never a real file id: file
/// ids are `dir << 32 | index`, and no transfer has 4 billion directories).
pub const PROBE_ID: u64 = u64::MAX - 0xFFFF;

pub fn is_probe(id: u64) -> bool {
    id >= PROBE_ID
}

const PROBE_FRAME: usize = 64 << 10;
const PROBE_SMALL: u64 = 256 << 10;
const PROBE_LARGE: u64 = 4 << 20;
/// Above this (from the small probe) the large one runs.
const LARGE_FROM_BPS: u64 = 500_000_000;
pub const GBIT: u64 = 1_000_000_000;
/// Files up to this size are sent whole on a fast link.
pub const WHOLE_MAX: u64 = 32 << 20;

/// What the probe measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct LinkInfo {
    /// Bits per second.
    pub bps: u64,
    pub rtt_us: u64,
}

impl LinkInfo {
    /// zstd level for this link.
    pub fn zstd_level(&self) -> i32 {
        match self.bps {
            b if b >= GBIT => 1,
            b if b >= GBIT / 10 => 3,
            _ => 6,
        }
    }

    /// Routing: whether a file of `size` bytes with an old copy at the receiver should
    /// be sent as a delta.
    pub fn wants_delta(&self, size: u64) -> bool {
        size > WHOLE_MAX || self.bps < GBIT
    }
}

/// Sender side: measure the link and tell the receiver. Output the receiver forwards
/// meanwhile is printed.
pub(crate) fn probe(conn: &Conn, out: &Output) -> Result<LinkInfo, Stop> {
    let forced = std::env::var("FASTRSYNC_TEST_LINK_BPS").ok().and_then(|v| v.parse().ok());
    let link = match forced {
        Some(bps) => LinkInfo { bps, rtt_us: 0 },
        None => {
            let rtt = ping(conn, out)?;
            let small = burst(conn, out, PROBE_SMALL, rtt)?;
            let bps = if small >= LARGE_FROM_BPS { burst(conn, out, PROBE_LARGE, rtt)? } else { small };
            LinkInfo { bps, rtt_us: rtt.as_micros() as u64 }
        }
    };
    conn.msg(Msg::LinkInfo(link)).map_err(Stop::Io)?;
    if out.verbose() > 1 {
        out.info(format!(
            "link: {:.1} Mbit/s, {:.1} ms round trip",
            link.bps as f64 / 1e6,
            link.rtt_us as f64 / 1e3
        ));
    }
    Ok(link)
}

/// Longest wait for a probe reply (a 4 MiB burst at 1 Mbit/s takes ~34 s).
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// Wait for one reply, printing forwarded output.
fn reply(conn: &Conn, out: &Output, want: fn(&Msg) -> bool) -> Result<(), Stop> {
    let deadline = Instant::now() + REPLY_TIMEOUT;
    loop {
        match recv_until(conn, Some(deadline))? {
            Frame::Msg(m) if want(&m) => return Ok(()),
            Frame::Msg(Msg::Log { stderr, escape, text }) => out.print_forwarded(stderr, escape, &text),
            Frame::Msg(Msg::Fatal { msg }) => return Err(Stop::Io(io::Error::other(format!("remote: {msg}")))),
            other => return Err(Stop::Io(io::Error::new(io::ErrorKind::InvalidData, format!("probe: unexpected {other:?}")))),
        }
    }
}

fn ping(conn: &Conn, out: &Output) -> Result<Duration, Stop> {
    let t = Instant::now();
    conn.msg(Msg::Ping).map_err(Stop::Io)?;
    reply(conn, out, |m| matches!(m, Msg::Pong))?;
    Ok(t.elapsed())
}

/// Send `bytes` of probe data and time it until acknowledged; bits per second. The
/// frames spread over every data stream, so they can arrive in any order, before or after
/// control messages: each carries the burst's size (in `off`), and the receiver counts.
fn burst(conn: &Conn, out: &Output, bytes: u64, rtt: Duration) -> Result<u64, Stop> {
    let data = noise(PROBE_FRAME);
    let t = Instant::now();
    let mut sent = 0u64;
    let mut i = 0u64;
    while sent < bytes {
        let n = (bytes - sent).min(PROBE_FRAME as u64) as usize;
        conn.send(Frame::Data { id: PROBE_ID + (i % 0x1000), off: bytes, bytes: data[..n].to_vec() }).map_err(Stop::Io)?;
        sent += n as u64;
        i += 1;
    }
    reply(conn, out, |m| matches!(m, Msg::ProbeAck))?;
    // The acknowledgement adds one round trip to the transmission time.
    let secs = t.elapsed().saturating_sub(rtt).max(Duration::from_micros(100)).as_secs_f64();
    Ok((bytes as f64 * 8.0 / secs) as u64)
}

/// Incompressible bytes (compression must not flatter the link).
fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Receiver side of the probe: counts each burst's data and acknowledges it once all of
/// it has arrived.
#[derive(Default)]
pub struct ProbeReceiver {
    /// (burst size, bytes received so far)
    bursts: Vec<(u64, u64)>,
}

impl ProbeReceiver {
    /// Handle a probe frame; `true` if it was one (and was answered when due).
    pub fn handle(&mut self, conn: &Conn, f: &Frame) -> io::Result<bool> {
        match f {
            Frame::Msg(Msg::Ping) => conn.msg(Msg::Pong).map(|()| true),
            Frame::Data { id, off: size, bytes } if is_probe(*id) => {
                let i = match self.bursts.iter().position(|(s, _)| s == size) {
                    Some(i) => i,
                    None => {
                        self.bursts.push((*size, 0));
                        self.bursts.len() - 1
                    }
                };
                let got = &mut self.bursts[i].1;
                let before = *got;
                *got += bytes.len() as u64;
                if before < *size && *got >= *size {
                    conn.msg(Msg::ProbeAck)?;
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

/// How the sender compresses frames (`-z`, `--compress-level`, adaptive packs).
pub struct Compression {
    /// Every frame (`-z`).
    pub all: bool,
    /// Small-file packs (adaptive strategy, or `-z`).
    pub packs: bool,
    level: AtomicI32,
    /// Step the level down while compressing can't keep up with the link.
    auto: bool,
    /// Link speed in bytes per second (0: unknown).
    link_bytes: AtomicU64,
}

impl Compression {
    pub fn new(all: bool, packs: bool, level: Option<i32>) -> Compression {
        Compression {
            all,
            packs: packs || all,
            level: AtomicI32::new(level.unwrap_or(1)),
            auto: level.is_none(),
            link_bytes: AtomicU64::new(0),
        }
    }

    pub fn off() -> Compression {
        Compression::new(false, false, None)
    }

    /// The link is known: start from its level.
    pub fn set_link(&self, link: LinkInfo) {
        self.link_bytes.store(link.bps / 8, Relaxed);
        if self.auto {
            self.level.store(link.zstd_level(), Relaxed);
        }
    }

    /// Level for a frame (`None`: send it raw).
    pub fn level(&self, pack: bool) -> Option<i32> {
        (self.all || (self.packs && pack)).then(|| self.level.load(Relaxed))
    }

    /// One frame of `raw` bytes took `took` to compress: if all cores compressing at
    /// that rate would still fall behind the link, step the level down.
    pub fn observe(&self, raw: usize, took: Duration) {
        let link = self.link_bytes.load(Relaxed);
        if !self.auto || link == 0 || raw < 64 << 10 {
            return;
        }
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get()) as f64;
        let rate = raw as f64 / took.as_secs_f64().max(1e-6) * cores;
        if rate < link as f64 {
            let mut l = self.level.load(Relaxed);
            while l > 1 {
                match self.level.compare_exchange_weak(l, l - 1, Relaxed, Relaxed) {
                    Ok(_) => break,
                    Err(now) => l = now,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_and_levels_follow_the_link() {
        let fast = LinkInfo { bps: 10 * GBIT, rtt_us: 100 };
        let lan = LinkInfo { bps: GBIT / 2, rtt_us: 500 };
        let slow = LinkInfo { bps: 20_000_000, rtt_us: 80_000 };
        assert_eq!((fast.zstd_level(), lan.zstd_level(), slow.zstd_level()), (1, 3, 6));
        assert!(!fast.wants_delta(4 << 20) && fast.wants_delta(64 << 20));
        assert!(lan.wants_delta(4 << 20) && slow.wants_delta(200 << 10));
    }

    #[test]
    fn compression_steps_down_when_it_falls_behind() {
        let c = Compression::new(false, true, None);
        assert_eq!(c.level(false), None);
        c.set_link(LinkInfo { bps: 20_000_000, rtt_us: 0 });
        assert_eq!(c.level(true), Some(6));
        // 1 MiB in 10 s is slower than 2.5 MB/s on every core count.
        c.observe(1 << 20, Duration::from_secs(10));
        assert_eq!(c.level(true), Some(5));
        let fixed = Compression::new(true, false, Some(9));
        fixed.set_link(LinkInfo { bps: 20_000_000, rtt_us: 0 });
        fixed.observe(1 << 20, Duration::from_secs(10));
        assert_eq!(fixed.level(false), Some(9));
    }
}
