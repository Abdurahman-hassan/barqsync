# Implementation milestones

This tracker turns [the audit plan](findings.md) into reviewable
parts. The original audit remains the record of the findings. A part is complete
only when its implemented scope and verification are recorded below; completing
a part does not close other acceptance criteria in the audit.

| Milestone | Parts | Status | Acceptance gate |
| --- | --- | --- | --- |
| 1. Mutation safety | 1A: same-file guards, mandatory atomic backups, source-removal authorization. 1B: descriptor confinement and sender-owned acknowledgement ledger. | 1A implemented; 1B partial (ledger, peer-input validation, multi-root guard; no descriptor confinement) | Preserve source, destination and prior backup on failure; reject unauthorized filesystem operations. |
| 2. Transfer correctness | 2A: merged source namespace and scan-completion deletion barrier. 2B: file/pack state validation and end-to-end integrity. 2C: full source identity/change tracking. | 2A implemented (#19: one tree for several sources, no deletion after read errors); 2B partial (file, pack and delta range validation; no end-to-end hash); 2C partial (#17: type, device, inode, size and nanosecond mtime) | Delete only against a complete source view; commit and acknowledge only verified files. |
| 3. Runtime and protocol | 3A: move coordination outside Rayon. 3B: cancellation and bounded shutdown. 3C: metadata chunking, error propagation and bounded stream admission. | 3A implemented; 3B implemented (#19: Ctrl-C and lost peers end both sides in bounded time; no `--timeout` yet); 3C partial (metadata chunking, send-failure propagation; no stream admission) | One-worker transfers finish; disconnects and oversized frames cannot hang a session. |
| 4. SSH data streams | 4A: versioned session negotiation and private Unix socket helpers. 4B: SSH lifecycle, fallback and diagnostics. 4C: signed release integration and real Mac acceptance. | Relay fallback implemented (#11) and accepted on a real Mac with its firewall on; 4A (no public listener) and release signing pending | Push/pull on two Macs with firewalls enabled, no public data listener and no leaked helpers. |
| 5. Resource safety and performance | 5A: byte-based memory budget. 5B: safe CDC reads of mutable files. 5C: measured large-file scheduling and live progress. | Pending | Bounded memory and clean source-change failures; publish measured performance tradeoffs. |
| 6. Compatibility and maintenance | 6A: remote path/IPv6 parsing. 6B: CI, dependency audits and fault injection. 6C: diagnostics, durability documentation and release verification. | 6A implemented; 6B/6C pending | Compatibility matrix, CI and packaged artifact checks pass. |

Dependencies follow the audit: milestone 4 requires milestones 1–3; performance
changes depend on correctness and cancellation. Keep wire-format changes together
with an explicit protocol version change and compatibility tests.

## Work records

- [Part 1A: mutation safety](01a-mutation-safety.md).
- [Part 3A: worker coordination](03a-worker-coordination.md).
- [Peer input (parts of 1B, 2B and 3C)](peer-input.md).
- [Part 6A: remote locations and filename compatibility](06a-remote-locations.md).
- [Checkpoint verification](verification.md).
- [Milestone 4: parallel streams through ssh](04-ssh-relay.md).
- [P1 fixes before 0.4.0: organization and status](p1-coordination.md).

## Baseline reconciliation

On 20 October, the checkpoint was reconciled with `origin/main` at `eeb6955`
(v0.4.0, protocol 10). PRs 11–14 are already merged. Their SSH relay fallback,
home-relative paths, progress and removal reporting must be preserved. The
audit's proposed `ssh-streams` transport and private Unix rendezvous are still
separate work; milestone 4 must assess the existing relay before adding another
transport. Milestone 6 must build on the existing tilde/host fixes.

The checkpoint was merged as [PR 15](https://github.com/Abdurahman-hassan/barqsync/pull/15)
at `b1b257a`, after macOS and Linux CI passed (including Linux QUIC). Part 6A
starts from that merge. The three active P1 worktrees remain with their owners;
their unfinished drafts were not included in this checkpoint or modified here.

## Verification outside this checkout

Linux-specific paths need Linux execution. Real-host/firewall tests require two
available Macs. Developer ID signing/notarization needs the project's release
credentials and distribution setup. None of these is considered passed by a
local fake-SSH test. Do not change firewall settings to make a test pass.
