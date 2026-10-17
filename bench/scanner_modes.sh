#!/usr/bin/env bash
# Compare the two scanners (`--scanner=dirtask` vs `--scanner=jwalk`) on many small files.
#
#   bench/gen.py BENCH_DIR A C          # datasets first
#   bench/scanner_modes.sh BENCH_DIR [local|remote|all]
#
# A: 200k x 4 KiB files, fresh copy. C: 500k x 1 KiB files, near no-op (3 files changed),
# also with --delete. Remote runs go through a local fake-ssh shim (the receiver scans
# the destination). Env: BARQSYNC=/path/to/barqsync  RUNS=5
set -euo pipefail

DIR=${1:?usage: bench/scanner_modes.sh BENCH_DIR [local|remote|all]}
WHICH=${2:-all}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
FR=${BARQSYNC:-$ROOT/target/release/barqsync}
RUNS=${RUNS:-5}
OUT=$ROOT/bench/results
mkdir -p "$OUT"
[[ -x "$FR" ]] || { echo "build first: cargo build --release" >&2; exit 1; }
[[ -d "$DIR/A_src" && -d "$DIR/C_src" ]] || { echo "missing A_src/C_src (run bench/gen.py $DIR A C)" >&2; exit 1; }
printf '#!/bin/sh\nshift\nexec /bin/sh -c "$*"\n' > "$DIR/fake-ssh"; chmod +x "$DIR/fake-ssh"
tag=$(date +%Y%m%d-%H%M%S)
# Touch three files: in the first, middle and last directory (whatever the --scale).
dirs=$(ls "$DIR/C_src" | sort)
pick() { awk -v n="$1" 'NR == n' <<<"$dirs"; }
count=$(wc -l <<<"$dirs" | tr -d ' ')
MUT=""
for d in "$(pick 1)" "$(pick $(((count + 1) / 2)))" "$(pick "$count")"; do
  f=$(ls "$DIR/C_src/$d" | head -1)
  MUT+="printf x >> '$DIR/C_src/$d/$f'; "
done
RSH="-e '$DIR/fake-ssh' --remote-path='$FR'"

for where in local remote; do
  [[ "$WHICH" == "$where" || "$WHICH" == all ]] || continue
  if [[ $where == remote ]]; then host=fakehost:; rsh=$RSH; else host=; rsh=; fi
  echo "== A $where: fresh copy of $(find "$DIR/A_src" -type f | wc -l | tr -d ' ') files"
  hyperfine --runs "$RUNS" --warmup 1 --prepare "rm -rf '$DIR/S_dst'" \
    -n "dirtask" "'$FR' -a --scanner=dirtask $rsh '$DIR/A_src/' $host'$DIR/S_dst/'" \
    -n "jwalk" "'$FR' -a --scanner=jwalk $rsh '$DIR/A_src/' $host'$DIR/S_dst/'" \
    --export-markdown "$OUT/scanner-A-$where-$tag.md"
  rm -rf "$DIR/S_dst"
  echo "== C $where: near no-op of $(find "$DIR/C_src" -type f | wc -l | tr -d ' ') files"
  "$FR" -a "$DIR/C_src/" "$DIR/S_dst/"
  hyperfine --runs "$RUNS" --warmup 1 --prepare "$MUT" \
    -n "dirtask" "'$FR' -a --scanner=dirtask $rsh '$DIR/C_src/' $host'$DIR/S_dst/'" \
    -n "jwalk" "'$FR' -a --scanner=jwalk $rsh '$DIR/C_src/' $host'$DIR/S_dst/'" \
    -n "dirtask --delete" "'$FR' -a --delete --scanner=dirtask $rsh '$DIR/C_src/' $host'$DIR/S_dst/'" \
    -n "jwalk --delete" "'$FR' -a --delete --scanner=jwalk $rsh '$DIR/C_src/' $host'$DIR/S_dst/'" \
    --export-markdown "$OUT/scanner-C-$where-$tag.md"
  rm -rf "$DIR/S_dst"
done
echo "results written to $OUT/scanner-*-$tag.md"
