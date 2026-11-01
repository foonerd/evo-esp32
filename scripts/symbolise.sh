#!/usr/bin/env bash
# Name the code addresses in a heap-trace dump (or any console text).
#
# Usage: scripts/symbolise.sh <elf> < console-excerpt
# Needs: riscv32-esp-elf-gdb on PATH or named by EVO_GDB; python3
#
# Every 0x4xxxxxxx in the input is looked up once in the ELF and the
# text is printed again with "0xADDR" followed by "<symbol+off>"; an
# address outside the ELF's code stays as it was.

set -euo pipefail

[ $# -eq 1 ] || { echo "usage: $0 <elf> < text" >&2; exit 2; }
elf="$1"
[ -f "$elf" ] || { echo "no such ELF: $elf" >&2; exit 1; }
gdb="${EVO_GDB:-riscv32-esp-elf-gdb}"
command -v "$gdb" >/dev/null || { echo "$gdb is not on PATH; set EVO_GDB" >&2; exit 1; }

text=$(cat)
mapfile -t addrs < <(grep -oE '0x4[0-9a-fA-F]{7}' <<<"$text" | sort -u)
[ "${#addrs[@]}" -gt 0 ] || { printf '%s\n' "$text"; exit 0; }

# One gdb run; each lookup is preceded by a marker line so the answers
# (one or two lines each: the code section and the overlapping rodata
# placeholder) can be told apart.
cmds=$(mktemp)
for a in "${addrs[@]}"; do
  printf 'echo @@%s\\n\ninfo symbol %s\n' "$a" "$a"
done > "$cmds"
answers=$(mktemp)
"$gdb" -batch -x "$cmds" "$elf" > "$answers" 2>/dev/null || true
python3 - "$text" "$answers" <<'PY'
import re, sys
text = sys.argv[1]
names = {}
addr = None
for line in open(sys.argv[2]):
    line = line.rstrip("\n")
    if line.startswith("@@"):
        addr = line[2:]
        continue
    if addr is None or "No symbol matches" in line or "_flash_rodata_dummy_start" in line:
        continue
    m = re.match(r"(.*) in section (\S+)", line)
    if not m:
        continue
    name = m.group(1)
    # Rust paths are long; keep the last segment and the offset.
    m2 = re.match(r"^<?(.*?)>? \+ (\d+)$", name)
    if m2:
        path, off = m2.group(1), m2.group(2)
        tail = re.split(r"::(?![^<]*>)", path)[-1]
        name = f"{tail}+{off}"
    names.setdefault(addr, name)
def sub(m):
    a = m.group(0)
    return f"{a} <{names[a]}>" if a in names else a
print(re.sub(r"0x4[0-9a-fA-F]{7}", sub, text))
PY
rm -f "$cmds" "$answers"
