# Implementation milestones

This tracker turns [the audit plan](../../IMPLEMENTATION_PLAN.md) into reviewable
parts. The original audit remains the record of the findings. A part is complete
only when its implemented scope and verification are recorded below; completing
a part does not close other acceptance criteria in the audit.

| Milestone | Parts | Status | Acceptance gate |
| --- | --- | --- | --- |
| 1. Mutation safety | 1A: same-file guards, mandatory atomic backups, source-removal authorization. 1B: descriptor confinement and sender-owned acknowledgement ledger. | 1A implemented; 1B partial (ledger only) | Preserve source, destination and prior backup on failure; reject unauthorized filesystem operations. |
| 2. Transfer correctness | 2A: merged source namespace and scan-completion deletion barrier. 2B: file/pack state validation and end-to-end integrity. 2C: full source identity/change tracking. | Pending | Delete only against a complete source view; commit and acknowledge only verified files. |
| 3. Runtime and protocol | 3A: move coordination outside Rayon. 3B: cancellation and bounded shutdown. 3C: metadata chunking, error propagation and bounded stream admission. | 3A implemented; 3B/3C pending | One-worker transfers finish; disconnects and oversized frames cannot hang a session. |
| 4. SSH data streams | 4A: versioned session negotiation and private Unix socket helpers. 4B: SSH lifecycle, fallback and diagnostics. 4C: signed release integration and real Mac acceptance. | Pending | Push/pull on two Macs with firewalls enabled, no public data listener and no leaked helpers. |
| 5. Resource safety and performance | 5A: byte-based memory budget. 5B: safe CDC reads of mutable files. 5C: measured large-file scheduling and live progress. | Pending | Bounded memory and clean source-change failures; publish measured performance tradeoffs. |
| 6. Compatibility and maintenance | 6A: remote path/IPv6 parsing. 6B: CI, dependency audits and fault injection. 6C: diagnostics, durability documentation and release verification. | Pending | Compatibility matrix, CI and packaged artifact checks pass. |

Dependencies follow the audit: milestone 4 requires milestones 1–3; performance
changes depend on correctness and cancellation. Keep wire-format changes together
with an explicit protocol version change and compatibility tests.

## Work records

- [Part 1A: mutation safety](01a-mutation-safety.md).
- [Part 3A: worker coordination](03a-worker-coordination.md).
- [Checkpoint verification](verification.md).

## Baseline reconciliation

On 20 October, the checkpoint was reconciled with `origin/main` at `eeb6955`
(v0.4.0, protocol 10). PRs 11–14 are already merged. Their SSH relay fallback,
home-relative paths, progress and removal reporting must be preserved. The
audit's proposed `ssh-streams` transport and private Unix rendezvous are still
separate work; milestone 4 must assess the existing relay before adding another
transport. Milestone 6 must build on the existing tilde/host fixes.

## Verification outside this checkout

Linux-specific paths need Linux execution. Real-host/firewall tests require two
available Macs. Developer ID signing/notarization needs the project's release
credentials and distribution setup. None of these is considered passed by a
local fake-SSH test. Do not change firewall settings to make a test pass.
