#!/usr/bin/env bash
# --streams behind a firewall that lets in ssh and nothing else, the way the macOS
# firewall treats an app it hasn't been told about: direct stream connections are reset,
# so barqsync carries the streams over extra ssh connections instead. Real OpenSSH and a
# real iptables rule, inside the container:
#
#   bench/net/container.sh firewall
set -euo pipefail
command -v iptables >/dev/null || {
  apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq iptables >/dev/null
}
cargo build --release -q
ln -sf /target/release/barqsync /usr/local/bin/barqsync
# The container's own address: ssh to it, not to loopback, so the server listens there
# (as on a real host) and the rule below has something to block.
IP=$(ip -4 -o addr show scope global | awk '{sub(/\/.*/, "", $4); print $4; exit}')
[[ -n $IP ]] || { echo "no non-loopback address" >&2; exit 1; }
ssh-keygen -A >/dev/null
/usr/sbin/sshd -o ListenAddress="$IP" -o PermitRootLogin=prohibit-password -o UseDNS=no
RSH="ssh -o StrictHostKeyChecking=accept-new"
H="root@$IP"
$RSH -o BatchMode=yes "$H" true

W=/bench/fw
rm -rf "$W" && mkdir -p "$W"
python3 - "$W/src" <<'PY'
import os, random, sys
r = random.Random(7)
for d in range(4):
    os.makedirs(f"{sys.argv[1]}/d{d}")
    for i in range(50):
        with open(f"{sys.argv[1]}/d{d}/f{i:03}", "wb") as f:
            f.write(r.randbytes(256 << 10))
PY

# New connections only (`--syn`): both ends share the address here, so replies to the
# ssh client would match too.
rule=(INPUT -d "$IP" -p tcp --syn ! --dport 22 -j REJECT --reject-with tcp-reset)
iptables -A "${rule[@]}"
trap 'iptables -D "${rule[@]}"' EXIT
echo "== firewall: only ssh reaches $IP"
iptables -S INPUT | grep REJECT

fail=0
for dir in push pull; do
  rm -rf /dst/fw
  if [[ $dir == push ]]; then from="$W/src/"; to="$H:/dst/fw/"; else from="$H:$W/src/"; to=/dst/fw/; fi
  echo "== $dir, --transport tcp --streams 4"
  start=$(date +%s.%N)
  code=0
  out=$(barqsync -a -vv --stats --transport tcp --streams 4 -e "$RSH" "$from" "$to" 2>&1) || code=$?
  took=$(python3 -c "import time; print(f'{time.time() - $start:.2f}')")
  grep -E 'parallel streams|using .* streams|Total bytes sent|rror' <<<"$out" || true
  [[ $code == 0 ]] || { echo "   exit code $code"; tail -5 <<<"$out"; fail=1; }
  if diff -r "$W/src" /dst/fw >/dev/null; then echo "   identical copy in ${took}s"; else echo "   MISMATCH"; fail=1; fi
  grep -q 'through ssh' <<<"$out" || { echo "   streams did not go through ssh"; fail=1; }
  sleep 0.5
  left=$(pgrep -fc -- '--stream-relay' || true)
  [[ $left == 0 ]] && echo "   no relay left running" || { echo "   $left relays left running"; fail=1; }
done
exit $fail
