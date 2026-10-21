# Part 1A: mutation safety

Date: 20 October 2026.

Status: implemented; checkpoint verification is recorded in
[verification.md](verification.md). The full audit release gates remain open.

## Changes

Revised on 21 October 2026 to match rsync where the first checkpoint did not, and to
close the gaps listed below it.

- **DATA-01:** a source that is also the destination (the same path, a `./` alias,
  or a hard link) is never truncated or removed. In-place copying opens both files
  without truncating and compares those descriptors; when they are one file it writes
  nothing, since the file already holds the data, and cuts the length only after
  copying otherwise. `--remove-source-files` compares the source with the destination
  and keeps it ("Skipping sender remove of destination file", partial-transfer status),
  as rsync does. Syncs that change nothing (`-a f f`, `src/ src/`, a destination of
  hard links to the source) succeed, as with rsync; the first checkpoint failed them.
- **DATA-04:** a backup that can't be made stops that file's replacement: regular
  files, remote whole files/deltas/packs, io_uring packs, symlinks, special files,
  hard-link followers and alternate-destination links and copies. Hard-link followers
  and special files are now backed up at all, as rsync does. A new backup is built
  under a temp name beside the previous one and renamed over it only once complete.
  A backup that would replace the file itself (an empty suffix with a backup directory
  leading back to it) or the source being copied (`-b f~ f`) is refused, and an empty
  `--suffix` without `--backup-dir` is rejected up front, as rsync does. A backup name
  that is already a hard link to the file holds that version and is kept (with
  `--inplace` it becomes a copy first); the first checkpoint refused it on every run.
- **SEC-02:** the sender records each non-directory entry it lists (root, path, and
  its own metadata) and honors a `Synced` only for entries on that record that it
  hasn't removed yet, each once; anything else is an error and nothing is removed.
  `Synced` without `--remove-source-files`, or in a dry run, is a protocol error, and
  the removal helper refuses on its own too. `Synced` now carries entry names only:
  the receiver no longer sends size and time, so there is nothing to trust (protocol
  10, finalized before v0.4.0 was released).
- **DATA-06:** before unlinking, the helper compares type, device/inode, size and
  mtime to the nanosecond with the sender's record. Same-second edits, same-size inode
  replacement and symlink swaps prevent removal. The macOS bulk scanner now reports
  device numbers sign-extended like `stat`, so its records compare equal to `lstat`.
- Removal happens as each entry is acknowledged, as with rsync, not only after a
  fully successful session: one failed file no longer keeps every other source.
- Sender control-send failures count as errors.

`remove_source` takes the sender's `&Meta` record and, for local transfers, the
destination path.

## Verification coverage

`tests/mutation_safety.rs` covers same-file aliases (whole-file, delta, backup and
removal modes), trees synced onto themselves or onto hard links to them with both
scanners, the descriptor check, invalid backup directories (locally and through
fake-SSH push/pull, threaded and Tokio I/O, small and large files, deltas, io_uring),
hard-link and special-file backups, an existing hard-linked backup, a directory in the
backup's way, unreadable backup sources and retry, backups onto the source or the file
itself, the removal helper's checks, and authorized removal of files, hard links and
symlinks locally, in a push and in a pull.

`tests/source_ack_safety.rs` uses misbehaving receivers with postcard and rkyv:
removal not requested (with and without dry run), dry run, absolute and traversal
paths, unknown roots, entries created after listing, duplicates, changed and replaced
sources, acknowledgements before the listing was answered, and valid removal.

`tests/remote_lifecycle.rs` and the rsync/xattr suites cover removal with hard links
and deferred xattrs. Each fix's regression test was checked to fail with that fix
reverted (the io_uring receiver path on Linux only).

## Remaining acceptance work

- Descriptor-based confinement and protection from concurrent parent-directory
  or symlink replacement are **not implemented**. SEC-01 remains open.
- The record comes from listing metadata. Ctime and verification against the file
  actually transferred still need work; a file changed between the check and the
  unlink is not caught. This is not a filesystem snapshot.
- Same-file detection covers transfers on one machine; a remote transfer to the
  same host (`localhost:`) is not detected, as with rsync.
- Backup paths colliding with a different active output need namespace-wide
  checks. ENOSPC and injected short-write failures still need dedicated tests.
  These changes do not establish crash durability or directory fsync guarantees.
