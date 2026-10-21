# Part 2A follow-up: scan completion before deletion

Date: 21 October 2026. Finding: DATA-03. Status: implemented in the current tree,
not part of the published 0.4.0 release.

## Baseline review

Work resumed from `567e570`, after reviewing PRs 17–21 and their moved documentation.
The same-file, backup and acknowledgement changes from PR 17, peer-input validation
from PR 18, and merged roots/cancellation from PR 19 are retained. The baseline
macOS suite reported 249 passing tests (3 real-host cases returned early), and
all-targets Clippy passed with warnings denied.

The review found that the DATA-03 guard only stopped deletion after an error was
reported. Directory tasks still removed absent destination entries before scanning
deeper source directories. On this baseline, a source `deep/broken -> missing-target`
with `-aL --delete --threads=1` caused exit 23 and the skip-deletion notice, but
`dst/keep` had already been deleted. The nested destination entry survived.
The new regression failed on the baseline with “deleted before the later scan error”.

## Changes

- Collect extraneous destination entries, deduplicated by path, while comparing
  listings. Deletion runs after all scan/transfer tasks have joined and before
  directory timestamps and permissions are finalized.
- Any reported source-scan error discards all pending deletion candidates. Remote
  failures and cancellation leave the queue uncommitted. Sender errors also prevent
  deletion, even if the sender only reports them in its final message.
- Require one `ScanDone` with the received listing count, reject later listings and
  premature `SenderDone`, and refuse deletion when a selected directory was never
  listed. These checks use existing protocol-10 messages and change no wire schema.
- Keep entries selected by another remote root, regardless of which listing arrived
  first. Before deletion, recheck the candidate's type, device and inode; a replaced
  candidate is preserved and reported as a partial transfer.
- Preserve the current filter, backup, dry-run and directory-fixup rules. Successful
  transfers still remove extraneous entries; their deletion output occurs later.

This barrier applies to pruning entries absent from the source. Replacing an entry
explicitly selected by the source remains part of the normal transfer. It is not a
filesystem snapshot or descriptor confinement: concurrent parent/symlink swaps,
same-inode edits and mutations after the last check remain separate hardening work.

Pending records use memory proportional to the number of deletion candidates, and
space occupied by those entries is reclaimed at the end rather than during scanning.
The byte-based memory budget in milestone 5A remains open.

## Verification

- `tests/deletion.rs`: a nested scan failure preserves earlier candidates in local
  transfers with both scanners and in push/pull, with threaded and Tokio I/O.
- `tests/peer_input.rs`: candidates survive until successful completion; late errors,
  fatal failure, invalid/missing/repeated scan completion, missing directory listings,
  overlapping roots in either order, and replaced candidates cannot remove them.
  Both wire encodings and connection backends run.
- Existing deletion tests cover merged roots, filters, named/relative sources and
  directory metadata.
- On macOS arm64, `env -u BARQSYNC_TEST_REMOTE cargo test --locked --offline`
  reported 255 passed, 0 failed. Three real-host cases returned early because no
  external test host was configured; 252 tests exercised their checks.
- `cargo clippy --locked --offline --all-targets -- -D warnings` and
  `git diff --check` passed. Linux and QUIC verification run in the PR's CI.

## Repository housekeeping

The checkpoint branches are now `fix/audit-safety-checkpoint` and
`fix/remote-location-parsing` locally and on GitHub; the original local snapshot is
`audit/safety-wip`. Their commit targets are unchanged. Current documentation uses
these names and the updated checkout/release state. Commits use Abdelrahman Hassan
Hamdy's existing repository identity, and PRs use the `Abdurahman-hassan` account.
Other worktrees and untracked local notes were preserved.
GitHub retains the original head-branch labels on already-merged PR records;
renaming live branches does not rewrite those historical records or commits.
