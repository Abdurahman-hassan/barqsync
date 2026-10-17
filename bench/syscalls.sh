#!/usr/bin/env bash
# Syscall counts (original plan §8, step 3): `strace -c -f` for rsync vs barqsync.
# The plan's bar: barqsync should make at least 50% fewer syscalls than `rsync -a`.
# Linux only; run inside the container: bench/net/container.sh syscalls
set -euo pipefail
SCALE=${SCALE:-1}
W=/bench-sys
OUT=/work/bench/results
mkdir -p "$OUT" "$W"
tag=$(date +%Y%m%d-%H%M%S)
REPORT="$OUT/syscalls-$tag.md"
FR=/target/release/barqsync
cargo build --release -q

python3 - "$W" "$SCALE" <<'EOF'
import os, random, sys
w, scale = sys.argv[1], float(sys.argv[2])
def tree(root, dirs, files, size, seed):
    if os.path.isdir(root):
        return
    r = random.Random(seed)
    for d in range(dirs):
        os.makedirs(f"{root}/d{d:04d}")
        for i in range(files):
            with open(f"{root}/d{d:04d}/f{i:05d}", "wb") as f:
                f.write(r.randbytes(size))
tree(f"{w}/A", max(1, int(20 * scale)), 1000, 4096, 1)
tree(f"{w}/C", max(1, int(200 * scale)), 100, 1024, 2)
EOF

# count TOOL WORKLOAD CMD...: run under strace, print the total number of syscalls.
count() {
  local name=$1; shift
  strace -f -c -S calls -o "/tmp/strace-$name.txt" "$@" >/dev/null 2>&1
  # Columns: % time, seconds, usecs/call, calls, [errors], syscall.
  awk '$NF == "total" {print $4}' "/tmp/strace-$name.txt" | tail -1
}
top() {
  sed -n '3,10p' "/tmp/strace-$1.txt" | awk '{printf "%s %s, ", $NF, $4} END {print ""}'
}

{
  echo "# Syscall counts ($tag)"
  echo
  echo "| Workload | rsync | barqsync | barqsync / rsync |"
  echo "|---|---:|---:|---:|"
} > "$REPORT"

rm -rf /tmp/sys_r /tmp/sys_f
r=$(count rsync-A rsync -a "$W/A/" /tmp/sys_r/)
f=$(count fast-A "$FR" -a "$W/A/" /tmp/sys_f/)
echo "| A: copy $(find "$W/A" -type f | wc -l) × 4 KiB | $r | $f | $(awk "BEGIN{printf \"%.2f\", $f/$r}") |" >> "$REPORT"

rsync -a "$W/C/" /tmp/sysc_r/ && "$FR" -a "$W/C/" /tmp/sysc_f/
printf x >> "$W/C/d0000/f00000"
r=$(count rsync-C rsync -a "$W/C/" /tmp/sysc_r/)
printf x >> "$W/C/d0001/f00001"
f=$(count fast-C "$FR" -a "$W/C/" /tmp/sysc_f/)
echo "| C: no-op over $(find "$W/C" -type f | wc -l) files | $r | $f | $(awk "BEGIN{printf \"%.2f\", $f/$r}") |" >> "$REPORT"

{
  echo
  echo "Top syscalls (count):"
  echo
  for n in rsync-A fast-A rsync-C fast-C; do echo "- **$n**: $(top "$n")"; done
} >> "$REPORT"
cat "$REPORT"
