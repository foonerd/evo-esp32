#!/usr/bin/env bash
# Assemble the flashable image for one firmware crate.
#
# Usage: scripts/image.sh firmware/<vendor>-<model>     (after cargo build --release there)
# Needs: python3; esptool 5.x (with espsecure) as a python module, on PATH,
#        or in ESP-IDF's python_env
# Env:   EVO_SIGNING_KEY=<pem>  the ECDSA P-256 key the app is signed with,
#        required when the crate's sdkconfig.defaults requires signed apps
#        (make one: espsecure generate-signing-key --version 2 --scheme ecdsa256 key.pem)
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
# espsecure ships with esptool and is found the same way.
if python3 -c 'import esptool' 2>/dev/null; then
  esptool=(python3 -m esptool)
  espsecure=(python3 -m espsecure)
elif command -v esptool >/dev/null; then
  esptool=(esptool)
  espsecure=(espsecure)
else
  idf_py=$(find "$tools" -path '*python_env*/bin/python' | head -1)
  if [ -z "$idf_py" ] || ! "$idf_py" -c 'import esptool' 2>/dev/null; then
    echo "esptool not found: pip/pipx install esptool, or point ESP_IDF_TOOLS_INSTALL_DIR at the tools" >&2
    exit 1
  fi
  esptool=("$idf_py" -m esptool)
  espsecure=("$idf_py" -m espsecure)
fi

# esp-idf-sys does not feed a crate's partitions.csv to the ESP-IDF build, so
# the table in flasher_args.json is ESP-IDF's default. A crate that ships its
# own partitions.csv gets it generated here with ESP-IDF's own tool, and the
# app goes where that table puts its factory slot (the first app slot when
# there is no factory), not where ESP-IDF's default would.
if [ -f "$crate/partitions.csv" ]; then
  gen=$(find "$tools" -name gen_esp32part.py | head -1)
  [ -n "$gen" ] || { echo "gen_esp32part.py not found under $tools" >&2; exit 1; }
  python3 "$gen" -q "$crate/partitions.csv" "$out/$stem-partition-table.bin"
  app_off=$(python3 -I - "$out/$stem-partition-table.bin" <<'PY'
import struct, sys
table = open(sys.argv[1], "rb").read()
first = None
for i in range(0, len(table), 32):
    e = table[i:i+32]
    if e[:2] != b"\xaa\x50":
        break
    typ, sub, off = e[2], e[3], struct.unpack("<I", e[4:8])[0]
    if typ == 0 and sub == 0:
        print(hex(off)); sys.exit(0)
    if typ == 0 and first is None:
        first = off
print(hex(first) if first is not None else "")
PY
)
  [ -n "$app_off" ] || { echo "partitions.csv has no app partition" >&2; exit 1; }
else
  cp "$build/$pt_file" "$out/$stem-partition-table.bin"
fi

"${esptool[@]}" --chip "$chip" elf2image \
    --flash-mode "$mode" --flash-freq "$freq" --flash-size "$size" \
    -o "$out/$stem.ota.bin" "$elf"

# A firmware that requires signed apps (sdkconfig) takes only an image
# with a Secure Boot v2 signature block, checked against the key of the
# firmware already running: the block is appended here, with the key
# EVO_SIGNING_KEY names. The same key must sign every image a player is
# to accept after this one; a different key means a USB flash.
if grep -q '^CONFIG_SECURE_SIGNED_APPS_NO_SECURE_BOOT=y' "$crate/sdkconfig.defaults"; then
  key="${EVO_SIGNING_KEY:-}"
  if [ -z "$key" ] || [ ! -f "$key" ]; then
    echo "this firmware requires signed images: set EVO_SIGNING_KEY to an ECDSA P-256 PEM" >&2
    echo "  (espsecure generate-signing-key --version 2 --scheme ecdsa256 key.pem; keep it out of the tree)" >&2
    exit 1
  fi
  mv "$out/$stem.ota.bin" "$out/$stem.unsigned.bin"
  "${espsecure[@]}" sign-data --version 2 --keyfile "$key" \
      --output "$out/$stem.ota.bin" "$out/$stem.unsigned.bin" >/dev/null
  rm -f "$out/$stem.unsigned.bin"
  echo "app signed with $(basename "$key")"
fi

# The bootloader refuses an app longer than its partition; say so here, not
# on the bench.
read -r app_size app_cap < <(python3 -I - "$out/$stem.ota.bin" "$out/$stem-partition-table.bin" "$app_off" <<'PY'
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

# Two images: `.factory.bin` is the whole flash from 0x0 (bootloader,
# table, the app in the factory slot) for USB; `.ota.bin` is the app alone,
# what the player takes over the air, from the card, or from the page.
"${esptool[@]}" --chip "$chip" merge-bin -o "$out/$stem.factory.bin" \
    --flash-mode "$mode" --flash-freq "$freq" --flash-size "$size" \
    "$boot_off" "$out/$stem-bootloader.bin" \
    "$pt_off" "$out/$stem-partition-table.bin" \
    "$app_off" "$out/$stem.ota.bin"

# The firmware carries `build:<git describe>` and logs it at boot. The image
# must carry this tree's id, or it is some other build and must not be
# flashed as this one.
want="build:$(git describe --always --dirty --tags 2>/dev/null || echo unknown)"
if grep -q -a -F -- "$want" "$out/$stem.ota.bin"; then
  echo "image carries $want"
else
  echo "image does not carry $want: it is not a build of this tree" >&2
  exit 1
fi

(cd "$out" && sha256sum "$stem"*.bin | tee "$stem.SHA256SUMS")
echo "flash (USB, the whole flash): esptool --chip $chip --port <PORT> write-flash 0x0 dist/$stem.factory.bin"
echo "update (over the air):        scripts/push.sh <player> dist/$stem.ota.bin"
