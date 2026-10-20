#!/usr/bin/env bash
# Real-transfer comparison of fast_rsync (through frsync), barqsync and rsync over ssh to
# the sshd in the "remote" network namespace (run setup.sh first):
#
#   bench/fast_rsync/netbench.sh net.tsv
#
# Profiles (the VM this was written on has no netem, so tbf limits the rate on both veth
# ends, and wan goes through relay.py, which delays every chunk 10 ms each way):
#   lan   1 Gbit, no added delay
#   wan   100 Mbit, 20 ms RTT
#
# Each run resets the destination to the old copy, times the push, counts the bytes that
# cross veth0 (tx = client to server, rx = server to client) and checks the result with
# diff -r. PROFILES, WORKLOADS, TOOLS and RUNS pick a subset. TOOLS may include other
# barqsync builds installed as barqsync-NAME on both ends (e.g. barqsync-0.1.1), to compare
# versions in one session.
set -uo pipefail
OUT=$1
PROFILES=${PROFILES:-lan wan}
WORKLOADS=${WORKLOADS:-inplace shift append mid sqlite}
TOOLS=${TOOLS:-frsync barqsync rsync}
RUNS=${RUNS:-3}
W=${W:-/bench}

unshape() {
  tc qdisc del dev veth0 root 2>/dev/null
  ip netns exec remote tc qdisc del dev veth1 root 2>/dev/null
}
shape() {
  unshape
  case $1 in
    lan) q="rate 1gbit burst 1mb latency 50ms"; HOST=bench ;;
    wan) q="rate 100mbit burst 256kb latency 100ms"; HOST=bench-wan ;;
    *) echo "unknown profile $1" >&2; exit 1 ;;
  esac
  tc qdisc add dev veth0 root tbf $q
  ip netns exec remote tc qdisc add dev veth1 root tbf $q
}
trap unshape EXIT
ctr() { cat /sys/class/net/veth0/statistics/$1; }

echo -e "profile\tworkload\ttool\trun\tsecs\tup_bytes\tdown_bytes\tok" > "$OUT"
for prof in $PROFILES; do
  shape "$prof"
  ssh -o BatchMode=yes "$HOST" true
  for wl in $WORKLOADS; do
    for run in $(seq 1 "$RUNS"); do
      for t in $TOOLS; do
        rm -rf "$W/dst" && cp -a "$W/data/$wl/old" "$W/dst"
        src=$W/data/$wl/new
        case $t in
          frsync) cmd=(frsync -e ssh "$src" "$HOST:$W/dst") ;;
          barqsync) cmd=(barqsync -a -e ssh "$src/" "$HOST:$W/dst/") ;;
          barqsync-*) cmd=("$t" -a -e ssh --remote-path="$(command -v "$t")" "$src/" "$HOST:$W/dst/") ;;
          rsync) cmd=(rsync -a -e ssh "$src/" "$HOST:$W/dst/") ;;
        esac
        tx0=$(ctr tx_bytes); rx0=$(ctr rx_bytes)
        s=$(date +%s.%N)
        "${cmd[@]}" >/dev/null 2>"$OUT.err"; rc=$?
        e=$(date +%s.%N)
        tx1=$(ctr tx_bytes); rx1=$(ctr rx_bytes)
        if [ $rc -eq 0 ] && diff -rq "$src" "$W/dst" >/dev/null; then ok=OK; else ok="FAIL($rc)"; cat "$OUT.err"; fi
        secs=$(awk "BEGIN { printf \"%.3f\", $e - $s }")
        echo -e "$prof\t$wl\t$t\t$run\t$secs\t$((tx1 - tx0))\t$((rx1 - rx0))\t$ok" | tee -a "$OUT"
      done
    done
  done
done
rm -f "$OUT.err"
echo DONE
