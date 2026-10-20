#!/usr/bin/env bash
# The fixed cost of each transport: one 1 KiB file pushed and pulled over a shaped link,
# with BARQSYNC_TIMING milestones from both sides (one clock: both run in this container),
# broken down into connecting, transport setup, the transfer and closing. Runs inside the
# container: bench/net/container.sh fixed [profiles...] (default: far).
#
# RUNS (default 10) per variant and direction; VARIANTS (default "ssh quic auto"; also
# tcp). auto starts every run without its per-host memory (BARQSYNC_CACHE_DIR is emptied).
set -euo pipefail
PROFILES=${*:-far}
VARIANTS=${VARIANTS:-ssh quic auto}
RUNS=${RUNS:-10}
FR=/target/release/barqsync
OUT=/work/bench/results
LOG=/tmp/fixed-cost
tag=$(date +%Y%m%d-%H%M%S)
REPORT="$OUT/fixed-cost-$tag.md"
mkdir -p "$OUT" "$LOG" /bench /dst

echo "== building barqsync (release)"
cargo build --release -q
ln -sf "$FR" /usr/local/bin/barqsync
echo "== starting sshd on 127.0.0.1"
ssh-keygen -A >/dev/null
/usr/sbin/sshd -o ListenAddress=127.0.0.1 -o PermitRootLogin=prohibit-password -o UseDNS=no
ssh -o BatchMode=yes bench true
source "$(dirname "$0")/link.sh"

head -c 1024 /dev/urandom > /bench/one
for p in $PROFILES; do
  echo "== profile: $p"
  shape "$p"
  for dir in push pull; do
    if [[ $dir == push ]]; then args="/bench/one bench:/dst/one"; else args="bench:/bench/one /dst/one"; fi
    for v in $VARIANTS; do
      case $v in
        ssh) t=() ;;
        tcp) t=(--transport tcp) ;;
        quic) t=(--transport quic) ;;
        auto) t=(--transport auto) ;;
        *) echo "unknown variant $v" >&2; exit 1 ;;
      esac
      for i in $(seq "$RUNS"); do
        rm -rf /dst/one /tmp/fixed-cost-cache
        f="$LOG/$p-$dir-$v-$i.log"
        s=$(date +%s.%N)
        env BARQSYNC_TIMING=1 BARQSYNC_CACHE_DIR=/tmp/fixed-cost-cache "$FR" -a -e ssh ${t[@]+"${t[@]}"} $args 2> "$f"
        e=$(date +%s.%N)
        echo "wall $s $e" >> "$f"
        cmp -s /bench/one /dst/one || { echo "transfer failed: $f" >&2; cat "$f" >&2; exit 1; }
      done
    done
  done
done

python3 - "$LOG" "$REPORT" "$RUNS" "$PROFILES" "$VARIANTS" <<'EOF'
import statistics, sys
log, report, runs, profiles, variants = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4].split(), sys.argv[5].split()

def parse(path):
    marks, wall = {}, None
    for line in open(path):
        w = line.split()
        if w[:2] == ["barqsync", "timing:"]:
            side, t, label = w[2], float(w[3]), " ".join(w[6:])
            marks.setdefault((side, label), t)
        elif w and w[0] == "wall":
            wall = (float(w[1]), float(w[2]))
    return marks, wall

def ms(a, b):
    return None if a is None or b is None else (b - a) * 1e3

def phases(marks, wall):
    c = lambda l: marks.get(("client", l))
    s = lambda l: marks.get(("server", l))
    end = c("remote shell exited")
    return {
        "total": ms(*wall),
        "connect": ms(wall[0], c("handshake done")),
        "setup": ms(c("handshake done"), c("setup done")),
        "transfer": ms(c("setup done"), c("transfer done")),
        "close": ms(c("transfer done"), end),
        "client finish": ms(c("transfer done"), c("connection closed")),
        "server finish": ms(s("transfer done"), s("connection closed")),
        "server exit seen": ms(s("connection closed"), end),
    }

cols = ["total", "connect", "setup", "transfer", "close", "client finish", "server finish", "server exit seen"]
with open(report, "w") as out:
    out.write(f"# Fixed cost per transfer ({report.split('fixed-cost-')[1][:-3]})\n\n")
    out.write(f"One 1 KiB file, {runs} runs each; medians in ms (total: mean ± sd). Phases, from the client's\n")
    out.write("milestones: connect = ssh up to the protocol handshake, setup = the transport's setup,\n")
    out.write("transfer = up to the last protocol message, close = from there to the remote shell's exit.\n")
    out.write("client/server finish = closing the connection on each side; server exit seen = from the\n")
    out.write("server's finish to the client seeing the remote shell exit.\n\n")
    for p in profiles:
        out.write(f"## {p}\n\n| direction, transport | " + " | ".join(cols) + " |\n|:---|" + "---:|" * len(cols) + "\n")
        for d in ["push", "pull"]:
            for v in variants:
                rows = [phases(*parse(f"{log}/{p}-{d}-{v}-{i}.log")) for i in range(1, runs + 1)]
                cells = []
                for k in cols:
                    vals = [r[k] for r in rows if r[k] is not None]
                    if not vals:
                        cells.append("–")
                    elif k == "total":
                        sd = statistics.stdev(vals) if len(vals) > 1 else 0
                        cells.append(f"{statistics.mean(vals):.0f} ± {sd:.0f}")
                    else:
                        cells.append(f"{statistics.median(vals):.0f}")
                out.write(f"| {d}, {v} | " + " | ".join(cells) + " |\n")
        out.write("\n")
print(open(report).read())
print(f"report: bench/results/{report.split('/')[-1]}")
EOF
