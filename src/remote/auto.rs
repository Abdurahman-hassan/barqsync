//! `--transport auto`: QUIC where it pays, the ssh pipe elsewhere.
//!
//! QUIC is 2–4× faster than the ssh pipe on long, lossy links, ties it on a clean WAN and
//! is a few percent slower on a LAN; where UDP is blocked, trying it costs time. So, right
//! after the handshake and before the first listing:
//!
//! 1. the server opens its QUIC endpoint and sends the offer, as for `--transport quic`
//!    (the server is told `quic`, so a server of an older protocol version can still read
//!    the handshake and say it is one);
//! 2. the client pings the server over ssh and starts connecting to QUIC at the same time,
//!    so on a long link the handshake is done about when the pong arrives. A round trip
//!    under [`QUIC_FROM_RTT`] means a short link: the attempt is dropped and the transfer
//!    stays on the ssh pipe;
//! 3. otherwise the client waits for QUIC until [`deadline`] (300 ms plus two round trips
//!    from the ping). If it connected and its own handshake measured a long round trip
//!    too, the transfer moves to QUIC; if not, it stays on ssh.
//!
//! A host where QUIC didn't connect is remembered for [`FORGET_AFTER`] (see [`Memory`]),
//! and not tried again meanwhile.
//!
//! Loss can't be measured up front: the handshake is a handful of packets, too few to see
//! a 1% loss rate, and a burst big enough to would cost more on a clean WAN than QUIC saves
//! there. A long round trip is the signal instead. (A ping that ssh had to resend looks
//! long; the QUIC handshake's own measurement catches that.) But a QUIC transfer counts its
//! losses, so afterwards the client knows (see [`verdict`]): a host where QUIC lost nothing
//! in enough packets, at a rate the ssh pipe's window can carry, is remembered as clean
//! for [`FORGET_AFTER`], and transfers to it stay on the ssh pipe meanwhile. That saves
//! QUIC's setup and close (about two round trips) where QUIC has no loss to recover from.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Round trips from this long on make QUIC worth trying.
pub const QUIC_FROM_RTT: Duration = Duration::from_millis(30);
/// Time for a QUIC handshake beyond its two round trips (crypto, scheduling, one lost packet).
pub const SLACK: Duration = Duration::from_millis(300);
/// How long a host where QUIC failed, or saw a clean link, is left on the ssh pipe
/// without trying again.
pub const FORGET_AFTER: Duration = Duration::from_secs(3600);
/// Packets a QUIC transfer must have sent for "no loss" to mean a clean link: at the
/// lossy profile's 0.5% a link this sample shows none of its losses about once in 150.
pub const SAMPLE: u64 = 1000;
/// The ssh pipe moves at most its channel window per round trip (OpenSSH: 2 MiB). A link
/// that QUIC drove at three quarters of that may be faster than the pipe can use.
pub const SSH_WINDOW: u64 = 2 << 20;

/// The round trip from which QUIC is tried (`BARQSYNC_AUTO_RTT_MS` overrides it, for tests
/// and experiments).
pub fn threshold() -> Duration {
    threshold_from(std::env::var("BARQSYNC_AUTO_RTT_MS").ok().as_deref())
}

fn threshold_from(var: Option<&str>) -> Duration {
    var.and_then(|v| v.trim().parse().ok()).map_or(QUIC_FROM_RTT, Duration::from_millis)
}

/// Whether a link with this round trip should get QUIC.
pub fn wants_quic(rtt: Duration, threshold: Duration) -> bool {
    rtt >= threshold
}

/// How long a QUIC attempt may take on a link with this round trip, counted from its start
/// (with the ping): the handshake takes a round trip; another and the slack cover crypto,
/// scheduling and one lost packet (sent again after about 150 ms by either side, both
/// assuming a 50 ms round trip until they have measured one).
pub fn deadline(rtt: Duration) -> Duration {
    SLACK + 2 * rtt
}

/// What this side of a QUIC session saw of the link.
#[derive(Debug, Clone, Copy, Default)]
pub struct Seen {
    /// Stream data this side sent, in packets (STREAM frames, near enough). Packets that
    /// only acknowledge are never declared lost, so they don't count: a pull, where the
    /// client mostly acknowledges, rarely says anything.
    pub sent_packets: u64,
    /// Packets declared lost (only ones that needed an acknowledgement).
    pub lost_packets: u64,
    pub sent_bytes: u64,
    pub min_rtt: Duration,
    /// How long the session was open.
    pub open: Duration,
}

