#!/usr/bin/env bash
# Network benchmarks (original plan §8): rsync vs fastrsync over a real sshd on loopback,
# with the link shaped by tc netem. Runs inside the container (bench/net/container.sh net).
#
# Profiles (netem applies on each pass through `lo`, so a round trip is delayed twice
# and both directions share the rate):
#   lan    1 Gbit,  0.1 ms each way  (~0.2 ms RTT)
#   wan    100 Mbit, 10 ms each way  (~20 ms RTT)
#   lossy  20 Mbit,  40 ms each way, 0.5% loss each way (~80 ms RTT, ~1% per round trip)
#
# Source and destination live on tmpfs (see container.sh), so the link is what's measured.
#
# Workloads (sizes scale with SCALE, default 1; WORKLOADS="small delta" picks some):
#   small  fresh push of 8,000 files of 4 KiB in 40 directories
#   delta  push of a 256 MiB image with 8 x 1 MiB in-place changes (old copy on the remote)
#   noop   push of 40,000 files where 3 changed
#   text   fresh push of 4,000 compressible text files of 8-32 KiB
#   mid    push of 40 files of 4 MiB, each with 1 KiB changed (old copies on the remote)
#
# VARIANTS picks commands by name (default: all), e.g. VARIANTS="rsync ssh quic".
set -euo pipefail
PROFILES=${*:-lan wan lossy}
WORKLOADS=${WORKLOADS:-small delta noop}
VARIANTS=${VARIANTS:-rsync ssh ssh-tokio tcp tcp-tokio quic quic-rkyv}
RUNS=${RUNS:-3}
SCALE=${SCALE:-1}
FR=/target/release/fastrsync
W=/bench
OUT=/work/bench/results
mkdir -p "$OUT" "$W"
tag=$(date +%Y%m%d-%H%M%S)
REPORT="$OUT/net-$tag.md"

echo "== building fastrsync (release)"
cargo build --release -q
ln -sf "$FR" /usr/local/bin/fastrsync

echo "== starting sshd on 127.0.0.1"
ssh-keygen -A >/dev/null
/usr/sbin/sshd -o ListenAddress=127.0.0.1 -o PermitRootLogin=prohibit-password -o UseDNS=no
ssh -o BatchMode=yes bench true

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
tree(f"{w}/small", max(1, int(40 * scale)), 200, 4096, 1)
def text_tree(root, dirs, files, seed):
    if os.path.isdir(root):
        return
    r = random.Random(seed)
    words = [bytes(r.choice(b"abcdefghijklmnopqrstuvwxyz") for _ in range(r.randint(2, 9))) for _ in range(500)]
    for d in range(dirs):
        os.makedirs(f"{root}/d{d:04d}")
        for i in range(files):
            n = r.randint(8 << 10, 32 << 10)
            out = bytearray()
            while len(out) < n:
                out += b" ".join(r.choice(words) for _ in range(12)) + b" %d\n" % r.randint(0, 10**6)
            with open(f"{root}/d{d:04d}/f{i:05d}.txt", "wb") as f:
                f.write(out[:n])
text_tree(f"{w}/text", max(1, int(40 * scale)), 100, 4)
mid = f"{w}/mid_new"
if not os.path.isdir(mid):
    r = random.Random(5)
    os.makedirs(mid)
    os.makedirs(f"{w}/mid_old")
    for i in range(max(1, int(40 * scale))):
        data = bytearray(r.randbytes(4 << 20))
        open(f"{w}/mid_old/m{i:03d}.bin", "wb").write(data)
        off = r.randrange(0, (4 << 20) - 1024)
        data[off:off + 1024] = r.randbytes(1024)
        open(f"{mid}/m{i:03d}.bin", "wb").write(data)
tree(f"{w}/noop", max(1, int(400 * scale)), 100, 1024, 2)
big = f"{w}/big_old.img"
if not os.path.exists(big):
    n = int((256 << 20) * scale) >> 20 << 20
    r = random.Random(3)
    data = bytearray(b"".join(r.randbytes(1 << 20) for _ in range(n >> 20)))
    open(big, "wb").write(data)
    for _ in range(8):
        off = r.randrange(0, n - (1 << 20))
        data[off:off + (1 << 20)] = r.randbytes(1 << 20)
    open(f"{w}/big_new.img", "wb").write(data)
EOF
touch -d '2001-01-01' "$W/big_old.img"

