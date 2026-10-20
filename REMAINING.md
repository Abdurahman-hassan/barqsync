# What's left in barqsync

Status on 20 October 2026. Released: **0.3.0** (crates.io, GitHub release with binaries for
macOS and Linux, `main` at `f9f8d8d`). It includes `-X`/`-A`, `--transport auto` and the CI
and release automation; its protocol is 9, so it can't talk to 0.2.x.

## Fixed on `fix/streams-through-firewall` (not released yet)

| Problem | Cause | Fix | Checked by |
| --- | --- | --- | --- |
| `--streams` failed with "Connection reset by peer" and fell back to the slow single ssh pipe | The server listens for the streams on a new random TCP port. A firewall on the server (the macOS firewall for an app it hasn't been told about, or any host or network firewall, NAT or VPN) refuses it. The only way around it was a `sudo socketfilterfw` rule on the remote, repeated after every reinstall. | When no direct connection opens, each stream goes over its own extra ssh connection instead. `barqsync --stream-relay` on the server passes the bytes to the stream listener on that machine, which now also listens on loopback with the same port. TLS with the pinned certificate and token still runs end to end, and several TCP connections still carry the data. No firewall change is needed. | **The real macOS firewall** (Intel MacBook, macOS 15.8, firewall on, a build with no exception) answered the direct connection with "Connection reset by peer (os error 54)". The 4 streams then went through ssh. On 57 MB over Wi-Fi via Tailscale: push 18.0 s on the single ssh pipe vs 12.7 s with relays, pull 65.5 s vs 38.6 s. All files were identical and no relay was left running. Also `bench/net/container.sh firewall` (real OpenSSH plus an iptables rule that resets everything but ssh), and integration tests. |
| The first relay version stalled for 10–20 s with real ssh, then fell back | `io::copy` uses `splice(2)` between pipe and socket on Linux. Through sshd's pipes, the TLS handshake stopped after the server's first flight. It worked whenever the relay ran slowly (under strace). | The relay uses plain reads and writes. | The same firewall run. The relay's log showed the full handshake passing within 2 ms. |
| `host:~/dir/` and `--remote-path=~/bin/barqsync` failed (`mkdir "~/dir/" failed`) | Paths travel in the handshake, so no shell expands `~`. `--remote-path` was quoted whole. | The server expands a leading `~` or `~user`. A leading `~` in `--remote-path` is left unquoted for the remote shell, as rsync does. | Unit tests; push and pull with both through the test shell |
| A missing barqsync on the remote gave a vague error | – | Exit status 127 now says the command wasn't found, and that ssh commands read `~/.zshenv`, not `~/.zshrc`. | `bad_remote_path_fails_cleanly` |
| A version mismatch named only protocol numbers | – | Both sides now name their barqsync version and say to install the same one on both machines. | – |
| `@host:dir` (an empty `$user`) showed ssh's usage text | – | Refused before ssh runs, with a clear message. | Integration test |
| A big local copy to a disk without clones moved the progress line only at the end | Progress counted a file when it was done. | The copy loops count as they write: `copy_file_range` on Linux, and the buffered and uncached loops. Only transfers count, not backups or `--link-dest` copies. | Unit tests |
| `--stats` showed "Literal data: 0 bytes" for files sent or copied whole | Only deltas counted literal data. | A whole file counts all of its bytes as literal data, as in rsync: a local copy, a whole file over the wire, or one in a small-file pack. | Differential test against rsync (local, push, pull) |
| README described the delta piece sizes by the new file's size (open review finding on PR 3) | – | The size of the old copy decides; the README now says so. | – |

Compatibility: a client with this fix talking to a 0.3.0 server can't use relays, because the
server doesn't know `--stream-relay`. It then falls back to the ssh pipe as before. No
protocol change, so this can ship as **0.3.1**.

## Next steps, in order

1. **Merge `fix/streams-through-firewall`** and release 0.3.1: bump the version, add a
   CHANGELOG entry, tag `v0.3.1` (the release workflow builds the binaries), then
   `cargo publish`.
2. **Review and merge PR #12**: the QUIC setup and close cost cut, `--transport auto`
   remembering hosts with a clean link, and the pull `--remove-source-files` fix ("sender
   removed" lines, exit 23 on failure). It's on protocol 10, so its release is 0.4.0. If
   PR #11 merges first, PR #12 needs main merged again (both touch the stream setup).
3. **Install the release on the second Mac** (`~/.cargo/bin/barqsync`). Then `--streams`
   works there without the firewall exception. A test build of this branch is in
   `~/bin/barqsync-pr11` on that Mac.
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
- Worth trying: making `--transport auto` the default once it can tell clean long links
  from lossy ones; a cheaper adaptive probe (`--strategy adaptive` costs 4–22% on
  incompressible data); a refinement threshold set from the measured round trip.

### Outside barqsync (the Mac-to-Mac setup)

- One Mac is on 2.4 GHz Wi-Fi (0.5–7.8 MB/s measured): a cable or 5 GHz is the biggest
  speed-up available.
- The external drive on the other Mac hung (processes stuck in state U): unplug it or
  restart, then check how it is mounted and its write speed before using it again.
- Once 0.3.1 is installed on both Macs, `--streams` no longer needs the firewall exception
  (checked against that Mac's firewall). QUIC still does.
- That Mac's disk is full: 6–7 GiB free of 932 GiB.