/// What a QUIC transfer says about the link it ran on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No loss in [`SAMPLE`] packets or more, at a rate the ssh pipe can carry.
    Clean,
    /// Losses: QUIC recovers from them better than the ssh pipe's TCP.
    Lossy,
    /// No loss, but faster than the ssh pipe's window allows on this round trip.
    Fast,
}

/// The verdict on a transfer, or none when it sent too little to tell.
pub fn verdict(seen: &Seen) -> Option<Verdict> {
    if seen.lost_packets > 0 {
        return Some(Verdict::Lossy);
    }
    if seen.sent_packets < SAMPLE {
        return None;
    }
    // Bytes per round trip, averaged over the session (idle time included, so a short
    // burst can look slower than it was).
    let per_rtt = seen.sent_bytes as f64 * seen.min_rtt.as_secs_f64() / seen.open.as_secs_f64().max(1e-3);
    Some(if per_rtt >= 0.75 * SSH_WINDOW as f64 { Verdict::Fast } else { Verdict::Clean })
}

/// What `auto` chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A short link (or no QUIC on the server): the ssh pipe.
    Ssh,
    Quic,
    /// QUIC was wanted but didn't connect in time.
    Unreachable,
}

/// Hosts where QUIC didn't connect lately, or found a clean link: one empty file per host
/// and [`Mark`] whose modification time says when. Kept in `$XDG_RUNTIME_DIR` when there
/// is one (gone at logout), else the user's cache directory; `BARQSYNC_CACHE_DIR`
/// overrides it. Unusable directories just mean nothing is remembered.
pub struct Memory {
    dir: Option<PathBuf>,
}

/// What [`Memory`] keeps per host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// QUIC didn't connect.
    Unreachable,
    /// QUIC's last transfer lost nothing (see [`verdict`]).
    Clean,
}

impl Mark {
    fn prefix(self) -> &'static str {
        match self {
            Mark::Unreachable => "quic-unreachable",
            Mark::Clean => "quic-clean",
        }
    }
}

impl Memory {
    pub fn new() -> Memory {
        let env = |k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let dir = env("BARQSYNC_CACHE_DIR")
            .or_else(|| env("XDG_RUNTIME_DIR").map(|d| d.join("barqsync")))
            .or_else(|| env("XDG_CACHE_HOME").map(|d| d.join("barqsync")))
            .or_else(|| {
                let home = env("HOME")?;
                Some(if cfg!(target_os = "macos") { home.join("Library/Caches/barqsync") } else { home.join(".cache/barqsync") })
            });
        Memory { dir }
    }

    pub fn at(dir: PathBuf) -> Memory {
        Memory { dir: Some(dir) }
    }

    /// One file per host (without `user@`: the link is the host's) and mark, named by hash.
    fn file(&self, mark: Mark, host: &str) -> Option<PathBuf> {
        let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
        let hash = blake3::hash(host.as_bytes()).to_hex();
        Some(self.dir.as_ref()?.join(format!("{}-{}", mark.prefix(), &hash[..32])))
    }

    /// Whether `host` got `mark` within [`FORGET_AFTER`].
    pub fn has(&self, mark: Mark, host: &str) -> bool {
        self.has_at(mark, host, SystemTime::now())
    }

    fn has_at(&self, mark: Mark, host: &str, now: SystemTime) -> bool {
        let Some(f) = self.file(mark, host) else { return false };
        let Ok(when) = fs::metadata(f).and_then(|m| m.modified()) else { return false };
        now.duration_since(when).is_ok_and(|age| age < FORGET_AFTER)
    }

    pub fn remember(&self, mark: Mark, host: &str) {
        let (Some(dir), Some(f)) = (&self.dir, self.file(mark, host)) else { return };
        let _ = fs::create_dir_all(dir).and_then(|()| fs::write(f, b""));
    }

    pub fn forget(&self, mark: Mark, host: &str) {
        if let Some(f) = self.file(mark, host) {
            let _ = fs::remove_file(f);
        }
    }
}

