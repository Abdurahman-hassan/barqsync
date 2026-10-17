#!/usr/bin/env bash
# Compare remote transports and wire formats through the local fake-ssh shim (no real
# link: this measures CPU and protocol overhead; bench/net/ adds link profiles).
#
#   bench/gen.py BENCH_DIR A C          # datasets first
#   bench/transport_modes.sh BENCH_DIR
#
# A: 200k x 4 KiB files, fresh push. C: 500k files, 3 changed (metadata-bound).
# Env: BARQSYNC=/path/to/barqsync  RUNS=3
set -euo pipefail

DIR=${1:?usage: bench/transport_modes.sh BENCH_DIR}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
FR=${BARQSYNC:-$ROOT/target/release/barqsync}
RUNS=${RUNS:-3}
OUT=$ROOT/bench/results
mkdir -p "$OUT"
[[ -x "$FR" ]] || { echo "build first: cargo build --release" >&2; exit 1; }
printf '#!/bin/sh\nshift\nexec /bin/sh -c "$*"\n' > "$DIR/fake-ssh"; chmod +x "$DIR/fake-ssh"
tag=$(date +%Y%m%d-%H%M%S)
R="'$FR' -a -e '$DIR/fake-ssh' --remote-path='$FR'"

dirs=$(ls "$DIR/C_src" | sort)
count=$(wc -l <<<"$dirs" | tr -d ' ')
MUT=""
for n in 1 $(((count + 1) / 2)) "$count"; do
  d=$(awk -v n="$n" 'NR == n' <<<"$dirs"); f=$(ls "$DIR/C_src/$d" | head -1)
  MUT+="printf x >> '$DIR/C_src/$d/$f'; "
done

modes=(
  "ssh, postcard|--transport ssh --wire postcard"
  "ssh, rkyv|--transport ssh --wire rkyv"
  "tcp x4, postcard|--transport tcp --streams 4"
  "quic x16, postcard|--transport quic"
  "quic x16, rkyv|--transport quic --wire rkyv"
)
args=()
for m in "${modes[@]}"; do args+=(-n "${m%%|*}" "$R ${m#*|} '$DIR/SRC/' fakehost:'$DIR/T_dst/'"); done

echo "== A: fresh push of $(find "$DIR/A_src" -type f | wc -l | tr -d ' ') files"
hyperfine --runs "$RUNS" --warmup 1 --prepare "rm -rf '$DIR/T_dst'" "${args[@]//SRC/A_src}" \
  --export-markdown "$OUT/transport-A-$tag.md"
rm -rf "$DIR/T_dst"
echo "== C: near no-op push of $(find "$DIR/C_src" -type f | wc -l | tr -d ' ') files"
"$FR" -a "$DIR/C_src/" "$DIR/T_dst/"
hyperfine --runs "$RUNS" --warmup 1 --prepare "$MUT" "${args[@]//SRC/C_src}" \
  --export-markdown "$OUT/transport-C-$tag.md"
rm -rf "$DIR/T_dst"
echo "results written to $OUT/transport-*-$tag.md"
