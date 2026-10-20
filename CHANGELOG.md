# Changelog

## 0.4.0 - 2026-10-20

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
