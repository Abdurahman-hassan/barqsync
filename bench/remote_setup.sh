#!/usr/bin/env bash
# Prepare a real second machine for remote tests and benchmarks (key-based ssh only).
#
#   bench/remote_setup.sh user@host
#
# Builds barqsync for the host's architecture, copies it to ~/bin/barqsync there, and
# reports whether a reference rsync 3.x is available on the host.
set -euo pipefail
HOST=${1:?usage: bench/remote_setup.sh user@host}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=10 "$HOST")

"${SSH[@]}" true 2>/dev/null || {
  echo "key-based ssh to $HOST failed; set it up first: ssh-copy-id $HOST" >&2
  exit 1
}
arch=$("${SSH[@]}" uname -m)
os=$("${SSH[@]}" uname -s)
case "$os/$arch" in
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  Darwin/arm64) target=aarch64-apple-darwin ;;
  Linux/x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux/aarch64) target=aarch64-unknown-linux-gnu ;;
  *) echo "unsupported host: $os/$arch" >&2; exit 1 ;;
esac
echo "host: $HOST ($os/$arch) → $target"
rustup target add "$target" >/dev/null 2>&1 || true
cargo build --release -q --manifest-path "$ROOT/Cargo.toml" --target "$target"
"${SSH[@]}" 'mkdir -p ~/bin'
scp -q "$ROOT/target/$target/release/barqsync" "$HOST:bin/barqsync"
echo "installed: $("${SSH[@]}" '~/bin/barqsync --version')"

ref=$("${SSH[@]}" 'for c in /usr/local/bin/rsync /opt/homebrew/bin/rsync /usr/bin/rsync; do v=$($c --version 2>/dev/null | head -1); case "$v" in "rsync  version 3"*) echo "$c"; exit;; esac; done' || true)
if [[ -n "$ref" ]]; then
  echo "reference rsync on host: $ref"
else
  echo "no rsync 3.x on host (Apple's openrsync isn't a fair baseline); install with: brew install rsync"
fi
cat <<EOF

Run the real-host tests and benchmarks with:
  BARQSYNC_TEST_REMOTE=$HOST BARQSYNC_TEST_REMOTE_RSYNC=${ref:-<rsync path>} cargo test --test real_remote
  REMOTE=$HOST REMOTE_BARQSYNC=bin/barqsync REMOTE_RSYNC=${ref:-<rsync path>} bench/run.sh <dir> D
EOF
