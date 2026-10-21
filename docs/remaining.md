# What's left in barqsync

Status on 21 October 2026.

- **0.4.2** adds the interrupt-notice ordering and bounded QUIC shutdown fixes from
  PRs 23–24. The [release record](releases/0.4.2.md) describes its scope and checks.
  It retains 0.4.1's complete-scan deletion barrier and late-interrupt fix from PR 22.
  Protocol remains 10, so 0.4.x cannot talk to 0.3.x: upgrade both ends.

How the P1 work was organized, and where each finding landed:
[audit/p1-coordination.md](audit/p1-coordination.md). The
milestone tracker is [audit/README.md](audit/README.md).

The 0.4.1 release incorporates the [deletion barrier](audit/02a-deletion-barrier.md):
0.4.0 stopped deletion after a reported source error, but could already have deleted
entries before a later scan failure. Pending deletions now wait for a successful scan.

## Next steps, in order

1. **Security advisory** on GitHub for SEC-01 and SEC-02: versions up to 0.3.0 affected,
   fixed in 0.4.0. Yanking 0.1 to 0.3 on crates.io because of the data-loss bugs is your
   call.
2. **Install 0.4.2 on both Macs.** Then delete the old copies on the second Mac:
   `~/bin/barqsync-pr11` and `~/bin/barqsync-0.4.0-rc` (test builds), and `~/bin/barqsync`
   (0.1.0). Its firewall exceptions for those are no longer needed for `--streams`.
3. **Tidy up:**
   - The initial audit edits are committed and merged (PRs 15–19). Their original
     snapshot is retained locally as `audit/safety-wip`. The primary checkout was
     refreshed from `main` for the 0.4.2 release; preserve untracked local files
     when changing branches.
   - Delete merged and superseded branches: the milestone branches, the PR branches,
     `p1-base`, and the worktrees under `.claude/worktrees/`.
   - Remove the release worktree `/tmp/barqsync-v0.4.0`
     (`git worktree remove /tmp/barqsync-v0.4.0`).
   - Follow-up review of PRs 11–19 remains useful; release checks do not replace review
     of the implementation and its remaining acceptance criteria.
4. **Next releases:** follow [Releasing next time](audit/p1-coordination.md#releasing-next-time). 0.4.0 was tagged
   before its release PR merged, so the published crate's changelog says 2026-10-20 and the
   crate includes `IMPLEMENTATION_PLAN.md` and `REMAINING.md` (since moved into `docs/`).

## Still open after 0.4.2

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
