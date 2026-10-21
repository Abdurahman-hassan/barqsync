# Checkpoint verification

Date: 20 October 2026. Host: macOS arm64, Rust 1.92.0.

The pending fixes were reviewed against current `origin/main` (`eeb6955`,
v0.4.0). PRs 11–14 already contain the preceding transport, progress and release
work. The original audit describes v0.3.0 and is historical evidence.

## Commands

```sh
env -u BARQSYNC_TEST_REMOTE cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
git diff --check
```

Results for the reconciled checkpoint:

| Check | Result |
| --- | --- |
| Full local suite | 210 reported passed, 0 failures; 3 real-host tests returned early, so 207 tests exercised their cases. |
| New mutation regressions | 8 tests passed. |
| New remote lifecycle regressions | 2 tests passed, including both wire encodings and both I/O backends. |
| New malicious-acknowledgement regressions | 1 test passed across 20 peer scenarios. |
| Clippy, all targets, warnings denied | Passed. |
| Diff whitespace check | Passed. |

The new tests also ran successfully before integration with v0.4.0. An initial sandboxed full-suite run
could not create TCP/UDP sockets; the same tests passed with local networking
permitted. No firewall configuration was changed.

The three `real_remote` tests return early without `BARQSYNC_TEST_REMOTE`; their
reported success is **not evidence of real-host testing**. Real Mac firewall
acceptance remains separate from these results.

## Checkpoint CI and merge

[PR 15](https://github.com/Abdurahman-hassan/barqsync/pull/15) passed
[CI](https://github.com/Abdurahman-hassan/barqsync/actions/runs/39486494027)
on macOS and Linux before merging into `main` at `b1b257a`. Both jobs built all
targets, ran the full suite and Clippy. Linux also ran the suite with
`BARQSYNC_TRANSPORT=quic`. These CI results add Linux coverage to the local
verification above; they do not establish real-host/firewall acceptance.
