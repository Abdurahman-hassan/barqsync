# Changelog

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
