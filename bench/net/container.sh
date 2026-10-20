#!/usr/bin/env bash
# Run the Linux verification suite in a privileged Podman (or Docker) container.
#
#   bench/net/container.sh build                 build the image (once)
#   bench/net/container.sh net [profiles...]     rsync vs barqsync over real ssh, tc-shaped
#   bench/net/container.sh syscalls              strace -c syscall counts
#   bench/net/container.sh flamegraph [workload] perf + inferno flamegraph
#   bench/net/container.sh test                  cargo test + clippy on Linux
#   bench/net/container.sh shell                 interactive shell
#
# Privileged because tc (NET_ADMIN), perf and drop_caches need it; the container only
# shapes its own loopback interface. Results go to bench/results/.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
ENGINE=${ENGINE:-$(command -v podman || command -v /opt/podman/bin/podman || command -v docker)}
IMAGE=barqsync-bench
cmd=${1:-help}
shift || true

run() {
  # /bench and /dst are RAM-backed: the container's overlay filesystem makes parallel
  # small-file writes erratic, which would measure the VM's disk instead of the network.
  # TARGET_VOLUME keeps one checkout's builds apart from another's, REGISTRY_VOLUME keeps
  # downloaded crates between runs; BARQSYNC_* variables
  # (e.g. BARQSYNC_TRANSPORT=auto for `test`) and WORKLOADS/VARIANTS/GSO are passed through.
  local pass=()
  [[ -n ${REGISTRY_VOLUME:-} ]] && pass+=(-v "$REGISTRY_VOLUME":/usr/local/cargo/registry)
  for v in $(compgen -v | grep -E '^(BARQSYNC_[A-Z_]+|WORKLOADS|VARIANTS|GSO)$'); do pass+=(-e "$v"); done
  "$ENGINE" run --rm --privileged --security-opt seccomp=unconfined \
    --tmpfs /bench:rw,size=2g --tmpfs /dst:rw,size=2g \
    -v "$ROOT":/work -w /work -v "${TARGET_VOLUME:-barqsync-target}":/target -e CARGO_TARGET_DIR=/target \
    -e RUNS="${RUNS:-3}" -e SCALE="${SCALE:-1}" ${pass[@]+"${pass[@]}"} "$@"
}

case "$cmd" in
  build) "$ENGINE" build -t "$IMAGE" -f "$ROOT/bench/net/Containerfile" "$ROOT/bench/net" ;;
  net) run "$IMAGE" bash bench/net/net_bench.sh "$@" ;;
  syscalls) run "$IMAGE" bash bench/syscalls.sh "$@" ;;
  flamegraph) run "$IMAGE" bash bench/flamegraph.sh "$@" ;;
  test) run "$IMAGE" bash -c 'cargo test 2>&1 | grep -E "^test result|FAILED|panicked|^error"; cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"' ;;
  shell) run -it "$IMAGE" bash ;;
  *) sed -n '2,13p' "$0"; exit 1 ;;
esac
