# P1 fixes before 0.4.0: how they were organized, and where each landed

Date: 20–21 October 2026.

## How it started

0.4.0 was prepared for release in [PR 14](https://github.com/Abdurahman-hassan/barqsync/pull/14):
the version bump, the changelog date and the README protocol line. Before the tag was made,
`git pull` in the main checkout stopped on uncommitted changes. Those changes turned out to
be a Codex session's work in progress. It had written
[the audit](findings.md), with 18 findings against v0.3.0, 12 of them P1,
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

## Release checks, 21 October 2026

All run on `main` at `2a8794a`, from a clean checkout.

- **CI on `main`:** passed.
- **The three data-loss reproductions** now match rsync 3.5.1:
  - `--inplace -I f f` leaves `f` intact;
  - `--remove-source-files g g` keeps `g` and reports "Skipping sender remove of
    destination file" (exit 23);
  - `--delete a/ b/ dst/` keeps `alpha` and `beta`.
- **The audit's CLI probes** (`cli_probes.py`):
  - `--delete` across several sources, and after a read error: the same as rsync,
    including "IO error encountered -- skipping file deletion" and exit 23;
  - same-file `--inplace` and `--remove-source-files`: the same as rsync;
  - a failed backup keeps the old file. The exit code is 23, where rsync stops the whole run
    with 11;
  - `--threads=1` and `2` remote runs finish.
- **The audit's protocol probes** (`protocol_probes.rs`), ported to the current API: its
  `run` functions take one more argument, and `Synced` now carries only paths.
  - A listing naming `../escaped` is refused (exit 12) and nothing is written.
  - A file with no data, or a pack shorter than announced, is discarded (exit 23).
  - `Synced` without `--remove-source-files` or in a dry run is refused (exit 12).
  - `Synced` for a file outside the transfer while `--remove-source-files` is on is refused
    ("removal of an entry not sent or already removed"). In all three cases the file is kept.
  - A source changed within the same second is kept.
- **Real Mac** (Intel MacBook Pro, macOS 15.8, firewall on, this build installed at a new path
  with no exception, Tailscale directly over the LAN at a 6 ms round trip), 57 MB in 30
  files, all identical after each run, nothing left running:

  | | plain ssh | `--streams 4` |
  | --- | ---: | ---: |
  | push | 4.7 s | 5.3 s |
  | pull | 7.0 s | 6.1 s |

  For `--streams 4`, the direct connection was refused ("unexpected end of file" and
  "Connection reset by peer"), so all 4 streams went through ssh. On this fast link, the
  streams' setup cost about cancels their gain at 57 MB. On the slower link of 20 October
  they were 1.4 to 1.7 times faster ([04-ssh-relay.md](04-ssh-relay.md)).
- **`cargo publish --dry-run`:** packaged and verified 0.4.0 at `2a8794a` (95 files, 368 KiB
  compressed).

## How 0.4.0 was released

0.4.0 was tagged and published on 21 October from `main` at `2a8794a`, before PR 20 was
merged:

- The tag `v0.4.0` points at `2a8794a`.
- crates.io has had 0.4.0 since 08:25 UTC.
- The release workflow built the binaries from the tag.

The published crate is the code checked above, but two things in it differ from PR 20:

- Its `CHANGELOG.md` still dates 0.4.0 2026-10-20.
- It includes `IMPLEMENTATION_PLAN.md` and `REMAINING.md`, the names these documents had at
  the repository root then (now `docs/audit/findings.md` and `docs/remaining.md`).

Neither affects the program. A published crate version can't be replaced, so they stay.
PR 20 dates the changelog 2026-10-21, the day 0.4.0 was published, and its `Cargo.toml`
exclusion applies from the next release.

The earlier version of the steps below caused this. They changed the date only in a pull
request, then tagged `origin/main`, so the tag never contained the change. They also ran
the dry run before the package contents changed.

## Releasing next time

1. Put everything the release changes (version, CHANGELOG date, README, package contents)
   in one pull request, and merge it.
2. Fetch and note the exact commit to release. Check that CI passed on that commit.

   ```bash
   git fetch origin && git rev-parse origin/main
   ```

3. Run the release checks and the dry run in a clean worktree of **that commit**:

   ```bash
   git worktree add /tmp/barqsync-release <commit> && cd /tmp/barqsync-release && cargo publish --dry-run
   ```

4. Tag that same commit by its hash, not by `origin/main`, which may have moved since:

   ```bash
   git tag -a vX.Y.Z -m "barqsync X.Y.Z" <commit> && git push origin vX.Y.Z
   ```

5. Publish from the same worktree: `cargo publish`.
