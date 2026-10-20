# What's left in barqsync

Status on 20 October 2026.

- **Released:** 0.3.0 (crates.io, and a GitHub release with binaries and installers for
  macOS and Linux). Protocol 9.
- **On `main`, not released:**
  - parallel streams through ssh when a firewall blocks them;
  - `~` in remote paths;
  - clearer errors;
  - local copy progress;
  - whole files counted as literal data in `--stats`;
  - QUIC's fixed cost cut to about two round trips;
  - `--transport auto` remembering clean links;
  - pull `--remove-source-files` reporting its removals;
  - `BARQSYNC_TIMING`.

  The details are in `CHANGELOG.md` under "Unreleased". The protocol is now 10, so the next
  release can't talk to 0.3.x.

## Next steps, in order

1. **Release 0.4.0.**
   - Set `version = "0.4.0"` in `Cargo.toml` and update `Cargo.lock` (`cargo update -p barqsync`).
   - Rename the CHANGELOG's "Unreleased" heading to "0.4.0 - <date>".
   - In the README, change "0.3.x uses protocol 9" to "0.4.x uses protocol 10 and cannot
     communicate with 0.3.x or earlier".
   - Tag `v0.4.0` and push the tag: the release workflow builds the binaries, installers and
     checksums.
   - Run `cargo publish` from `main`.
2. **Install 0.4.0 on both Macs** (`cargo install barqsync`, or the release's installer).
   Then `--streams` works on the second Mac without its firewall exception, as checked
   against that firewall. Afterwards, delete the old copies there: the test build
   `~/bin/barqsync-pr11` and `~/bin/barqsync` (0.1.0).
3. **Have Codex review what went in without it.** PRs 11, 12 and 13 hit the Codex usage
   limit. Once it resets, ask for a review (`@codex review` on a PR), or review
   `main` since `v0.3.0` yourself.
4. **Clean up branches.** All of these are merged or superseded:
   - the milestone branches `local-engine`, `delta-engine`, `remote-ssh`, `parallel-tcp`,
     `polish-m6`, `plan-v2`, `m7-integrated`, `m8-verify`, `m9-delta-cdc`, `m10-jwalk-scanner`,
     `m11-quic-rkyv`, `m12-adaptive`, `m13-io-uring`, `m14-verification`;
   - the PR branches `ccr-2f85cd18-jxdmlm`, `docs/scrub-host-address`, `feat/*` and `fix/*`;
   - `feat/zen-lamport-e84742` and `feat/gallant-sanderson-bf6749`, which hold early
     work that was redone.

   Keep the milestone branches if you want the history by milestone; `main` has all the
   code. There are also 11 local worktrees under `.claude/worktrees/`.

## Still open

### Bugs and rough edges

- **QUIC behind a firewall.** UDP can't go through ssh, so `--transport quic` and `auto` fall
  back to the single ssh pipe when the server's firewall blocks UDP. A better fallback would
  be relayed TCP streams, as `--transport tcp` now does. That needs the server to offer both
  (a protocol change).
- **Firewalls that drop instead of reset ("stealth mode").** The direct attempt waits its full
  5 s before the relays start, on every run. The fix: remember per host that direct
  connections fail, as `--transport auto` does for QUIC.
- **Relays need ssh keys or an agent.** They run with `BatchMode=yes`, so a host that only
  takes passwords stays on the single ssh pipe.
- **Leftover `barqsync --server` after Ctrl-C** (seen twice on macOS, never reproduced). Most
  likely the external drive that hung at the time: rsync's servers writing to it got stuck
  the same way, in state U, where even `kill -9` can't end a process. If it happens with a
  healthy disk, run `sample <pid>` on the leftover process before killing it.
- **`cargo install --git` needs the package name** (`cargo install --git <url> barqsync`),
  because the repository also has the benchmark crate `bench/fast_rsync` (`frbench`).
  Installing from crates.io isn't affected.

### Not built (from rsync or the original plan)

- Daemon mode (`rsync://`) and the single-argument listing mode.
- NFS isn't tested (no server). Cold-cache benchmarks need `sudo purge` or `drop_caches`.
- No benchmark on a fast wired LAN yet. The real-host runs were over Tailscale: first across
  the internet (about 4 Mbit/s), then directly over Wi-Fi (about 30 Mbit/s).
- **Worth trying:**
  - Make `--transport auto` the default. It now tells clean long links from lossy ones after a
    transfer, but it still opens a UDP port on every server and hasn't met a fast real long
    link.
  - A cheaper adaptive probe: `--strategy adaptive` costs 4–22% on incompressible data.
  - A refinement threshold set from the measured round trip.

### Outside barqsync (the Mac-to-Mac setup)

- One Mac is on 2.4 GHz Wi-Fi (0.5–7.8 MB/s measured). A cable or 5 GHz is the biggest
  speed-up available.
- The external drive on the second Mac hung (processes stuck in state U). Unplug it or
  restart, then check how it is mounted and its write speed before using it again.
- The second Mac's disk is nearly full: 6–7 GiB free of 932 GiB.
- QUIC to the second Mac still needs its firewall exception; `--streams` doesn't anymore.

## Done since 0.3.0, for reference

| Problem | Fix | Checked by |
| --- | --- | --- |
| `--streams` failed with "Connection reset by peer" behind a firewall and fell back to the slow single ssh pipe | Blocked streams go over extra ssh connections. A relay on the server passes them to the stream listener over loopback. TLS with the pinned certificate and token still runs end to end. | The real macOS firewall: push 18.0 s on the single ssh pipe vs 12.7 s with relays, and pull 65.5 s vs 38.6 s, on 57 MB with identical files. Also iptables with real OpenSSH (`bench/net/container.sh firewall`) and integration tests. |
| The first relay version stalled for 10–20 s with real ssh | `io::copy` splices between pipe and socket on Linux, and through sshd's pipes the TLS handshake stalled. The relay now uses plain reads and writes. | The same firewall runs |
| `host:~/dir/` and `--remote-path=~/…` failed | The server expands a leading `~`, and the client leaves it unquoted in `--remote-path`, as rsync does | Tests, and the real Mac |
| Vague errors (missing remote binary, version mismatch, `@host`) | Exit 127 now says the command wasn't found and mentions `~/.zshenv`. A mismatch names both versions. `@host` is refused before ssh runs. | Tests |
| A big local copy moved the progress line only at the end | The copy loops count as they go (transfers only) | Unit tests |
| `--stats` "Literal data: 0 bytes" for whole files | A whole file counts as literal data, as in rsync | Differential test against rsync (local, push, pull) |
| QUIC cost three to five round trips more than ssh; pull `--remove-source-files` was silent | PR 12 | Its tests (see the PR) |
