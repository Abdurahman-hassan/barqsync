#!/usr/bin/env bash
# Compare the two delta designs (`--delta=blocks` vs `--delta=cdc`) on big files.
#
#   bench/gen.py BENCH_DIR B E          # datasets first
#   bench/delta_modes.sh BENCH_DIR [local|remote|all]
#
# B: 10 GiB with 50 x 1 MiB in-place overwrites. E: 2 GiB with inserts and deletes.
# Each run updates a fresh clone of the old version. Runs are interleaved (one of each
# command per round) because big sequential writes slow down as the SSD's write cache
# fills, which would favour whichever command ran first. Remote runs go through a local
# fake-ssh shim: they measure signatures, matching and the protocol, not a network.
# Env: BARQSYNC=/path/to/barqsync  RUNS=3  DATASETS="B E"
set -euo pipefail

DIR=${1:?usage: bench/delta_modes.sh BENCH_DIR [local|remote|all]}
WHICH=${2:-all}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
FR=${BARQSYNC:-$ROOT/target/release/barqsync}
RUNS=${RUNS:-3}
OUT=$ROOT/bench/results
mkdir -p "$OUT"
[[ -x "$FR" ]] || { echo "build first: cargo build --release" >&2; exit 1; }
if [[ $(uname) == Darwin ]]; then CLONE="cp -c"; else CLONE="cp --reflink=auto"; fi
printf '#!/bin/sh\nshift\nexec /bin/sh -c "$*"\n' > "$DIR/fake-ssh"; chmod +x "$DIR/fake-ssh"

now() { python3 -c 'import time; print(time.monotonic())'; }

tag=$(date +%Y%m%d-%H%M%S)
report=$OUT/delta-modes-$tag.md
{
  echo "# --delta=blocks vs --delta=cdc ($tag)"
  echo
  echo "$(uname -sm), $(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu) cpus, $RUNS interleaved runs, warm cache; barqsync $("$FR" --version)"
  echo
  echo "| Dataset | Where | Command | Mean (s) | Min (s) | Max (s) | Literal | Matched |"
  echo "|---|---|---|---:|---:|---:|---:|---:|"
} > "$report"

run_set() { # dataset where args...
  local ds=$1 where=$2; shift 2
  local cmds=("$@") n=${#@}
  local -a times lits mats
  local i r start end line
  for ((i = 0; i < n; i++)); do times[i]=""; done
  for ((r = 0; r < RUNS; r++)); do
    for ((i = 0; i < n; i++)); do
      rm -f "$DIR/${ds}_work.img"
      $CLONE "$DIR/${ds}_dst.img" "$DIR/${ds}_work.img"
      touch -t 200001010000 "$DIR/${ds}_work.img"
      sync
      start=$(now)
      line=$(eval "${cmds[i]}" --stats | grep -E '^(Literal|Matched) data' | tr '\n' ' ')
      end=$(now)
      cmp -s "$DIR/${ds}_src.img" "$DIR/${ds}_work.img" || { echo "MISMATCH: ${cmds[i]}" >&2; exit 1; }
      times[i]="${times[i]} $(python3 -c "print($end - $start)")"
      lits[i]=$(sed -E 's/.*Literal data: ([0-9,]+).*/\1/' <<<"$line")
      mats[i]=$(sed -E 's/.*Matched data: ([0-9,]+).*/\1/' <<<"$line")
      echo "  $ds $where round $((r + 1)): $(python3 -c "print(f'{$end - $start:.2f}')") s  ${cmds[i]%% \'$DIR*}  (literal ${lits[i]})"
    done
  done
  for ((i = 0; i < n; i++)); do
    read -r mean mn mx <<<"$(python3 -c "import sys; t=[float(x) for x in sys.argv[1:]]; print(f'{sum(t)/len(t):.2f} {min(t):.2f} {max(t):.2f}')" ${times[i]})"
    local label=${cmds[i]#\'$FR\' }; label=${label%% \'$DIR*}
    echo "| $ds | $where | \`$label\` | $mean | $mn | $mx | ${lits[i]} | ${mats[i]} |" >> "$report"
  done
}

for ds in ${DATASETS:-B E}; do
  [[ -f "$DIR/${ds}_src.img" ]] || { echo "missing $DIR/${ds}_src.img (run bench/gen.py $DIR $ds)" >&2; exit 1; }
  src="'$DIR/${ds}_src.img'"; dst="'$DIR/${ds}_work.img'"
  if [[ "$WHICH" == local || "$WHICH" == all ]]; then
    echo "== $ds local"
    run_set "$ds" local \
      "'$FR' -a --no-whole-file --delta=blocks $src $dst" \
      "'$FR' -a --no-whole-file --delta=cdc $src $dst" \
      "'$FR' -a --no-whole-file --no-clone --delta=blocks $src $dst" \
      "'$FR' -a --no-whole-file --no-clone --delta=cdc $src $dst"
  fi
  if [[ "$WHICH" == remote || "$WHICH" == all ]]; then
    echo "== $ds remote (fake ssh)"
    rsh="-e '$DIR/fake-ssh' --remote-path='$FR'"
    run_set "$ds" remote \
      "'$FR' -a --delta=blocks $src fakehost:$dst $rsh" \
      "'$FR' -a --delta=cdc $src fakehost:$dst $rsh"
  fi
done
rm -f "$DIR"/*_work.img
echo
cat "$report"
echo "results written to $report"