impl Default for Memory {
    fn default() -> Memory {
        Memory::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn long_round_trips_get_quic() {
        // Measured: lan 0.2 ms and wan 20 ms stay on ssh, lossy and far 80 ms go to QUIC.
        assert!(!wants_quic(Duration::from_micros(200), QUIC_FROM_RTT));
        assert!(!wants_quic(ms(20), QUIC_FROM_RTT));
        assert!(wants_quic(QUIC_FROM_RTT, QUIC_FROM_RTT));
        assert!(wants_quic(ms(80), QUIC_FROM_RTT));
        assert!(wants_quic(Duration::ZERO, Duration::ZERO));
    }

    #[test]
    fn the_deadline_allows_two_round_trips_and_a_retransmission() {
        assert_eq!(deadline(ms(80)), ms(460));
        assert_eq!(deadline(Duration::ZERO), SLACK);
        // Blocked UDP costs far less than --transport quic's 3 s on any link that gets
        // QUIC, up to satellite round trips.
        assert!(ms(600) + deadline(ms(600)) < super::super::quic::CONNECT_TIMEOUT);
        // One lost handshake packet on a lossy 80 ms link is sent again in time: the
        // client's Initial after three of its 50 ms guesses, the server's first flight
        // (half a round trip in) likewise.
        let guess = 3 * super::super::quic::INITIAL_RTT;
        for resend in [guess, ms(40) + guess] {
            assert!(resend + ms(80) < deadline(ms(80)), "{resend:?}");
        }
    }

    #[test]
    fn the_rtt_threshold_can_be_overridden() {
        assert_eq!(threshold_from(None), QUIC_FROM_RTT);
        assert_eq!(threshold_from(Some("0")), Duration::ZERO);
        assert_eq!(threshold_from(Some("150")), ms(150));
        assert_eq!(threshold_from(Some("fast")), QUIC_FROM_RTT);
    }

    #[test]
    fn memory_remembers_per_host_and_forgets() {
        use Mark::*;
        let tmp = tempfile::tempdir().unwrap();
        let m = Memory::at(tmp.path().join("state"));
        assert!(!m.has(Unreachable, "me@far"));
        m.remember(Unreachable, "me@far");
        assert!(m.has(Unreachable, "me@far"));
        assert!(m.has(Unreachable, "far"), "the user name doesn't matter");
        assert!(m.has(Unreachable, "other@far"));
        assert!(!m.has(Unreachable, "near"));
        assert!(!m.has(Clean, "far"), "marks are separate");
        m.remember(Clean, "far");
        assert!(m.has(Clean, "me@far"));
        let later = SystemTime::now() + FORGET_AFTER + Duration::from_secs(1);
        assert!(!m.has_at(Unreachable, "far", later), "expired");
        assert!(!m.has_at(Clean, "far", later), "expired");
        m.forget(Unreachable, "far");
        assert!(!m.has(Unreachable, "me@far"));
        assert!(m.has(Clean, "far"));
        // Nowhere to keep it: nothing remembered, nothing fails.
        let none = Memory { dir: None };
        none.remember(Unreachable, "far");
        assert!(!none.has(Unreachable, "far"));
        std::fs::write(tmp.path().join("file"), b"").unwrap();
        let unusable = Memory::at(tmp.path().join("file/state"));
        unusable.remember(Clean, "far");
        assert!(!unusable.has(Clean, "far"));
    }

    #[test]
    fn verdicts_need_enough_packets_and_respect_the_ssh_window() {
        let rtt = ms(80);
        let seen = |sent_packets, lost_packets, sent_bytes, open| Seen { sent_packets, lost_packets, sent_bytes, min_rtt: rtt, open };
        // Any loss: lossy, however little was sent.
        assert_eq!(verdict(&seen(50, 1, 60_000, ms(500))), Some(Verdict::Lossy));
        assert_eq!(verdict(&seen(100_000, 3, 140 << 20, Duration::from_secs(20))), Some(Verdict::Lossy));
        // No loss in too few packets says nothing.
        assert_eq!(verdict(&seen(SAMPLE - 1, 0, 1 << 20, ms(800))), None);
        // A 128 MiB transfer at 100 Mbit (13 MB/s, 1 MB per 80 ms): the pipe keeps up.
        assert_eq!(verdict(&seen(100_000, 0, 128 << 20, Duration::from_secs(11))), Some(Verdict::Clean));
        // The same at 1 Gbit: more per round trip than the pipe's window.
        assert_eq!(verdict(&seen(100_000, 0, 128 << 20, ms(1100))), Some(Verdict::Fast));
    }
}
