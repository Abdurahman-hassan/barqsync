# Changelog

## Unreleased

### Compatibility

- Remote transfers now use protocol 10. Upgrade barqsync on both machines together;
  this version cannot communicate with 0.3.0 or earlier releases.

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
