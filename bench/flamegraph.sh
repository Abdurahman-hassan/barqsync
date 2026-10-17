#!/usr/bin/env bash
# Flamegraphs (original plan §8, step 3) of a barqsync run, written to bench/results/.
#
#   Linux (in the container):  bench/net/container.sh flamegraph [copy|noop|delta]
#   macOS (no root needed):    bench/flamegraph.sh [copy|noop|delta]
#
# Linux uses perf; macOS uses `sample`. Both collapse with inferno
# (`cargo install inferno` on the Mac). The binary is built with debug symbols.
set -euo pipefail
WL=${1:-copy}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=$ROOT/bench/results
mkdir -p "$OUT"
W=${W:-/tmp/barqsync-flame}
PROF_TARGET=${PROF_TARGET:-$ROOT/target-prof}
[[ -d /target ]] && PROF_TARGET=/target-prof
export CARGO_PROFILE_RELEASE_DEBUG=true CARGO_PROFILE_RELEASE_STRIP=false
cargo build --release -q --manifest-path "$ROOT/Cargo.toml" --target-dir "$PROF_TARGET"
FR=$PROF_TARGET/release/barqsync

python3 - "$W" <<'EOF'
import os, random, sys
w = sys.argv[1]
r = random.Random(7)
if not os.path.isdir(f"{w}/src"):
    for d in range(50):
        os.makedirs(f"{w}/src/d{d:03d}")
        for i in range(400):
            open(f"{w}/src/d{d:03d}/f{i:04d}", "wb").write(r.randbytes(4096))
if not os.path.exists(f"{w}/big_old.img"):
    data = bytearray(b"".join(r.randbytes(1 << 20) for _ in range(512)))
    open(f"{w}/big_old.img", "wb").write(data)
    for _ in range(16):
        off = r.randrange(0, len(data) - (1 << 20))
        data[off:off + (1 << 20)] = r.randbytes(1 << 20)
    open(f"{w}/big_new.img", "wb").write(data)
EOF

case "$WL" in
  copy) rm -rf "$W/dst"; CMD=("$FR" -a "$W/src/" "$W/dst/") ;;
  noop) "$FR" -a "$W/src/" "$W/dst/"; CMD=("$FR" -a "$W/src/" "$W/dst/") ;;
  delta) cp "$W/big_old.img" "$W/work.img"; touch -t 200101010000 "$W/work.img"
         CMD=("$FR" -a --no-whole-file --no-clone "$W/big_new.img" "$W/work.img") ;;
  *) echo "workload: copy | noop | delta" >&2; exit 1 ;;
esac

SVG="$OUT/flame-$(uname -s | tr 'A-Z' 'a-z')-$WL-$(date +%Y%m%d-%H%M%S).svg"
if [[ $(uname) == Linux ]]; then
  PERF=perf
  "$PERF" record -q -F 999 -g -o /tmp/perf.data -- "${CMD[@]}" >/dev/null
  "$PERF" script -i /tmp/perf.data 2>/dev/null | inferno-collapse-perf | inferno-flamegraph > "$SVG"
else
  command -v inferno-collapse-sample >/dev/null || { echo "need: cargo install inferno" >&2; exit 1; }
  "${CMD[@]}" >/dev/null &
  pid=$!
  sample "$pid" 30 1 -mayDie -file /tmp/barqsync.sample >/dev/null 2>&1 || true
  wait "$pid" || true
  inferno-collapse-sample /tmp/barqsync.sample | inferno-flamegraph > "$SVG"
fi
echo "flamegraph: ${SVG#$ROOT/}"
