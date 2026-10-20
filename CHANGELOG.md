# Changelog

## 0.4.0 - 2026-10-20

### Compatibility

- Remote transfers now use protocol 10. Upgrade barqsync on both machines together;
  0.4.x cannot communicate with 0.3.x or earlier releases.

### Added

- `BARQSYNC_TIMING=1` prints timestamped milestones of a remote transfer from both sides
  (handshake, transport setup, done messages, each step of closing), and
  `bench/net/container.sh fixed` uses it to break a one-file push and pull into connect,
  setup, transfer and close.

### Changed

- QUIC transfers cost about two round trips more than the SSH pipe instead of three to
  five: the sender closes as soon as the receiver reports it is done, nothing waits for
  acknowledgements, and the client no longer waits for the server to acknowledge its
  connection token.
- `--transport auto` connects to QUIC while its SSH ping is in flight, and remembers for an
  hour hosts where a QUIC transfer saw a clean link, which then stay on the SSH pipe without
  a ping. SSH remains the default.

### Fixed

- A pull with `--remove-source-files` now prints the sender's removals with `-vv` and exits
  with code 23 when a removal fails, as rsync does; it used to print nothing and exit 0.
- Parallel streams (`--streams`, `--transport tcp`) now work behind a firewall that only
  lets ssh in, such as the macOS firewall: when direct connections are refused, the
  streams travel over extra ssh connections instead of falling back to the single ssh
  pipe. No firewall exception is needed. This needs ssh keys or an agent on the remote
  host, and barqsync with this fix on both machines.
- `~` at the start of a remote path (`host:~/dir/`) or of `--remote-path` now means the
  remote home directory, as with rsync.
- A big local copy to a disk without clones now moves the progress line as it is copied,
  not only when it is done.
- `--stats` now counts a file sent or copied whole as literal data, as rsync does
  ("Literal data" was 0 unless a delta was used).
- Clearer errors: barqsync missing on the remote host (with a hint about `~/.zshenv`), a
  version mismatch (naming both versions), and a host with an empty user name (`@host`).
- A file synced onto itself (`--inplace -I f f`, another path to it, or a hard link to it
  in the destination) is left intact; `--inplace` used to empty it. With
  `--remove-source-files`, a source that is also the destination is kept and reported
  ("Skipping sender remove of destination file", exit 23), as rsync does; it used to be
  deleted.
- `--backup` no longer replaces a file whose backup could not be made: the error is
  reported (exit 23) and the old version stays. It used to be replaced anyway, losing the
  old version. A new backup is written under a temporary name and renamed over the
  previous one, so a failed backup leaves the previous one in place. A backup that would
  replace the file itself or the source being copied (`-b f~ f`) is refused, and an empty
  `--suffix` without `--backup-dir` is rejected, as rsync does.
- With `--backup`, files replaced by a hard link (`-H`) or by a device or special file are
  now backed up first, as rsync does; their old versions used to be lost.
- `--remove-source-files` now keeps a source that changed after it was listed even when
  its size and modification second stayed the same: the sender compares the file type,
  device and inode, size and modification time to the nanosecond. It used to compare size
  and whole seconds, and only for regular files.
- Remote transfers with `--threads=1` no longer hang: the protocol loop waited on the
  only worker thread for messages that needed that thread's queued work.
- `--delete` no longer deletes after an error reading the source side (a dangling symlink
  under `-L`, an unreadable entry, attributes or ACLs that can't be read, a missing
  source): such a listing lacks entries the source has. As rsync does, barqsync prints
  "IO error encountered -- skipping file deletion", deletes nothing more and exits with
  code 23, locally and in both directions of a remote transfer. `--scanner=jwalk` also
  stopped dropping entries it could not stat without a word.
- Several sources into one destination (`barqsync -a --delete a/ b/ dst/`) are now one
  tree, as in rsync: `--delete` keeps whatever any source has, a name in several comes
  from the first one, a directory wins over a file of the same name, and directories of
  the same name merge their contents. Each source used to be synced on its own, so
  `--delete` removed the other sources' files (only `b`'s survived), and remotely the
  sources raced each other. This covers local transfers with both scanners, pushes and
  pulls, named items (`a/ c dst/`) and `-R` paths from several bases.

### Security

- With `--remove-source-files`, the sending side now removes only entries it listed itself
  in the same run, each at most once, and judges whether they changed by its own records.
  A request from the receiving side to remove anything else, or any removal request when
  `--remove-source-files` is off or in a dry run, is refused and nothing is removed.
  Before, a malicious or compromised receiving side could make the sending side delete
  files outside the transfer, even without `--remove-source-files`.

## 0.3.0 - 2026-10-20

### Compatibility

- Remote transfers now use protocol 9. Upgrade barqsync on both machines together;
  version 0.3.x cannot communicate with 0.2.x or earlier releases.

### Added

- Preserve extended attributes with `-X`/`--xattrs` (including `-XX`) and ACLs with
  `-A`/`--acls`, locally and remotely, following rsync's platform-specific rules.
  Attribute-only updates are supported, and long attribute values are fetched only
  when needed.
- Select QUIC automatically on long links with `--transport auto`, with a bounded
  connection attempt and SSH fallback where UDP is blocked. SSH remains the default.
- Automated builds, tests and linting on macOS and Linux, plus release archives,
  checksums and installers for macOS and Linux on arm64 and x86_64.

### Fixed

- APFS clones no longer retain source extended attributes unless `-X` requests them.
- Network benchmark profiles clear the remembered transport choice when warming
  automatic transport selection.
- Benchmark reports no longer include the test host's address.
