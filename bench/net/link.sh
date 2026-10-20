# Shared by the container benchmarks (net_bench.sh, fixed_cost.sh): shape loopback like a
# link with tc netem. `shape PROFILE` (lan, wan, lossy, far, none; PROFILE-noudp drops
# all UDP too); the profiles are described in net_bench.sh.
# Loopback's 64 KiB MTU makes netem drop whole bursts and stall TCP on retransmission
# timeouts; use an Ethernet MTU and a queue deep enough for the bandwidth-delay product.
ip link set dev lo mtu 1500
# TCP still hands netem segments of up to 44 packets (GSO), and a loss drops all of them.
# GSO=off makes TCP send single packets, so its losses are as random as a real link's
# (quinn's UDP batches of up to 10 packets stay batched: that errs in TCP's favor).
if [[ ${GSO:-on} == off ]]; then
  ip link set dev lo gso_max_segs 1
fi
shape() {
  tc qdisc del dev lo root 2>/dev/null || true
  tc qdisc del dev lo ingress 2>/dev/null || true
  case "${1%-noudp}" in
    lan) tc qdisc add dev lo root netem delay 0.1ms rate 1gbit limit 20000 ;;
    wan) tc qdisc add dev lo root netem delay 10ms rate 100mbit limit 20000 ;;
    lossy) tc qdisc add dev lo root netem delay 40ms rate 20mbit loss 0.5% limit 20000 ;;
    far) tc qdisc add dev lo root netem delay 40ms rate 100mbit limit 20000 ;;
    none) ;;
    *) echo "unknown profile $1" >&2; exit 1 ;;
  esac
  if [[ $1 == *-noudp ]]; then
    tc qdisc add dev lo handle ffff: ingress
    tc filter add dev lo parent ffff: protocol ip u32 match ip protocol 17 0xff action drop
  fi
}
trap 'tc qdisc del dev lo root 2>/dev/null || true; tc qdisc del dev lo ingress 2>/dev/null || true' EXIT
