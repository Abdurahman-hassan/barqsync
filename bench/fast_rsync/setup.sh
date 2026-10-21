#!/usr/bin/env bash
# One-time setup (as root, once per boot) for the fast_rsync comparison in
# docs/fast-rsync-comparison.md: builds barqsync and the harness, puts the datasets on a tmpfs
# at $W, and starts an sshd in its own network namespace ("remote") reached over a veth
# pair, plus a relay that adds 10 ms each way for the wan profile.
#
#   apt-get install -y openssh-server openssh-client iproute2 rsync python3
#   bench/fast_rsync/setup.sh
#   bench/fast_rsync/engine.sh > engine.txt
#   bench/fast_rsync/netbench.sh net.tsv
#   python3 bench/fast_rsync/summarize.py net.tsv engine.txt
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
W=${W:-/bench}

echo "== building barqsync and the harness (release)"
(cd "$REPO" && cargo build --release -q)
(cd "$HERE" && cargo build --release -q)
ln -sf "$REPO/target/release/barqsync" /usr/local/bin/barqsync
ln -sf "$HERE/target/release/frsync" /usr/local/bin/frsync

echo "== datasets on a tmpfs at $W"
mkdir -p "$W"
mountpoint -q "$W" || mount -t tmpfs -o size=10g tmpfs "$W"
if [ ! -d "$W/data" ]; then
  mkdir -p "$W/data"
  python3 -I "$HERE/gen.py" "$W/data"
  find "$W/data" -path '*/old/*' -exec touch -d 2001-01-01 {} +
fi

# In a VM, the first touch of fresh guest memory can be slow (it was in the Firecracker VM
# the comparison ran in), which mostly penalizes frsync's whole-file buffers. Touch all
# but 3 GiB of the available memory once so every run starts from backed pages.
echo "== pre-touching memory"
python3 -I - <<'PY'
avail = int(next(l.split()[1] for l in open("/proc/meminfo") if l.startswith("MemAvailable"))) << 10
bufs = []
for _ in range(max(0, avail - (3 << 30)) >> 30):
    b = bytearray(1 << 30)
    for i in range(0, len(b), 4096):
        b[i] = 1
    bufs.append(b)
print(f"   touched {len(bufs)} GiB")
PY

echo "== network namespace, veth pair, sshd, delay relay"
pkill -f '^/usr/sbin/sshd -o ListenAddress=10.77' || true
pkill -f '^python3 -I .*relay.py' || true
ip netns del remote 2>/dev/null || true
ip link del veth0 2>/dev/null || true
ip netns add remote
ip link add veth0 type veth peer name veth1
ip link set veth1 netns remote
ip addr add 10.77.0.1/24 dev veth0
ip link set veth0 up
ip netns exec remote ip addr add 10.77.0.2/24 dev veth1
ip netns exec remote ip link set veth1 up
ip netns exec remote ip link set lo up

mkdir -p /run/sshd /root/.ssh
chmod 700 /root/.ssh
ssh-keygen -A >/dev/null
[ -f /root/.ssh/id_ed25519 ] || ssh-keygen -q -t ed25519 -N '' -f /root/.ssh/id_ed25519
grep -qxFf /root/.ssh/id_ed25519.pub /root/.ssh/authorized_keys 2>/dev/null || cat /root/.ssh/id_ed25519.pub >> /root/.ssh/authorized_keys
chmod 600 /root/.ssh/authorized_keys
grep -q '^Host bench$' /root/.ssh/config 2>/dev/null || printf '%s\n' \
  'Host bench' '  HostName 10.77.0.2' '  StrictHostKeyChecking accept-new' \
  'Host bench-wan' '  HostName 10.77.0.2' '  Port 2222' '  HostKeyAlias 10.77.0.2' '  StrictHostKeyChecking accept-new' \
  >> /root/.ssh/config
# Start sshd only after the tmpfs is mounted: `ip netns exec` gives it a copy of the
# mount table, so a later mount would not be visible to the "remote" side.
ip netns exec remote /usr/sbin/sshd -o ListenAddress=10.77.0.2 -o ListenAddress=127.0.0.1 \
  -o PermitRootLogin=prohibit-password -o UseDNS=no
ip netns exec remote nohup python3 -I "$HERE/relay.py" 10.77.0.2 2222 127.0.0.1 22 0.010 >/dev/null 2>&1 &
sleep 1
ssh -o BatchMode=yes bench true
ssh -o BatchMode=yes bench-wan true
[ "$(ssh -o BatchMode=yes bench "ls $W/data | wc -l")" -gt 0 ] || { echo "remote side does not see $W/data" >&2; exit 1; }
echo "== ready"
