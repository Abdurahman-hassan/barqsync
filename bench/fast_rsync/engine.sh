#!/usr/bin/env bash
# In-process delta engine comparison (no network), 3 runs per dataset. Needs setup.sh.
#   bench/fast_rsync/engine.sh > engine.txt
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
W=${W:-/bench}
E=$HERE/target/release/engine
for run in 1 2 3; do
  for wl in ${ENGINE_WORKLOADS:-inplace shift append sqlite}; do
    f=$(ls "$W/data/$wl/new")
    echo "== run $run $wl"
    "$E" "$W/data/$wl/old/$f" "$W/data/$wl/new/$f"
  done
  echo "== run $run mid (m000)"
  "$E" "$W/data/mid/old/m000.bin" "$W/data/mid/new/m000.bin"
done
