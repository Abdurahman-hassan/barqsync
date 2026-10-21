# barqsync documentation

Start with the [README](../README.md) for what barqsync does and how to use it, and the
[changelog](../CHANGELOG.md) for what each release changed. Everything else is here.

## Using and understanding barqsync

| Document | What it covers |
| --- | --- |
| [implementation.md](implementation.md) | Every supported option, how each part works (local engine, remote transfers, streams, QUIC, delta, scanner), the milestones, testing, benchmarks and releasing |
| [fast-rsync-comparison.md](fast-rsync-comparison.md) | How the delta engine was measured against fast_rsync and rsync, and the changes that came from it |

## Design history

| Document | What it covers |
| --- | --- |
| [original-plan.md](original-plan.md) | The original design plan the project started from |
| [as-built-plan.md](as-built-plan.md) | The same plan, section by section, next to what was built and what the measurements decided |

## The October 2026 audit and the fixes in 0.4.x

| Document | What it covers |
| --- | --- |
| [audit/findings.md](audit/findings.md) | The audit of v0.3.0: 18 findings (12 P1), how each was reproduced, and the fixes it proposed |
| [audit/README.md](audit/README.md) | The milestone tracker: which parts of the audit are done, partial or open |
| [audit/p1-coordination.md](audit/p1-coordination.md) | How the P1 fixes were split across PRs 15–19, where each finding landed, the 0.4.0 release checks and how to release next time |
| [audit/04-ssh-relay.md](audit/04-ssh-relay.md) | Parallel streams through ssh when a firewall blocks them (NET-01), verified on a real Mac |
| [audit/01a-mutation-safety.md](audit/01a-mutation-safety.md), [audit/03a-worker-coordination.md](audit/03a-worker-coordination.md), [audit/peer-input.md](audit/peer-input.md), [audit/06a-remote-locations.md](audit/06a-remote-locations.md) | Work records for the individual fixes |
| [audit/verification.md](audit/verification.md) | How the first fix checkpoint was verified |
| [audit/02a-deletion-barrier.md](audit/02a-deletion-barrier.md) | The 0.4.1 deletion barrier, late-interrupt fix, and real-Mac verification |
| [releases/0.4.2.md](releases/0.4.2.md) | The 0.4.2 release scope and publication checks |
| [audit/review/2026-10-20/](audit/review/2026-10-20/) | The audit's probes and their original output against v0.3.0 |

## What's next

[remaining.md](remaining.md): what is left after 0.4.2, in order. It covers the
security advisory, open items from the audit, rough edges and features not built yet.

## Elsewhere in the repository

- `bench/`: the benchmark scripts. Their reports are in `bench/results/`, and the docs above
  cite them by file name.
- `docs/images/`: the README's charts (`make_images.py` draws them from the measurements).
