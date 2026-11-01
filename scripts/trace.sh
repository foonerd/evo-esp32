#!/usr/bin/env bash
# The leak hunt's build of one firmware crate: the heap trace across a
# track change, with call stacks. Frame pointers (sdkconfig.trace, and
# the same for the Rust side) let the kernel walk the stack on RISC-V;
# without them a trace records sizes and nothing else.
#
# Usage: scripts/trace.sh firmware/<vendor>-<model>
# Output: the crate's target-trace/ (its own, so the release build under
#         target/ is untouched) and the image in dist/ as usual.
# Then:   scripts/push.sh <player> dist/<image>.ota.bin, and read the
#         console: "trace: window closed" is followed by every internal
#         allocation the track change left alive, each with its callers;
#         scripts/symbolise.sh <elf> < dump names them.
#
# Never a release: the frame pointer and the trace cost code size and
# speed, and the trace's records take 96 KB of PSRAM.

set -euo pipefail

usage() { echo "usage: $0 firmware/<vendor>-<model>" >&2; exit 2; }
[ $# -eq 1 ] || usage

root="$(cd "$(dirname "$0")/.." && pwd)"
crate="$root/${1%/}"
[ -f "$crate/Cargo.toml" ] || { echo "no Cargo.toml in $1" >&2; exit 1; }
[ -f "$crate/sdkconfig.trace" ] || { echo "no sdkconfig.trace in $1" >&2; exit 1; }

# RUSTFLAGS replaces the crate's configured flags wholesale; carry them.
configured=$(python3 -I - "$crate" <<'PY'
import sys, tomllib
with open(f"{sys.argv[1]}/.cargo/config.toml", "rb") as f:
    c = tomllib.load(f)
target = c["build"]["target"]
print(" ".join(c.get("target", {}).get(target, {}).get("rustflags", [])))
PY
)

(
  cd "$crate"
  CARGO_TARGET_DIR=target-trace \
  ESP_IDF_SDKCONFIG_DEFAULTS="sdkconfig.defaults;sdkconfig.trace" \
  RUSTFLAGS="$configured -C force-frame-pointers=yes" \
    cargo build --release --features heap-trace
)
EVO_TARGET_DIR=target-trace "$root/scripts/image.sh" "$1"
