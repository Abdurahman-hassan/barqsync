# What's left in barqsync

Status on 20 October 2026. Released: **0.3.0** (crates.io, GitHub release with binaries for
macOS and Linux, `main` at `f9f8d8d`). It includes `-X`/`-A`, `--transport auto` and the CI
and release automation; its protocol is 9, so it can't talk to 0.2.x.

## Fixed on `fix/streams-through-firewall` (not released yet)

| Problem | Cause | Fix | Checked by |
| --- | --- | --- | --- |
| `--streams` failed with "Connection reset by peer" and fell back to the slow single ssh pipe | The server listens for the streams on a new random TCP port. A firewall on the server (the macOS firewall for an app it hasn't been told about, or any host or network firewall, NAT or VPN) refuses it. The only way around it was a `sudo socketfilterfw` rule on the remote, repeated after every reinstall. | When no direct connection opens, each stream goes over its own extra ssh connection instead. `barqsync --stream-relay` on the server passes the bytes to the stream listener on that machine, which now also listens on loopback with the same port. TLS with the pinned certificate and token still runs end to end, and several TCP connections still carry the data. No firewall change is needed. | `bench/net/container.sh firewall`: real OpenSSH plus an iptables rule that resets every TCP connection except ssh. Push and pull with 4 streams through ssh gave identical copies, and no relay was left running. Integration tests with a blocked direct path, and with relays failing too. |
| The first relay version stalled for 10–20 s with real ssh, then fell back | `io::copy` uses `splice(2)` between pipe and socket on Linux. Through sshd's pipes, the TLS handshake stopped after the server's first flight. It worked whenever the relay ran slowly (under strace). | The relay uses plain reads and writes. | The same firewall run. The relay's log showed the full handshake passing within 2 ms. |
| `host:~/dir/` and `--remote-path=~/bin/barqsync` failed (`mkdir "~/dir/" failed`) | Paths travel in the handshake, so no shell expands `~`. `--remote-path` was quoted whole. | The server expands a leading `~` or `~user`. A leading `~` in `--remote-path` is left unquoted for the remote shell, as rsync does. | Unit tests; push and pull with both through the test shell |
| A missing barqsync on the remote gave a vague error | – | Exit status 127 now says the command wasn't found, and that ssh commands read `~/.zshenv`, not `~/.zshrc`. | `bad_remote_path_fails_cleanly` |
| A version mismatch named only protocol numbers | – | Both sides now name their barqsync version and say to install the same one on both machines. | – |
| `@host:dir` (an empty `$user`) showed ssh's usage text | – | Refused before ssh runs, with a clear message. | Integration test |
| A big local copy to a disk without clones moved the progress line only at the end | Progress counted a file when it was done. | The copy loops count as they write: `copy_file_range` on Linux, and the buffered and uncached loops. Only transfers count, not backups or `--link-dest` copies. | Unit tests |
| README described the delta piece sizes by the new file's size (open review finding on PR 3) | – | The size of the old copy decides; the README now says so. | – |

Compatibility: a client with this fix talking to a 0.3.0 server can't use relays, because the
server doesn't know `--stream-relay`. It then falls back to the ssh pipe as before. No
protocol change, so this can ship as **0.3.1**.

## Next steps, in order

1. **Merge `fix/streams-through-firewall`** and release 0.3.1: bump the version, add a
   CHANGELOG entry, tag `v0.3.1` (the release workflow builds the binaries), then
   `cargo publish`.
2. **Try it against the real macOS firewall.** The second Mac didn't answer over ssh while this
   was built, so relays were tested against iptables, not the macOS firewall. Install 0.3.1
   on both Macs, then run `barqsync -av --streams 4 big/ user@mac:big/`. Expect "using 4
   streams through ssh". The relay reaches the listener over loopback, which the macOS
   firewall shouldn't filter. If it does, the relay tries the advertised address next.
3. **Review and merge PR 12:** `feat/quic-close-cost` (QUIC setup and close cost about three
   round trips lower; `--transport auto` remembers hosts with a clean link) and
   `fix/pull-remove-source-logs` on top of it (a pull with `--remove-source-files` now
   prints rsync's "sender removed" lines and exits 23 when a removal fails), merged with
   `main` as M19. Its `RemovalsDone` message makes the protocol 10, so a release that
   includes it can't talk to 0.3.x: release 0.3.1 from `main` before merging it, or make the
   next release 0.4.0 (and update the README's "0.3.x uses protocol 9").
4. **Clean up branches:** `feat/zen-lamport-e84742` holds one README commit from before the
   README rewrite (obsolete). The merged PR branches can go too.

## Still open

### Bugs and rough edges

- **QUIC behind a firewall.** UDP can't go through ssh, so `--transport quic` and `auto` fall
  back to the single ssh pipe when the server's firewall blocks them. A better fallback:
  relayed TCP streams, as `--transport tcp` now does. That needs the server to offer both
  (a protocol change).
- **Firewalls that drop instead of reset ("stealth mode").** The direct attempt waits its full
  5 s before the relays start, on every run. The fix is to remember per host that direct
  connections fail, as `--transport auto` does for QUIC.
- **Relays need ssh keys or an agent.** They run with `BatchMode=yes`, so a host that only
  takes passwords stays on the single ssh pipe.
- **Leftover `barqsync --server` after Ctrl-C** (seen twice on macOS, not reproduced). Most
  likely the external drive that hung at the time: rsync's servers writing to it got stuck
  the same way, in state U, where even `kill -9` can't end a process. If it happens with a
  healthy disk, run `sample <pid>` on the leftover process before killing it.
- **`cargo install --git` needs the package name** (`cargo install --git <url> barqsync`),
  because the repository also has the benchmark crate `bench/fast_rsync` (`frbench`).
  Installing from crates.io isn't affected.

### Not built (from rsync or the original plan)

- Daemon mode (`rsync://`) and the single-argument listing mode.
- NFS isn't tested (no server); cold-cache benchmarks need `sudo purge`/`drop_caches`.
- No benchmark on a real LAN yet: the real-host runs were over Tailscale across the internet.
- Worth trying: making `--transport auto` the default (with PR 12 it tells clean long links
  from lossy ones after a transfer; it still opens a UDP port on every server and hasn't met
  a fast real long link); a cheaper adaptive probe (`--strategy adaptive` costs 4–22% on
  incompressible data); a refinement threshold set from the measured round trip.

### Outside barqsync (the Mac-to-Mac setup)

- One Mac is on 2.4 GHz Wi-Fi (0.5–7.8 MB/s measured): a cable or 5 GHz is the biggest
  speed-up available.
- The external drive on the other Mac hung (processes stuck in state U): unplug it or
  restart, then check how it is mounted and its write speed before using it again.
- Once 0.3.1 is installed on both Macs, `--streams` no longer needs the firewall exception.
  QUIC still does.
