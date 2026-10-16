#!/usr/bin/env bash
# Benchmark fastrsync against rsync with hyperfine.
#
#   bench/gen.py  BENCH_DIR A B C      # create datasets first
#   bench/run.sh  BENCH_DIR [A|B|C|all] [--cold]
#
# --cold drops the OS page cache before every run (macOS: `sudo purge`,
# Linux: drop_caches) so disk I/O is measured, not RAM; it needs sudo.
# Env: RSYNC=/path/to/rsync  FASTRSYNC=/path/to/fastrsync  RUNS=5
set -euo pipefail

DIR=${1:?usage: bench/run.sh BENCH_DIR [A|B|C|all] [--cold]}
WHICH=${2:-all}
COLD=${3:-}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
FR=${FASTRSYNC:-$ROOT/target/release/fastrsync}
RUNS=${RUNS:-5}
OUT=$ROOT/bench/results
mkdir -p "$OUT"

if [[ -z "${RSYNC:-}" ]]; then
  for c in /opt/homebrew/bin/rsync /usr/local/bin/rsync /usr/bin/rsync; do
    v=$("$c" --version 2>/dev/null || true)
    if [[ "$v" == "rsync  version 3"* ]]; then RSYNC=$c; break; fi
  done
fi
[[ -n "${RSYNC:-}" ]] || { echo "need rsync 3.x (openrsync is not a fair baseline); set RSYNC=" >&2; exit 1; }
[[ -x "$FR" ]] || { echo "build first: cargo build --release" >&2; exit 1; }

if [[ "$COLD" == "--cold" ]]; then
  if [[ $(uname) == Darwin ]]; then CACHE=" && sync && sudo purge"; else CACHE=" && sync && echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null"; fi
  sudo -v
else
  CACHE=""
fi

rv=$("$RSYNC" --version); echo "rsync:     $RSYNC (${rv%%$'\n'*})"
echo "fastrsync: $FR ($("$FR" --version))"
echo "machine:   $(uname -sm), $(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu) cpus, cache: ${COLD:-warm}"
echo

tag=$(date +%Y%m%d-%H%M%S)${COLD:+-cold}

if [[ "$WHICH" == A || "$WHICH" == all ]]; then
  [[ -d "$DIR/A_src" ]] || { echo "missing $DIR/A_src (run bench/gen.py $DIR A)" >&2; exit 1; }
  echo "== A: small-file storm, fresh copy ($(find "$DIR/A_src" -type f | wc -l | tr -d ' ') files)"
  hyperfine --runs "$RUNS" --warmup 1 \
    --prepare "rm -rf '$DIR/A_dst'$CACHE" \
    -n "rsync -a" "'$RSYNC' -a '$DIR/A_src/' '$DIR/A_dst/'" \
    -n "fastrsync -a" "'$FR' -a '$DIR/A_src/' '$DIR/A_dst/'" \
    -n "rsync -a --inplace" "'$RSYNC' -a --inplace '$DIR/A_src/' '$DIR/A_dst/'" \
    -n "fastrsync -a --inplace" "'$FR' -a --inplace '$DIR/A_src/' '$DIR/A_dst/'" \
    --export-markdown "$OUT/A-$tag.md"
fi

if [[ "$WHICH" == B || "$WHICH" == all ]]; then
  [[ -f "$DIR/B_src.img" ]] || { echo "missing $DIR/B_src.img (run bench/gen.py $DIR B)" >&2; exit 1; }
  echo "== B: big file with scattered in-place changes ($(du -h "$DIR/B_src.img" | cut -f1))"
  # Each run starts from a fresh copy of the old version (a CoW clone where possible).
  if [[ $(uname) == Darwin ]]; then CLONE="cp -c"; else CLONE="cp --reflink=auto"; fi
  RESET="rm -f '$DIR/B_work.img' && $CLONE '$DIR/B_dst.img' '$DIR/B_work.img' && touch -t 200001010000 '$DIR/B_work.img'"
  hyperfine --runs "$RUNS" \
    --prepare "$RESET$CACHE" \
    -n "rsync -a" "'$RSYNC' -a '$DIR/B_src.img' '$DIR/B_work.img'" \
    -n "rsync -a --no-whole-file" "'$RSYNC' -a --no-whole-file '$DIR/B_src.img' '$DIR/B_work.img'" \
    -n "fastrsync -a" "'$FR' -a '$DIR/B_src.img' '$DIR/B_work.img'" \
    -n "fastrsync -a --no-whole-file" "'$FR' -a --no-whole-file '$DIR/B_src.img' '$DIR/B_work.img'" \
    -n "fastrsync -a --no-whole-file --no-clone" "'$FR' -a --no-whole-file --no-clone '$DIR/B_src.img' '$DIR/B_work.img'" \
    -n "fastrsync -a --no-clone" "'$FR' -a --no-clone '$DIR/B_src.img' '$DIR/B_work.img'" \
    --export-markdown "$OUT/B-$tag.md"
  rm -f "$DIR/B_work.img"
fi

if [[ "$WHICH" == C || "$WHICH" == all ]]; then
  [[ -d "$DIR/C_src" ]] || { echo "missing $DIR/C_src (run bench/gen.py $DIR C)" >&2; exit 1; }
  echo "== C: near no-op sync, 3 changed files ($(find "$DIR/C_src" -type f | wc -l | tr -d ' ') files)"
  "$FR" -a "$DIR/C_src/" "$DIR/C_dst/"
  MUT="printf x >> '$DIR/C_src/dir_00001/file_00001.bin'; printf x >> '$DIR/C_src/dir_02500/file_00050.bin'; printf x >> '$DIR/C_src/dir_04999/file_00099.bin'"
  hyperfine --runs "$RUNS" --warmup 1 \
    --prepare "$MUT$CACHE" \
    -n "rsync -a" "'$RSYNC' -a '$DIR/C_src/' '$DIR/C_dst/'" \
    -n "fastrsync -a" "'$FR' -a '$DIR/C_src/' '$DIR/C_dst/'" \
    --export-markdown "$OUT/C-$tag.md"
fi

echo
echo "results written to $OUT/*-$tag.md"
