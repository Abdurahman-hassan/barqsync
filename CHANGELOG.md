# Changelog

## Unreleased

### Compatibility

- Remote transfers now use protocol 11: the new options travel to the other side, which
  also reports vanished files and `--max-delete` stops. Upgrade barqsync on both machines
  together; this release cannot communicate with 0.4.x.

### Added

- rsync command lines that failed with "unexpected argument" now run unchanged:
  - `--no-OPTION` (`--no-o`, `--no-g`, `--no-p`, `--no-t`, `--no-D`, `--no-r`, `--no-l`, and
    more; see `--help`), applied in command-line order as rsync does, so `-a --no-o --no-g`
    works for NAS, exFAT and SMB targets. Options may also be repeated (`-a -a`, `-P --partial`).
  - `--delete-before`, `--delete-during`/`--del`, `--delete-delay` and `--delete-after`. All
    of them delete after the transfer, once the whole source was read, as `--delete`
    always has; see the implementation notes.
  - `--max-delete=N`: at most N deletions, in rsync's order; the rest are reported and the
    run ends with exit 25.
  - rsync's filter rules: `--filter`/`-f` with `-`, `+`, `H`, `S`, `P`, `R` and `!` rules,
    their modifiers, `.` merge files and `:` per-directory merge files; `-F`, `-FF` and
    `-C`/`--cvs-exclude`. `--include`/`--exclude` also take rsync's `+ `/`- ` prefixes.
  - `-m`/`--prune-empty-dirs`, `--mkpath`, and `-s`/`--secluded-args`/`--protect-args`
    (accepted; barqsync never puts paths on the remote command line).
  - `--timeout=SECONDS`: a remote transfer whose connection delivers nothing for that
    long ends with exit 30. Both sides send keepalives while busy.
- A source file that disappears between the scan and its transfer is now a warning
  ("file has vanished") and exit 24, as with rsync, instead of an error and exit 23.
- `--stats` also prints rsync's "Total bytes sent/received" lines, and `-v` or `--stats`
  rsync's "sent N bytes  received M bytes" and "total size is" summary lines.

### Fixed

