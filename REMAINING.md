# What's left in barqsync

Status on 21 October 2026.

- **Released:** 0.3.0 (crates.io, and a GitHub release with binaries and installers for
  macOS and Linux). Protocol 9.
- **0.4.0, on `main` (`2a8794a`), held until the release checks below pass.** It contains
  every P1 fix from [the audit](IMPLEMENTATION_PLAN.md) (PRs 15–19) plus PRs 11–13. The
  changes are listed in `CHANGELOG.md` under 0.4.0. The protocol is 10, so 0.4.x can't
  talk to 0.3.x.

How the P1 work was organized, and where each finding landed:
[docs/implementation/p1-coordination.md](docs/implementation/p1-coordination.md). The
milestone tracker is [docs/implementation/README.md](docs/implementation/README.md).

## Next steps, in order

1. **Release checks for 0.4.0:**
   - CI green on `main`.
   - The audit's probes and the three data-loss reproductions now fail safely or match
     rsync.
   - A real-Mac push and pull (`--streams 4` and plain ssh).
   - `cargo publish --dry-run` from a clean checkout.
   - The CHANGELOG date set to the release day.
2. **Publish 0.4.0** from a clean worktree, never the main checkout. Tag `origin/main`, then
   run `cargo publish` from that worktree. The commands are in `p1-coordination.md`; the
   tag starts the release workflow (binaries, installers, checksums).
3. **Security advisory** on GitHub for SEC-01 and SEC-02: versions up to 0.3.0 affected,
   fixed in 0.4.0. Yanking 0.1 to 0.3 on crates.io because of the data-loss bugs is your
   call.
4. **Install 0.4.0 on both Macs.** Then delete the old copies on the second Mac:
   `~/bin/barqsync-pr11` (a test build) and `~/bin/barqsync` (0.1.0).
5. **Tidy up:**
   - The main checkout still holds Codex's original uncommitted edits on top of v0.3.0.
     They're now on `main` (PRs 15–17) and kept on the local branch `audit/codex-wip`, so
     the checkout can be reset to `main` once you've checked nothing else is in it.
   - Delete merged and superseded branches: the milestone branches, the PR branches,
     `p1-base`, and the worktrees under `.claude/worktrees/`.
   - PRs 11–19 went in without a Codex review (usage limit). Ask for one once it resets.

## Still open after 0.4.0

### From the audit (P2, or what the P1 fixes left out)

- **Descriptor confinement:** file operations relative to an open directory with no-follow
  checks, against symlink-swap races (SEC-01 hardening, part 1B).
- **An end-to-end content hash** of each committed file (2B).
- **Bounded stream admission (NET-04) and a byte-based memory budget (PERF-01)** (3C, 5A).
- **Mutable files:** safe delta reads of files that shrink while mapped (PLAT-01, 5B), and
  large-file scheduling across streams (PERF-02, 5C).
- **Milestone 4A:** streams without any public listener (a private rendezvous through ssh).
  The relays already make streams work behind a firewall.
- **Signed and notarized macOS binaries;** CI fault injection and dependency audits (6B, 6C).
- **Gaps the PRs called out:**
  - No `--timeout` for a peer that stays connected but stops responding.
  - SIGTERM and SIGHUP aren't handled like SIGINT.
  - A remote transfer to the same host isn't detected as a same-file case (neither in
    rsync).
  - A source changed between the removal check and the unlink isn't caught.

### Rough edges found earlier

- **QUIC behind a firewall.** UDP can't go through ssh, so `--transport quic` and `auto` fall
  back to the single ssh pipe. A better fallback would be relayed TCP streams, which needs
  the server to offer both (a protocol change).
- **Firewalls that drop instead of reset ("stealth mode").** The direct attempt waits its full
  5 s before the relays start, on every run. The fix: remember the host.
- **Relays need ssh keys or an agent** (`BatchMode=yes`).
- **`cargo install --git` needs the package name** (`cargo install --git <url> barqsync`),
  because the repository also has the benchmark crate `bench/fast_rsync`. Installing from
  crates.io isn't affected.

### Not built (from rsync or the original plan)

- Daemon mode (`rsync://`) and the single-argument listing mode.
- NFS isn't tested. Cold-cache benchmarks need `sudo purge` or `drop_caches`.
- No benchmark on a fast wired LAN yet (the real-host runs were over Tailscale).
- **Worth trying:**
  - Make `--transport auto` the default.
  - A cheaper adaptive probe.
  - A refinement threshold set from the measured round trip.

### Outside barqsync (the Mac-to-Mac setup)

- One Mac is on 2.4 GHz Wi-Fi (0.5–7.8 MB/s measured). A cable or 5 GHz is the biggest
  speed-up available.
- The external drive on the second Mac hung earlier. Check how it is mounted and its write
  speed before using it again.
- The second Mac's disk is nearly full: 6–7 GiB free of 932 GiB.
- QUIC to the second Mac still needs its firewall exception; `--streams` doesn't anymore.
