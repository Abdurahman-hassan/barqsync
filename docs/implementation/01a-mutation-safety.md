# Part 1A: mutation safety

Date: 20 October 2026.

Status: implemented; checkpoint verification is recorded in
[verification.md](verification.md). The full audit release gates remain open.

## Changes

- **DATA-01:** compare device/inode identities before local work, backup and
  source removal. In-place copying opens both files without truncating, compares
  those descriptors, then truncates. In-place delta application checks its open
  source/output descriptors too. Same-path, `./` and hard-link aliases fail with
  a partial-transfer status while retaining their data.
- **DATA-04:** backup preparation returns errors to the replacement caller.
  Regular files, remote whole files/deltas/packs, io_uring packs, symlinks,
  special files, hard-link followers and alternate-destination copies stop on
  backup failure. A new backup is built beside the existing backup and renamed
  into place only after construction succeeds. Backup names aliasing the
  original inode are refused. Symlink backups preserve the link itself.
- **SEC-02:** source removal is checked at both the acknowledgement handler and
  the filesystem helper. Disabled removal and dry runs cannot unlink sources.
  A sender-owned ledger records the paths and metadata of listed non-directory
  entries. A peer's relative path is only a lookup key; its size and timestamps
  are ignored. Unknown roots/paths and duplicate acknowledgements report errors.
- **SEC-02 / DATA-06:** remote removals wait until a successful transfer has
  finished. An early `ReceiverDone`, sender error, receiver error or disconnect
  leaves sources in place. Before unlinking, the helper compares type,
  device/inode, size, mtime seconds and nanoseconds with the sender's snapshot.
  Same-second edits and same-size inode replacement therefore prevent removal.
- Sender control-send failures now contribute to its error count, preventing a
  failed send from authorizing source removal. Full send-error cancellation and
  metadata chunking remain milestone 3 work.

The checkpoint is reconciled with v0.4.0 (`eeb6955`), preserving protocol 10,
`RemovalsDone`, live local copy progress and literal-byte statistics. No wire
schema change is introduced. The Rust `remove_source` helper now takes `&Meta`
instead of peer-style size/seconds arguments.

## Verification coverage

`tests/mutation_safety.rs` covers both local scanners, same-file aliases,
whole-file and delta in-place modes, direct descriptor guards, directory
self-sync, invalid backup directories, collisions with the original inode,
failed atomic backup replacement, unreadable backup sources and successful
retry. Backup-failure cases run locally and through fake-SSH push/pull with
both threaded and Tokio I/O, small and large files, and the io_uring option.

`tests/source_ack_safety.rs` uses malicious peers with postcard and rkyv. It
checks disabled removal, dry run, absolute and traversal paths, unknown roots,
duplicate acknowledgements, changed/replaced sources and premature completion.
It also verifies that valid authorized removal uses the sender's metadata.

`tests/remote_lifecycle.rs` and the existing rsync/xattr suites cover successful
removal of regular files, symlinks, hard links and deferred xattrs. New CLI
regressions have a 20-second deadline and terminate their own process group on
failure. Protocol probes have 10-second receive/completion deadlines.

## Remaining acceptance work

- Descriptor-based confinement and protection from concurrent parent-directory
  or symlink replacement are **not implemented**. SEC-01 remains open.
- Acknowledgements still use the protocol-10 path-shaped key. Transfer IDs,
  per-entry lifecycle validation and bound source descriptors remain part 1B.
- The snapshot comes from listing metadata. Ctime, the identity of dereferenced
  symlinks and verification against the file actually transferred still need
  work. This is not a filesystem snapshot and does not close DATA-06.
- Backup paths colliding with a different active output need namespace-wide
  checks. ENOSPC and injected short-write failures still need dedicated tests.
  These changes do not establish crash durability or directory fsync guarantees.
- Linux io_uring execution and cross-filesystem backup fallback need Linux and
  cross-volume verification; selecting the flag on macOS does not exercise it.

Remote source removal is deliberately conservative: a failed session retains
all its sources, even if some individual files arrived successfully. This is a
behavior change from removing each source as its acknowledgement arrives.
