#!/usr/bin/env python3
"""Run commands in interleaved rounds (one of each per round) and report wall times.

  bench/interleave.py --runs 3 --prepare 'rm -rf dst' --out report.md -- \\
      'name one' 'command one' 'name two' 'command two' ...

hyperfine runs all rounds of one command before the next. For write-heavy workloads that
biases results: the SSD's write cache fills as the benchmark goes on, so later commands
run on a slower disk. Interleaving spreads that over every command.
"""

import argparse
import statistics
import subprocess
import sys
import time


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--prepare", default="")
    ap.add_argument("--warmup", type=int, default=1)
    ap.add_argument("--out")
    ap.add_argument("pairs", nargs="+")
    a = ap.parse_args()
    if len(a.pairs) % 2:
        sys.exit("expected name/command pairs")
    cmds = list(zip(a.pairs[::2], a.pairs[1::2]))
    times = {n: [] for n, _ in cmds}
    for r in range(a.warmup + a.runs):
        for name, cmd in cmds:
            if a.prepare:
                subprocess.run(a.prepare, shell=True, check=True)
            t = time.monotonic()
            p = subprocess.run(cmd, shell=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            dt = time.monotonic() - t
            if p.returncode != 0:
                sys.exit(f"{name} failed ({p.returncode}): {p.stderr.decode(errors='replace')}")
            if r >= a.warmup:
                times[name].append(dt)
            print(f"round {r + 1 - a.warmup:>2} {name}: {dt:.2f} s" + (" (warmup)" if r < a.warmup else ""), flush=True)
    base = statistics.mean(times[cmds[0][0]])
    lines = ["| Command | Mean (s) | Min (s) | Max (s) | vs first |", "|---|---:|---:|---:|---:|"]
    for name, _ in cmds:
        t = times[name]
        m = statistics.mean(t)
        lines.append(f"| `{name}` | {m:.2f} | {min(t):.2f} | {max(t):.2f} | {(m / base - 1) * 100:+.1f}% |")
    report = "\n".join(lines) + "\n"
    print(report)
    if a.out:
        open(a.out, "w").write(report)


if __name__ == "__main__":
    main()
