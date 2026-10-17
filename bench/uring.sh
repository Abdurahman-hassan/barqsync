#!/usr/bin/env bash
# --io-uring vs the normal write path for received small files (Linux container):
#   bench/net/container.sh shell, then: bash bench/uring.sh
# Remote push through a fake ssh (receiver = the server, on this machine) of 20,000
# files of 4 KiB, onto tmpfs and onto the VM's disk; wall time (hyperfine) and the
# syscall count of the whole transfer (strace -f -c).
set -euo pipefail
RUNS=${RUNS:-5}
FR=/target/release/fastrsync
OUT=/work/bench/results
tag=$(date +%Y%m%d-%H%M%S)
REPORT="$OUT/uring-$tag.md"
cargo build --release -q
W=/bench/uring
python3 - "$W" <<'EOF'
import os, random, sys
w = sys.argv[1]
if not os.path.isdir(w):
    r = random.Random(1)
    for d in range(20):
        os.makedirs(f"{w}/d{d:04d}")
        for i in range(1000):
            open(f"{w}/d{d:04d}/f{i:05d}", "wb").write(r.randbytes(4096))
EOF
printf '#!/bin/sh\nshift\nexec /bin/sh -c "$*"\n' > /tmp/fake-ssh; chmod +x /tmp/fake-ssh
R="$FR -a -e /tmp/fake-ssh --remote-path=$FR"
{
  echo "# --io-uring ($tag)"
  echo
  echo "Remote push (fake ssh) of 20,000 x 4 KiB files; $RUNS runs. Kernel $(uname -r)."
  echo
} > "$REPORT"
for target in /dst/uring /data/uring; do
  mkdir -p "$(dirname "$target")"
  echo "== $target"
  hyperfine --runs "$RUNS" --warmup 1 --prepare "rm -rf $target" --style basic \
    -n "normal writes" "$R $W/ fakehost:$target/" \
    -n "io_uring batches" "$R --io-uring $W/ fakehost:$target/" \
    --export-markdown /tmp/uring.md
  { echo "## onto $(df --output=fstype "$(dirname "$target")" | tail -1) ($target)"; echo; cat /tmp/uring.md; echo; } >> "$REPORT"
  for mode in "" "--io-uring"; do
    rm -rf "$target"
    strace -f -c -S calls -o /tmp/strace.txt $R $mode "$W/" "fakehost:$target/" >/dev/null 2>&1
    total=$(awk '$NF == "total" {print $4}' /tmp/strace.txt | tail -1)
    topcalls=$(sed -n '3,9p' /tmp/strace.txt | awk '{printf "%s %s, ", $NF, $4}')
    echo "- syscalls ${mode:-normal}: $total ($topcalls)" >> "$REPORT"
  done
  echo >> "$REPORT"
done
cat "$REPORT"
