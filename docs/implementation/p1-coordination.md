# P1 fixes before 0.4.0: how they were organized, and where each landed

Date: 20–21 October 2026.

## How it started

0.4.0 was prepared for release in [PR 14](https://github.com/Abdurahman-hassan/barqsync/pull/14):
the version bump, the changelog date and the README protocol line. Before the tag was made,
`git pull` in the main checkout stopped on uncommitted changes. Those changes turned out to
be a Codex session's work in progress. It had written
[the audit](../../IMPLEMENTATION_PLAN.md), with 18 findings against v0.3.0, 12 of them P1,
and had begun fixing them on top of v0.3.0.

Three of the audit's data-loss findings were reproduced on the 0.4.0 code before anything
else was decided:

| Command | 0.4.0 before the fixes | rsync |
| --- | --- | --- |
| `barqsync -a --inplace -I f f` | `f` emptied, exit 0 | keeps `f` |
| `barqsync -a --remove-source-files g g` | `g` deleted, exit 0 | keeps `g`, reports an error |
| `barqsync -a --delete a/ b/ dst/` | `dst` keeps only `beta` | keeps `alpha` and `beta` |

The user decided to hold 0.4.0 (version set on `main`, not tagged or published) until the
P1 findings were fixed.

## How the work was split

1. **Codex's work was kept.** Its uncommitted diff and untracked files (tests, notes,
   probes) were committed to the local branch `audit/codex-wip`, based on v0.3.0, through a
   separate worktree. The main checkout was left as it was. The user stopped the Codex
   session, which then turned its work into PRs 15 and 16.
2. **Three parallel lanes** each got its own worktree from `main` at `eeb6955`, its own Linux
   build volume, and a brief that named its findings, the files it owned and the other
   lanes' scopes. The merge order was A, B, C, and each merged `main` again before
   finishing. Shared rules:
   - protocol 10 stays, since no release speaks it;
   - CHANGELOG entries go under 0.4.0;
   - a regression test that fails without its fix;
   - full macOS and Linux suites with clippy;
   - the user as the only author.

| Lane | Findings | PR |
| --- | --- | --- |
| Codex checkpoint | DATA-01, DATA-04, SEC-02 (first part), NET-02 | [#15](https://github.com/Abdurahman-hassan/barqsync/pull/15) |
| Codex 6A | CLI-01 (bracketed IPv6, host validation) | [#16](https://github.com/Abdurahman-hassan/barqsync/pull/16) |
| A: safe changes to files | DATA-01, DATA-04, DATA-06, SEC-02 (sender-side record of removable entries) | [#17](https://github.com/Abdurahman-hassan/barqsync/pull/17) |
| B: peer input | SEC-01, DATA-05, PROTO-01 | [#18](https://github.com/Abdurahman-hassan/barqsync/pull/18) |
| C: deletion and lifecycle | DATA-02, DATA-03, NET-03 (its NET-02 duplicate of #15 was dropped) | [#19](https://github.com/Abdurahman-hassan/barqsync/pull/19) |

NET-01 had already been fixed in [PR 11](https://github.com/Abdurahman-hassan/barqsync/pull/11);
see [04-ssh-relay.md](04-ssh-relay.md).

## Where each P1 finding stands

| Finding | Fixed in | What's still left |
| --- | --- | --- |
| SEC-01: remote names escaping the destination | #18; #19 (multi-root directory and `-K` cases) | Descriptor confinement (`openat`, no-follow) against symlink-swap races |
| SEC-02: unauthorized source removal | #15, #17 | – |
| DATA-01: same-file copies | #15, #17 | A remote transfer to the same host (`localhost:`) isn't detected; rsync doesn't detect it either |
| DATA-02: several roots with `--delete` | #19 | – |
| DATA-03: deletion after scan errors | #19 | – |
| DATA-04: backup failures | #15, #17 | – |
| DATA-05: missing or malformed file data | #18 | An end-to-end content hash (2B) |
| DATA-06: source-removal change detection | #17 | A change between the check and the unlink; ctime isn't compared |
| NET-01: streams behind a firewall | #11 | No public listener (4A); see 04-ssh-relay.md |
| NET-02: `--threads=1` deadlock | #15 | – |
| NET-03: cancellation and disconnects | #19 | No `--timeout` for a peer that stays connected but stops responding; SIGTERM and SIGHUP aren't handled like SIGINT |
| PROTO-01: oversized messages | #18 | Bounded stream admission (NET-04) and a memory budget (PERF-01) are P2 |

Final test counts in the last lane (#19): macOS 249 and Linux 253 passed, clippy clean on
both.

## Earlier work in the same period

- [PR 10](https://github.com/Abdurahman-hassan/barqsync/pull/10), `-X`/`-A`. Two review findings
  were fixed before merging:
  - A deferred xattr value is written to the file's final path, not to the temp file
    renamed before the value arrives.
  - With `--remove-source-files`, a source is held back until its deferred values are
    written, and kept if writing them fails.
- [PR 6](https://github.com/Abdurahman-hassan/barqsync/pull/6): the test host's address was
  taken out of a published benchmark report, and `bench/real_host.sh` no longer writes it.
- [PR 11](https://github.com/Abdurahman-hassan/barqsync/pull/11),
  [PR 13](https://github.com/Abdurahman-hassan/barqsync/pull/13): streams through ssh, `~`
  paths, clearer errors, local copy progress, literal-data stats.
- [PR 14](https://github.com/Abdurahman-hassan/barqsync/pull/14): 0.4.0 version bump.

## Releasing 0.4.0

1. Wait for CI on `main` to pass after #19.
2. Run the audit's probes and the three reproductions above on `main`. Each must now fail
   safely or match rsync.
3. Check on the real Mac (Tailscale, firewall on): push and pull with `--streams 4`, plus a
   plain ssh push and pull.
4. Run `cargo publish --dry-run` from a clean checkout of `main`.
5. Set the CHANGELOG date to the release day.
6. Tag and publish from a clean worktree, never the main checkout:

   ```bash
   git fetch origin && git tag -a v0.4.0 -m "barqsync 0.4.0" origin/main && git push origin v0.4.0
   ```

   ```bash
   git worktree add /tmp/barqsync-v0.4.0 v0.4.0 && cd /tmp/barqsync-v0.4.0 && cargo publish
   ```

7. Publish a GitHub security advisory for SEC-01 and SEC-02: 0.1.0 to 0.3.0 affected,
   fixed in 0.4.0. Whether to yank 0.1 to 0.3 on crates.io because of the data-loss bugs is
   the user's call.
