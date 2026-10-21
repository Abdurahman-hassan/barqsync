# Milestone 4: parallel streams through ssh (NET-01)

Date: 20–21 October 2026.

Status: the user-facing problem is fixed and verified on a real Mac with its firewall on
([PR 11](https://github.com/Abdurahman-hassan/barqsync/pull/11)). The audit's stricter
acceptance gate, "no public data listener", is **not** met: the server still offers its
TLS listener first, and the relays are the fallback. 4A (a private Unix-socket rendezvous
with no listener) remains open.

Finding: NET-01, "Parallel streams require an additional inbound service that macOS can
block".

## The problem

`--streams N` (`--transport tcp`) carries file data on N extra TLS connections:

1. The server listens on a new random TCP port, on the address the client reached it at.
2. It sends the port, a pinned certificate fingerprint and a token over ssh.
3. The client connects to that port.

A firewall on the server refuses that port. Examples are the macOS application firewall
for a binary it hasn't been told about, or any host or network firewall, NAT or VPN that
only lets ssh in. barqsync then fell back to the single ssh pipe and lost the parallel
speed-up. The only workaround was a `sudo socketfilterfw` exception on the remote, repeated
after every reinstall. Users saw:

```text
parallel streams: could not connect to …:55575 (Connection reset by peer (os error 54)), using the ssh connection
```

## What was built

When no direct connection opens, the client opens the same N streams over N extra ssh
connections instead (`src/remote/relay.rs`):

- **The relay.** Each ssh connection runs `barqsync --stream-relay ADDR[,ADDR]` on the
  server. It connects to the stream listener on that machine and passes bytes both ways
  between its stdin/stdout and that socket. `--stream-relay` is handled in `main.rs` before
  the options are parsed, so it isn't part of `Options`, which travel in the handshake.
- **Loopback listener.** The listener also listens on loopback, on the same port
  (`streams::bind_on`). The relay tries loopback first, which firewalls don't filter, then
  the advertised address. If that loopback port is taken, the listener picks another port
  pair, up to 8 times.
- **End-to-end TLS.** TLS with the pinned certificate and the token still runs between the
  client and the server. The relay only sees ciphertext and holds no secret: the token
  never appears on a command line. `streams::client_handshake` is shared by direct and
  relayed streams.
- **ssh options.** With ssh as the remote shell, relays run with `BatchMode=yes` (they never
  prompt), `ControlMaster=no` and `ControlPath=none`, so each is its own TCP connection
  rather than a channel of a shared master, plus `ConnectTimeout=10`. These options come
  before the user's `-e` options, so ssh takes them first. Without key or agent
  authentication the relays fail, and the transfer stays on the ssh pipe.
- **Timeouts and clean-up.** All relays together get 20 s; stuck ones are killed. The client
  waits for relay processes at the end (`relay::reap`, at most 5 s), and kills the rest.
  The first line ssh printed on stderr explains a failed relay.
- **Messages.** `-v` says "… refused a direct connection (…), using N streams through ssh".
  When relays fail too, the message gives both reasons:
  "could not connect to … (…; through ssh: …), using the ssh connection".
- **Protocol.** No protocol change. A 0.3.0 server doesn't know `--stream-relay`, so a
  newer client falls back to the ssh pipe as before.

### A bug found on the way: `io::copy` in the relay

The first relay used `std::io::copy` in both directions. With real OpenSSH it stalled for
10–20 s and then fell back, but only when the relay ran at full speed. Under `strace` it
worked. Relay logs showed the cause:

- ClientHello (243 bytes) went up, and the server's first flight (671 bytes) came down.
- The client's next 136 bytes (Finished plus the token) never reached the listener.
- The server's 5 s read timeout then closed the connection.

On Linux, `io::copy` between a pipe and a socket uses `splice(2)` with a huge length.
Through sshd's pipes that left the handshake stuck. The relay now uses a plain read/write
loop (`relay::pump`), and with it the whole handshake took 2 ms. The fake-ssh integration
test hadn't caught this: there the relay's stdin and stdout are the client's own pipes,
with no sshd in between.

## Verification

- **Real macOS firewall:**
  - Setup: an Intel MacBook Pro running macOS 15.8 with the application firewall on, and
    the test build installed at a new path with no exception. The link was Tailscale,
    directly over 2.4 GHz Wi-Fi: 5 ms round trip, about 30 Mbit/s over ssh.
  - The direct connection got `Connection reset by peer (os error 54)`, the same error users
    reported, and the 4 streams went through ssh.

  | 57 MB, 30 files | single ssh pipe | 4 streams through ssh |
  | --- | ---: | ---: |
  | push | 18.0 s | **12.7 s** |
  | pull | 65.5 s | **38.6 s** |

  All files were identical (md5), and no barqsync process was left on the Mac.
- **Real OpenSSH plus a real firewall in the container:** `bench/net/container.sh firewall`
  (`bench/net/firewall.sh`). sshd listens on the container's own address, and an iptables
  rule resets every new TCP connection except ssh. Push and pull with 4 streams through ssh
  give identical copies, and no relay is left running.
- **Tests:**
  - `parallel_streams_go_through_ssh_when_blocked`, push and pull. Here
    `BARQSYNC_TEST_DIRECT_BLOCKED` makes the direct attempt fail.
  - `parallel_streams_fall_back_to_ssh_when_relays_fail_too`.
  - `the_listener_also_listens_on_loopback_with_the_same_port`, which runs fully on Linux.
  - Relay target and `--remote-path` quoting unit tests.

## Against the audit's milestone 4 parts

| Part | Status |
| --- | --- |
| 4A: versioned session negotiation, private Unix-socket helpers | Not done. Streams still use the TLS listener; relays reach it over loopback. No protocol change was needed. |
| 4B: ssh lifecycle, fallback and diagnostics | Done for the relay: bounded setup, reaping, both reasons on failure. PR 19 added bounded shutdown on Ctrl-C and lost peers, including relays. |
| 4C: signed release, real Mac acceptance | Real-Mac acceptance done (above). Signed and notarized binaries are not done. |

## Still open

- **No public listener (4A).** It would remove the direct attempt and its listener entirely.
- **Firewalls that drop instead of reset.** The direct attempt waits its full 5 s before the
  relays start, on every run. The fix is to remember per host that direct connections
  fail, as `--transport auto` does for QUIC.
- **Relays need ssh keys or an agent** (`BatchMode`).
- **QUIC behind a firewall.** UDP can't ride ssh, so `--transport quic` and `auto` fall back
  to the single pipe there. Falling back to relayed TCP streams would need the server to
  offer both (a protocol change).

## Other fixes in the same PRs

- **`~` paths** (PR 11). `host:~/dir/` and `--remote-path=~/…` mean the remote home: the
  server expands a leading `~` or `~user`, and the client leaves it unquoted for the remote
  shell. PR 16 later replaced remote-location parsing as a whole (bracketed IPv6, host
  validation) and kept this behavior.
- **Errors** (PR 11).
  - Exit 127 from the remote shell says barqsync wasn't found and mentions `~/.zshenv`.
  - A version mismatch names both barqsync versions.
  - `@host` (an empty user) is refused before ssh runs.
- **Local copy progress** (PR 11). Copies to disks without clones count bytes as they are
  written. Only transfers count (`progress::tracking`), not backups or `--link-dest` copies.
- **`--stats` literal data** (PR 13). A whole file now counts as literal data, as in rsync.
  A differential test covers local, push and pull.
- **README** (PR 11). The delta piece size follows the size of the old copy; this was an open
  review finding on PR 3.
