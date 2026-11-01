#!/usr/bin/env bash
# Name the code addresses in a heap-trace dump (or any console text).
#
# Usage: scripts/symbolise.sh <elf> < console-excerpt
# Needs: riscv32-esp-elf-gdb on PATH or named by EVO_GDB
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

cmds=$(mktemp)
for a in "${addrs[@]}"; do echo "info symbol $a"; done > "$cmds"
mapfile -t names < <("$gdb" -batch -x "$cmds" "$elf" 2>/dev/null \
  | sed -E 's/ in section .*//; s/^No symbol matches .*//')
rm -f "$cmds"

sed_script=$(mktemp)
for i in "${!addrs[@]}"; do
  name="${names[$i]:-}"
  [ -n "$name" ] || continue
  # Rust's mangled names are long; keep the path's tail.
  short=$(sed -E 's/^<?(.*::)?([^:<>]+(<[^>]*>)?)>? \+ ([0-9]+)$/\2+\4/' <<<"$name")
  printf 's|%s\\b|%s <%s>|g\n' "$a" "$a" "${short//|/\\|}" >> "$sed_script"
done
sed -f "$sed_script" <<<"$text"
rm -f "$sed_script"
