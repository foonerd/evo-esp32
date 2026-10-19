#!/usr/bin/env bash
# Assemble the flashable image for one firmware crate.
#
# Usage: scripts/image.sh firmware/<vendor>-<model>     (after cargo build --release there)
# Needs: python3; esptool 5.x as a python module, on PATH, or in ESP-IDF's python_env
#
# Offsets and flash settings come from ESP-IDF's flasher_args.json, so they
# track the crate's sdkconfig. The app image is built from the Rust ELF; the
# "libespidf.bin" that flasher_args.json lists is ESP-IDF's placeholder, not
# this firmware. Output goes to dist/ with a SHA256SUMS; a run that fails
# leaves nothing of this crate's previous image behind, so dist/ never
# holds an image older than the ELF it claims to be.

set -euo pipefail

usage() { echo "usage: $0 firmware/<vendor>-<model>" >&2; exit 2; }
[ $# -eq 1 ] || usage

root="$(cd "$(dirname "$0")/.." && pwd)"
crate="$root/${1%/}"
[ -f "$crate/Cargo.toml" ] || { echo "no Cargo.toml in $1" >&2; exit 1; }

read -r name version target < <(python3 -I - "$crate" <<'PY'
import sys, tomllib
crate = sys.argv[1]
with open(f"{crate}/Cargo.toml", "rb") as f:
    pkg = tomllib.load(f)["package"]
with open(f"{crate}/.cargo/config.toml", "rb") as f:
    target = tomllib.load(f)["build"]["target"]
print(pkg["name"], pkg["version"], target)
PY
)

elf="$crate/target/$target/release/$name"
[ -f "$elf" ] || { echo "missing $elf: run cargo build --release in $1 first" >&2; exit 1; }

# esp-idf-sys leaves one build per configuration under target/; an
# sdkconfig change makes a second. The newest is the one the ELF was
# linked against.
mapfile -t found < <(find "$crate/target/$target/release/build" -path '*esp-idf-sys*' -name flasher_args.json -printf '%T@ %p\n' | sort -rn | cut -d' ' -f2-)
[ "${#found[@]}" -ge 1 ] || { echo "no flasher_args.json under $1/target: build first" >&2; exit 1; }
[ "${#found[@]}" -eq 1 ] || echo "note: ${#found[@]} esp-idf-sys builds under $1/target; using the newest"
args="${found[0]}"
build="$(dirname "$args")"

read -r chip mode freq size boot_off boot_file pt_off pt_file app_off < <(python3 -I - "$args" <<'PY'
import json, sys
a = json.load(open(sys.argv[1]))
fs = a["flash_settings"]
print(a["extra_esptool_args"]["chip"], fs["flash_mode"], fs["flash_freq"], fs["flash_size"],
      a["bootloader"]["offset"], a["bootloader"]["file"],
      a["partition-table"]["offset"], a["partition-table"]["file"],
      a["app"]["offset"])
PY
)

out="$root/dist"
stem="$name-$version"
mkdir -p "$out"
rm -f "$out/$stem"*.bin "$out/$stem.SHA256SUMS"
cp "$build/$boot_file" "$out/$stem-bootloader.bin"

# Tools live in the crate's .embuild unless ESP_IDF_TOOLS_INSTALL_DIR
# points a build at a shared directory (custom:<path>).
tools="${ESP_IDF_TOOLS_INSTALL_DIR:-}"
case "$tools" in
  custom:*) tools="${tools#custom:}" ;;
  *) tools="$crate/.embuild" ;;
esac

# esptool: the python module if python3 has it, else the executable on PATH
# (a pipx install), else the one in ESP-IDF's own python environment.
if python3 -c 'import esptool' 2>/dev/null; then
  esptool=(python3 -m esptool)
elif command -v esptool >/dev/null; then
  esptool=(esptool)
else
  idf_py=$(find "$tools" -path '*python_env*/bin/python' | head -1)
  if [ -z "$idf_py" ] || ! "$idf_py" -c 'import esptool' 2>/dev/null; then
    echo "esptool not found: pip/pipx install esptool, or point ESP_IDF_TOOLS_INSTALL_DIR at the tools" >&2
    exit 1
  fi
  esptool=("$idf_py" -m esptool)
fi

# esp-idf-sys does not feed a crate's partitions.csv to the ESP-IDF build, so
# the table in flasher_args.json is ESP-IDF's default. A crate that ships its
# own partitions.csv gets it generated here with ESP-IDF's own tool.
if [ -f "$crate/partitions.csv" ]; then
  gen=$(find "$tools" -name gen_esp32part.py | head -1)
  [ -n "$gen" ] || { echo "gen_esp32part.py not found under $tools" >&2; exit 1; }
  python3 "$gen" -q "$crate/partitions.csv" "$out/$stem-partition-table.bin"
else
  cp "$build/$pt_file" "$out/$stem-partition-table.bin"
fi

"${esptool[@]}" --chip "$chip" elf2image \
    --flash-mode "$mode" --flash-freq "$freq" --flash-size "$size" \
    -o "$out/$stem-app.bin" "$elf"

# The bootloader refuses an app longer than its partition; say so here, not
# on the bench.
read -r app_size app_cap < <(python3 -I - "$out/$stem-app.bin" "$out/$stem-partition-table.bin" "$app_off" <<'PY'
import struct, sys, os
app = os.path.getsize(sys.argv[1])
table = open(sys.argv[2], "rb").read()
app_off = int(sys.argv[3], 0)
cap = 0
for i in range(0, len(table), 32):
    e = table[i:i+32]
    if e[:2] != b"\xaa\x50":
        break
    typ, sub, off, size = e[2], e[3], struct.unpack("<I", e[4:8])[0], struct.unpack("<I", e[8:12])[0]
    if typ == 0 and off == app_off:
        cap = size
print(app, cap)
PY
)
if [ "$app_size" -gt "$app_cap" ]; then
  echo "app is $app_size bytes, the partition at $app_off holds $app_cap: it will not boot" >&2
  exit 1
fi
echo "app $app_size bytes in a $app_cap byte partition"

"${esptool[@]}" --chip "$chip" merge-bin -o "$out/$stem.bin" \
    --flash-mode "$mode" --flash-freq "$freq" --flash-size "$size" \
    "$boot_off" "$out/$stem-bootloader.bin" \
    "$pt_off" "$out/$stem-partition-table.bin" \
    "$app_off" "$out/$stem-app.bin"

(cd "$out" && sha256sum "$stem"*.bin | tee "$stem.SHA256SUMS")
echo "flash: esptool --chip $chip --port <PORT> write-flash 0x0 dist/$stem.bin"
