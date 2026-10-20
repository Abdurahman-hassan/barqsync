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
//! Loss isn't measured: the handshake is a handful of packets, too few to see a 1% loss
//! rate, and a burst big enough to would cost more on a clean WAN than QUIC saves there.
//! A long round trip is the signal instead. (A ping that ssh had to resend looks long; the
//! QUIC handshake's own measurement catches that.)

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Round trips from this long on make QUIC worth trying.
pub const QUIC_FROM_RTT: Duration = Duration::from_millis(30);
/// Time for a QUIC handshake beyond its two round trips (crypto, scheduling, one lost packet).
pub const SLACK: Duration = Duration::from_millis(300);
/// How long a host where QUIC failed is left on the ssh pipe without trying again.
pub const FORGET_AFTER: Duration = Duration::from_secs(3600);

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

/// What `auto` chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A short link (or no QUIC on the server): the ssh pipe.
    Ssh,
    Quic,
    /// QUIC was wanted but didn't connect in time.
    Unreachable,
}

/// Hosts where QUIC didn't connect lately, one empty file per host whose modification
/// time says when. Kept in `$XDG_RUNTIME_DIR` when there is one (gone at logout),
/// else the user's cache directory; `BARQSYNC_CACHE_DIR` overrides it. Unusable
/// directories just mean nothing is remembered.
pub struct Memory {
    dir: Option<PathBuf>,
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

    /// One file per host (without `user@`: reachability is the host's), named by hash.
    fn file(&self, host: &str) -> Option<PathBuf> {
        let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
        let hash = blake3::hash(host.as_bytes()).to_hex();
        Some(self.dir.as_ref()?.join(format!("quic-unreachable-{}", &hash[..32])))
    }

    /// Whether QUIC failed to `host` within [`FORGET_AFTER`].
    pub fn unreachable(&self, host: &str) -> bool {
        self.unreachable_at(host, SystemTime::now())
    }

    fn unreachable_at(&self, host: &str, now: SystemTime) -> bool {
        let Some(f) = self.file(host) else { return false };
        let Ok(when) = fs::metadata(f).and_then(|m| m.modified()) else { return false };
        now.duration_since(when).is_ok_and(|age| age < FORGET_AFTER)
    }

    pub fn remember(&self, host: &str) {
        let (Some(dir), Some(f)) = (&self.dir, self.file(host)) else { return };
        let _ = fs::create_dir_all(dir).and_then(|()| fs::write(f, b""));
    }

    pub fn forget(&self, host: &str) {
        if let Some(f) = self.file(host) {
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
        let tmp = tempfile::tempdir().unwrap();
        let m = Memory::at(tmp.path().join("state"));
        assert!(!m.unreachable("me@far"));
        m.remember("me@far");
        assert!(m.unreachable("me@far"));
        assert!(m.unreachable("far"), "the user name doesn't matter");
        assert!(m.unreachable("other@far"));
        assert!(!m.unreachable("near"));
        let later = SystemTime::now() + FORGET_AFTER + Duration::from_secs(1);
        assert!(!m.unreachable_at("far", later), "expired");
        m.forget("far");
        assert!(!m.unreachable("me@far"));
        // Nowhere to keep it: nothing remembered, nothing fails.
        let none = Memory { dir: None };
        none.remember("far");
        assert!(!none.unreachable("far"));
        std::fs::write(tmp.path().join("file"), b"").unwrap();
        let unusable = Memory::at(tmp.path().join("file/state"));
        unusable.remember("far");
        assert!(!unusable.unreachable("far"));
    }
}
