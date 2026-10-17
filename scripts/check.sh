#!/usr/bin/env bash
# The project's check. Host workspace first (sdk, catalogue, steward, the
# host distribution): format, clippy with warnings denied, tests, docs with
# warnings denied. Then the scripts. Then every firmware crate, formatted
# and built for its device, when any exists.
#
# The firmware step needs the ESP toolchain (ldproxy on PATH; each crate's
# rust-toolchain.toml pulls its channel). With firmware crates present and
# no toolchain, the check fails and says so: a pass that never built the
# product would be a lie.

set -euo pipefail
cd "$(dirname "$0")/.."

echo "== host workspace"
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

echo "== no_std, on bare metal"
# The sdk claims no_std; proving it means building it for a target that
# has no std at all.
bare=riscv32imac-unknown-none-elf
if ! rustup target list --installed | grep -qx "$bare"; then
  echo "target $bare is not installed: rustup target add $bare" >&2
  exit 1
fi
cargo build -p evo-esp32-sdk --target "$bare" --locked

echo "== scripts"
for f in scripts/*.sh; do bash -n "$f"; done
if command -v shellcheck >/dev/null; then
  shellcheck scripts/*.sh
else
  echo "shellcheck is not on PATH; scripts parsed only"
fi

echo "== firmware"
crates=()
for crate in firmware/*/; do
  [ -f "$crate/Cargo.toml" ] && crates+=("$crate")
done
if [ "${#crates[@]}" -eq 0 ]; then
  echo "no firmware crate yet"
elif ! command -v ldproxy >/dev/null; then
  echo "ldproxy is not on PATH: the firmware cannot be built on this machine." >&2
  echo "  cargo install ldproxy; each firmware crate's rust-toolchain.toml names its channel." >&2
  exit 1
else
  for crate in "${crates[@]}"; do
    echo "-- $crate"
    (cd "$crate" && cargo fmt --check && cargo build --release)
  done
fi

echo "== ok"
