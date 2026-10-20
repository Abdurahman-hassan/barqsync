#!/usr/bin/env bash
# rsync vs barqsync between this machine and a real second host over real ssh
# (prepare the host with bench/remote_setup.sh first).
#
#   bench/real_host.sh user@host [scale]
#
# Sized for slow links: the data is generated on the host itself (no upload), and most
# workloads pull from it. With scale 1:
#   pull-small  400 random files of 4 KiB, fresh pull
#   pull-text   400 compressible text files of 4-16 KiB, fresh pull
#   pull-delta  an 8 MiB file with 4 x 64 KiB changed, old copy here
#   pull-noop   2,000 files of 1 KiB, 3 changed on the host
#   push-small  100 random files of 4 KiB, fresh push
# Env: RUNS=3  REMOTE_BARQSYNC=bin/barqsync  REMOTE_RSYNC=/usr/local/bin/rsync
#      VARIANTS="rsync ssh tcp quic adaptive quic-adaptive"
set -euo pipefail
HOST=${1:?usage: bench/real_host.sh user@host [scale]}
SCALE=${2:-1}
RUNS=${RUNS:-3}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
FR=${BARQSYNC:-$ROOT/target/release/barqsync}
RB=${REMOTE_BARQSYNC:-bin/barqsync}
RR=${REMOTE_RSYNC:-/usr/local/bin/rsync}
VARIANTS=${VARIANTS:-rsync ssh tcp quic adaptive quic-adaptive}
RD=/tmp/barqsync-realbench
LD=${LOCAL_DIR:-/tmp/barqsync-realbench-local}
OUT=$ROOT/bench/results
tag=$(date +%Y%m%d-%H%M%S)
REPORT=$OUT/real-host-$tag.md
SSH=(ssh -o BatchMode=yes "$HOST")
mkdir -p "$OUT" "$LD"
n() { python3 -c "print(max(1, int($1 * $SCALE)))"; }

echo "== generating data on $HOST:$RD"
"${SSH[@]}" "RD=$RD SMALL=$(n 100) TEXT=$(n 100) NOOP=$(n 20) PUSH=$(n 25) bash -s" <<'EOF'
set -e
rm -rf "$RD"; mkdir -p "$RD"; cd "$RD"
for d in 0 1 2 3; do mkdir -p small/d$d pushsrc/d$d text/d$d
  for i in $(seq 1 "$SMALL"); do head -c 4096 /dev/urandom > small/d$d/f$i; done
  for i in $(seq 1 "$PUSH"); do head -c 4096 /dev/urandom > pushsrc/d$d/f$i; done
  for i in $(seq 1 "$TEXT"); do
    awk -v s=$RANDOM -v n=$((4096 + RANDOM % 12288)) 'BEGIN { srand(s); split("alpha beta gamma delta sync file block chunk stream packet quick brown fox lazy dog report value index", w, " ");
      out = ""; while (length(out) < n) out = out w[int(rand() * 19) + 1] " " int(rand() * 100000) (rand() < 0.1 ? "\n" : " "); printf "%s", substr(out, 1, n) }' > text/d$d/f$i.txt
  done
done
for d in $(seq 1 "$NOOP"); do mkdir -p noop/d$d; for i in $(seq 1 100); do head -c 1024 /dev/urandom > noop/d$d/f$i; done; done
head -c 8388608 /dev/urandom > big_old.img
cp big_old.img big_new.img
for off in 100 700 1500 2000; do dd if=/dev/urandom of=big_new.img bs=4096 seek=$off count=16 conv=notrunc 2>/dev/null; done
touch -t 200101010000 big_old.img
EOF

echo "== seeding local copies (old big file, the no-op tree, the push source)"
"$FR" -a --remote-path="$RB" "$HOST:$RD/big_old.img" "$LD/big_old.img"
"$FR" -a --remote-path="$RB" "$HOST:$RD/noop/" "$LD/noop/"
rm -rf "$LD/pushsrc"; "$FR" -a --remote-path="$RB" "$HOST:$RD/pushsrc/" "$LD/pushsrc/"
rtt=$(ping -c 5 -q "${HOST#*@}" 2>/dev/null | awk -F/ '/round-trip|rtt/ {print $5}')
{
  echo "# Real host ($tag)"
  echo
  echo "Local: $(uname -sm), remote: $("${SSH[@]}" 'uname -sm; sysctl -n machdep.cpu.brand_string 2>/dev/null' | tr '\n' ' ')over ssh (host left out of the report); ping ${rtt:-?} ms average; $RUNS runs; scale $SCALE."
  echo
} > "$REPORT"

B="'$FR' -a --remote-path=$RB"
cmds_for() { # workload src dst
  local args="$2 $3" c=()
  for v in $VARIANTS; do
    case $v in
      rsync) c+=(-n "rsync" "rsync -a -e ssh --rsync-path=$RR $args") ;;
      ssh) c+=(-n "barqsync (ssh pipe)" "$B $args") ;;
      tcp) c+=(-n "barqsync --transport tcp" "$B --transport tcp --streams 4 $args") ;;
      quic) c+=(-n "barqsync --transport quic" "$B --transport quic $args") ;;
      adaptive) c+=(-n "barqsync --strategy adaptive" "$B --strategy adaptive $args") ;;
      quic-adaptive) c+=(-n "barqsync quic, adaptive" "$B --transport quic --strategy adaptive $args") ;;
    esac
  done
  printf '%s\0' "${c[@]}"
}
run() { # title prepare src dst
  local title=$1 prep=$2
  local -a cmds=()
  while IFS= read -r -d '' x; do cmds+=("$x"); done < <(cmds_for "$title" "$3" "$4")
  echo "== $title"
  hyperfine --runs "$RUNS" --warmup 0 --prepare "$prep" --style basic "${cmds[@]}" --export-markdown /tmp/real-$title.md 2>&1 | grep -E "Benchmark|Time" || true
  { echo "## $title"; echo; cat /tmp/real-$title.md; echo; } >> "$REPORT"
}
run pull-small "rm -rf '$LD/small'" "$HOST:$RD/small/" "'$LD/small/'"
run pull-text "rm -rf '$LD/text'" "$HOST:$RD/text/" "'$LD/text/'"
run pull-delta "cp '$LD/big_old.img' '$LD/work.img' && touch -t 200101010000 '$LD/work.img'" "$HOST:$RD/big_new.img" "'$LD/work.img'"
run pull-noop "${SSH[*]} 'printf x >> $RD/noop/d1/f1; printf x >> $RD/noop/d2/f50; printf x >> $RD/noop/d3/f99'" "$HOST:$RD/noop/" "'$LD/noop/'"
run push-small "${SSH[*]} 'rm -rf $RD/pushdst'" "'$LD/pushsrc/'" "$HOST:$RD/pushdst/"
cat "$REPORT"
echo "report: $REPORT"