- `--delete` no longer removes what the filter rules protect inside a directory it deletes:
  with `--exclude='*.log'`, `old/keep.log` stays, and so does `old` ("cannot delete
  non-empty directory"), as with rsync. Before, the whole directory was removed. The same
  holds when a directory is replaced by a file, which is then reported as rsync does
  ("could not make way for new regular file").
- `--delete` without `-r` or `-d` is refused (exit 1), as rsync does, instead of deleting
  from the destination's top directory.
- Without `-r` or `-d`, a `src/` argument is skipped ("skipping directory .") instead of
  copying the files in it, as rsync does; "skipping directory" no longer needs `-v`.
- A source that resolves to nothing no longer creates the destination directory.
- An `--exclude-from`/`--include-from` file that can't be read now ends with exit 11, as
  with rsync, instead of 1.

## 0.4.2 - 2026-10-21

### Compatibility

- Remote transfers continue to use protocol 10. Upgrade both ends to 0.4.2 to get
  the improved interruption and shutdown behavior on both machines.

### Fixed

- A remote transfer whose other side was interrupted now ends with exit 20, as rsync
  does. Before, it could end with exit 12 and a connection error such as `peer closed
  connection without sending TLS close_notify`. This happened with parallel streams
  (`--streams`, relayed streams, QUIC) when the data streams' end arrived before the
  interrupt notice on the control connection. Over the ssh pipe alone, the same
  happened when a write to the stopped side failed first. Now a data stream that ends
  early, or a failed write, waits up to 2 seconds for the control connection to say why
  (interrupted, a fatal error, or closed). A side that stops also sends its notice
  before it closes its data streams.
- `--transport quic` now ends as quickly as the other transports when the other side
  dies or stops responding:
  - A dead server is noticed by its ssh connection's end, after about 2–4 seconds
    (exit 12). Before, only QUIC's idle timeout noticed it, after 15–18 seconds.
  - If the ssh connection drops while QUIC still works, the transfer now ends too
    (exit 12), as rsync's does when its ssh dies. Before, it carried on over QUIC.
  - Ctrl-C with a stopped server now ends the client within about 3–4 seconds, as
    over ssh (7–9 seconds before). An interrupted or failed side still waits for a
    peer that answers, so that the peer gets the notice, but not for one from which
    nothing at all has arrived for a second.

## 0.4.1 - 2026-10-21

### Fixed

- `--delete` now waits for the complete source scan. In 0.4.0, a destination entry
  could be removed before a later directory produced a source-read error. Local,
  push and pull transfers now preserve all pending deletion candidates on such an
  error. Remote scan completion and listing counts are checked before deletion;
  replaced destination entries and entries selected by another root are kept.
- Ctrl-C during remote connection cleanup now returns exit 20 even if the transfer
  had already selected another status before the interrupt arrived.

### Documentation

- All documentation except the README and this changelog moved into `docs/`, with an index
  in `docs/README.md`:
  - `IMPLEMENTATION.md` is now `docs/implementation.md`;
  - `IMPLEMENTATION_PLAN.md` is now `docs/audit/findings.md`;
  - `REMAINING.md` is now `docs/remaining.md`;
  - the original and as-built plans and the fast_rsync comparison have shorter names;
  - the audit's work records and probes are under `docs/audit/`.

## 0.4.0 - 2026-10-21

### Compatibility

- Remote transfers now use protocol 10. Upgrade barqsync on both machines together;
  0.4.x cannot communicate with 0.3.x or earlier releases.

### Security

- The receiving side of a remote transfer now checks what the sending side tells it
  before acting on it. Every file name must be a single path component (not empty, `.` or
  `..`, with no `/` or NUL byte), each name may appear once per directory, and every
  directory listing must belong to a directory the receiver already accepted. Anything
  else stops the transfer with a protocol error (exit code 12) before any file is
  changed. A malicious or compromised remote host could otherwise make a pull write
  outside its destination. Names that are not UTF-8 are still accepted.
- When several sources are transferred into one destination, an entry of one source can
  no longer replace a directory another source is writing into, and with `-K` a symlink
  the transfer itself made never stands in for a directory. Sources could otherwise
  redirect each other's files outside the destination.
- With `--remove-source-files`, the sending side now removes only entries it listed itself
  in the same run, each at most once, and judges whether they changed by its own records.
  A request from the receiving side to remove anything else, or any removal request when
  `--remove-source-files` is off or in a dry run, is refused and nothing is removed.
  Before, a malicious or compromised receiving side could make the sending side delete
  files outside the transfer, even without `--remove-source-files`.

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

- A remote transfer only puts a file in place once all of its data has arrived: the data
  must match the size the sender announced, small-file packs must add up exactly, and
  delta copies must lie inside the basis file and the new file. A file that fails these
  checks, or whose data could not be written, is discarded and reported, and the transfer
  exits with code 23. Such a file used to be kept with missing or zero-filled data.
- Messages larger than 64 MiB, such as the listing of a directory with about a million
  entries, the delta signatures of many large files, or a long `--files-from` list, are
  now sent in several frames. They used to be dropped, which left both sides waiting or
  failed the handshake. A message over 1 GiB now stops the transfer on both sides with a
  clear error.
- A file listed as small (128 KiB or less) that grows before it is sent now goes as a
  large file instead of being read into memory whole.
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
- Ctrl-C and a lost connection now end a remote transfer within a few seconds on both
  sides, with rsync's exit codes, and leave no `barqsync --server` or `--stream-relay`
  running. Ctrl-C with a server that stopped reading used to hang the client forever;
  an interrupted client could exit 0, and a client whose server was interrupted exited 12
  (both now 20, as rsync does). An interrupted side now tells the other one, which stops
  as well. A connection whose writing side fails (a pipe cut between the two) is now
  noticed instead of waiting forever for answers; closing a connection and waiting for
  the remote shell and the stream relays are bounded.

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
