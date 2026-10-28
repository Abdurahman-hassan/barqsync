# What's left in barqsync

Status on 29 October 2026.

- **0.5.0** releases everything merged since 0.4.2: the hardening against hostile
  peers and swapped source files (PR 27), no deletion after a failed write (PR 30),
  the everyday rsync options (PRs 28, 31, 33, 37), `--out-format` and `--info`
  (PR 32), `--log-file` (PR 35) and the `-x` mount-point fixes (PRs 34, 36, 38). The
  [release record](releases/0.5.0.md) describes its scope and checks. Protocol 11, so
  0.5.x can't talk to 0.4.x: upgrade both ends together.
- **0.5.1** accepts `--info=mount` (PR 40), which 0.5.0 refused; see its
  [release record](releases/0.5.1.md). Protocol 11, as 0.5.0.

How the P1 work was organized, and where each finding landed:
[audit/p1-coordination.md](audit/p1-coordination.md). The
milestone tracker is [audit/README.md](audit/README.md).

The 0.4.1 release incorporates the [deletion barrier](audit/02a-deletion-barrier.md):
0.4.0 stopped deletion after a reported source error, but could already have deleted
entries before a later scan failure. Pending deletions now wait for a successful scan.

## Next steps, in order

1. **Yanking 0.1 to 0.3** on crates.io because of the data-loss bugs is your call. The
   GitHub advisories for SEC-01 and SEC-02 (versions up to 0.3.0 affected, fixed in 0.4.0)
   are published: GHSA-44h8-452c-567g and GHSA-vw2w-g2g6-46ww.
2. **Install 0.5.1 on both Macs.** Then delete the old copies on the second Mac:
   `~/bin/barqsync-pr11` and `~/bin/barqsync-0.4.0-rc` (test builds), and `~/bin/barqsync`
   (0.1.0). Its firewall exceptions for those are no longer needed for `--streams`.
3. **Tidy up:**
   - Delete merged and superseded branches: the milestone branches, the PR branches,
     and the worktrees under `.claude/worktrees/`.
   - Follow-up review of PRs 11–19 remains useful; release checks do not replace review
     of the implementation and its remaining acceptance criteria.
4. **Next releases:** follow [Releasing next time](audit/p1-coordination.md#releasing-next-time). 0.4.0 was tagged
   before its release PR merged, so the published crate's changelog says 2026-10-20 and the
   crate includes `IMPLEMENTATION_PLAN.md` and `REMAINING.md` (since moved into `docs/`).

## Still open after 0.5.1

### From the audit (P2, or what the P1 fixes left out)

- **Descriptor confinement:** file operations relative to an open directory with no-follow
  checks, against symlink-swap races in parent directories (SEC-01 hardening, part 1B).
  PR 27 covers only a listed source file itself being swapped for a symlink.
- **An end-to-end content hash** of each committed file (2B).
- **Bounded stream admission (NET-04) and a byte-based memory budget (PERF-01)** (3C, 5A).
- **Mutable files:** safe delta reads of files that shrink while mapped (PLAT-01, 5B), and
  large-file scheduling across streams (PERF-02, 5C).
- **Milestone 4A:** streams without any public listener (a private rendezvous through ssh).
  The relays already make streams work behind a firewall.
- **Signed and notarized macOS binaries;** CI fault injection and dependency audits (6B, 6C).
- **Gaps the PRs called out:**
  - `-c` still reads source files for their checksums by path, following a symlink
    swapped in after the scan; PR 27 made the data reads safe, not these.
  - No restricted server mode (left for later by PR 27).
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