RSYNC_R="rsync -a -e ssh"
FAST_ADAPT="$FR -a -e ssh --strategy adaptive"
FAST_FIXED="$FR -a -e ssh --strategy fixed"
FAST_QUIC_ADAPT="$FR -a -e ssh --transport quic --strategy adaptive"
FAST_SSH="$FR -a -e ssh"
FAST_TCP="$FR -a -e ssh --transport tcp --streams 4"
FAST_SSH_TOKIO="$FR -a -e ssh --net-io=tokio"
FAST_TCP_TOKIO="$FR -a -e ssh --net-io=tokio --transport tcp --streams 4"
FAST_QUIC="$FR -a -e ssh --transport quic"
FAST_QUIC_RKYV="$FR -a -e ssh --transport quic --wire rkyv"

# Loopback's 64 KiB MTU makes netem drop whole bursts and stall TCP on retransmission
# timeouts; use an Ethernet MTU and a queue deep enough for the bandwidth-delay product.
ip link set dev lo mtu 1500
shape() {
  tc qdisc del dev lo root 2>/dev/null || true
  case "$1" in
    lan) tc qdisc add dev lo root netem delay 0.1ms rate 1gbit limit 20000 ;;
    wan) tc qdisc add dev lo root netem delay 10ms rate 100mbit limit 20000 ;;
    lossy) tc qdisc add dev lo root netem delay 40ms rate 20mbit loss 0.5% limit 20000 ;;
    none) ;;
    *) echo "unknown profile $1" >&2; exit 1 ;;
  esac
}
trap 'tc qdisc del dev lo root 2>/dev/null || true' EXIT

{
  echo "# Network benchmarks ($tag)"
  echo
  echo "rsync $(rsync --version | head -1 | awk '{print $3}') vs fastrsync, real sshd on loopback shaped with tc netem; $RUNS runs each."
  echo
} > "$REPORT"

for p in $PROFILES; do
  echo "== profile: $p"
  shape "$p"
  echo "## $p" >> "$REPORT"
  for wl in $WORKLOADS; do
    case $wl in
      small)
        prep="rm -rf /dst/small"
        args="$W/small/ bench:/dst/small/" ;;
      delta)
        prep="cp $W/big_old.img /dst/big.img && touch -d 2001-01-01 /dst/big.img"
        args="$W/big_new.img bench:/dst/big.img" ;;
      noop)
        $FAST_SSH "$W/noop/" bench:/dst/noop/ >/dev/null
        prep="printf x >> $W/noop/d0000/f00000; printf x >> $W/noop/d0001/f00050; printf x >> $W/noop/d0002/f00099"
        args="$W/noop/ bench:/dst/noop/" ;;
      text)
        prep="rm -rf /dst/text"
        args="$W/text/ bench:/dst/text/" ;;
      mid)
        prep="rm -rf /dst/mid && cp -r $W/mid_old /dst/mid && touch -d 2001-01-01 /dst/mid/*"
        args="$W/mid_new/ bench:/dst/mid/" ;;
    esac
    cmds=()
    for v in $VARIANTS; do
      case $v in
        rsync) cmds+=(-n "rsync" "$RSYNC_R $args") ;;
        ssh) cmds+=(-n "fastrsync (ssh pipe)" "$FAST_SSH $args") ;;
        ssh-tokio) cmds+=(-n "fastrsync ssh, tokio I/O" "$FAST_SSH_TOKIO $args") ;;
        tcp) cmds+=(-n "fastrsync --streams 4" "$FAST_TCP $args") ;;
        tcp-tokio) cmds+=(-n "fastrsync --streams 4, tokio I/O" "$FAST_TCP_TOKIO $args") ;;
        quic) cmds+=(-n "fastrsync --transport quic" "$FAST_QUIC $args") ;;
        quic-rkyv) cmds+=(-n "fastrsync quic + rkyv" "$FAST_QUIC_RKYV $args") ;;
        fixed) cmds+=(-n "fastrsync --strategy fixed" "$FAST_FIXED $args") ;;
        adaptive) cmds+=(-n "fastrsync --strategy adaptive" "$FAST_ADAPT $args") ;;
        quic-adaptive) cmds+=(-n "fastrsync quic, adaptive" "$FAST_QUIC_ADAPT $args") ;;
        *) echo "unknown variant $v" >&2; exit 1 ;;
      esac
    done
    hyperfine --runs "$RUNS" --warmup 0 --prepare "$prep" --style basic "${cmds[@]}" \
      --export-markdown "/tmp/$p-$wl.md" 2>&1 | grep -E "Benchmark|Time|±" || true
    { echo; echo "### $wl"; echo; cat "/tmp/$p-$wl.md"; } >> "$REPORT"
  done
  echo >> "$REPORT"
done
echo "report: bench/results/$(basename "$REPORT")"
