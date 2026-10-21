# Part 6A: remote locations and filename compatibility

Date: 20 October 2026. Audit item: CLI-01. Baseline: `b1b257a`, the reviewed
checkpoint merged through PR 15. This part leaves the active P1 worktrees intact.

## Changes

`parse_location` now returns a checked result. It recognizes bracketed IPv6
addresses, with optional usernames and scope identifiers, and removes the
brackets only from the SSH destination argument. The colon after the closing
bracket separates the remote filename. Previously, `user@[::1]:/data` split at
the first colon inside the address.

All source and destination arguments are validated before local mutation or
remote shell startup. Empty users/hosts, option-like hosts, whitespace, control
characters, shell metacharacters, malformed brackets, invalid IPv6 literals and
non-UTF-8 hosts produce a syntax error. `host::module` and `rsync://` now fail
explicitly as unsupported daemon syntax. `--files-from` uses the checked parser
as well. Plain usernames, host aliases and scope names accept ASCII letters,
digits and `._-+`, without a leading `-`; SSH configuration can supply other login
names behind an alias.

Remote filename bytes remain an `OsString`; no lossy UTF-8 conversion or shell
evaluation is introduced. Absolute paths, empty paths, spaces, quotes, colons in
the path and trailing slashes retain their meaning. Local names with a colon
can be disambiguated by a leading `./`.

The remote-side tilde resolver and quoted executable handling from PR 11 remain
in place. `~`/`~/` use the remote home; known `~user/` prefixes use that remote
account's home, while unknown users remain literal. `--remote-path` permits only
the existing restricted leading tilde expansion and quotes the remaining text.
No protocol or dependency change is needed.

## Compatibility and verification

| Case | Coverage |
| --- | --- |
| IPv4, DNS names, SSH aliases, bracketed IPv6, scoped IPv6 | Parser matrix checks host and path independently. |
| Empty, home-relative, absolute, quoted and raw-byte paths | Parser matrix preserves filename bytes. |
| Local colon names | Parser cases retain local interpretation with `./` or an earlier slash. |
| Malformed and option-like hosts; daemon forms | Parser and CLI regressions require syntax failure before the fake shell starts. |
| Push and pull with IPv6 syntax, spaces, quotes and shell-like path text | Fake-SSH transfers verify exact file contents, the actual SSH destination argument, and absence of injected side effects. Both I/O backends and wire encodings run. |
| Non-UTF-8 filenames end to end | Linux-only transfer regression; APFS rejects these filenames. Raw-byte parser tests run on both platforms. |
| `~/` and `--remote-path=~/...` | Existing remote push/pull regression retained. |

Commands:

```sh
cargo test --locked --offline --lib paths::tests
env -u BARQSYNC_TEST_REMOTE cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
git diff --check
```

Local validation passed: 5 parser tests, the full macOS suite (214 reported
passed; 3 real-host tests returned early, leaving 211 exercised), all-targets
Clippy with warnings denied, and the diff whitespace check. An initial fixture
attempt using invalid UTF-8 filenames on APFS failed at directory creation;
that filesystem case is now an explicit Linux-only test, matching the existing
non-UTF-8 test policy.

Fake SSH establishes argument and filename compatibility; it does not establish routing
or authentication against a real IPv6 SSH server. The release and real-host
acceptance work in milestones 4 and 6C remains open.
