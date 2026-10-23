#!/usr/bin/env bash
# Send a firmware image to a player over the network.
#
# Usage: scripts/push.sh <player> [dist/<stem>.ota.bin] [--factory] [--user NAME]
#
#   <player>   the player's name or address (evo-player.local, 192.0.2.7)
#   .ota.bin   the app image from scripts/image.sh; the newest in dist/
#              when left out
#   --factory  instead of an update, ask the player to keep the firmware
#              it runs now as its factory copy (no image is sent)
#   --user     the page's name when the card locks it (web_user); curl
#              asks for the password, or takes it from EVO_WEB_PASSWORD
#
# The player stops playback, writes the image into the update slot it is
# not running from, answers with what it did, and restarts into it. The
# new firmware boots once as pending and confirms itself after a healthy
# start; a reset before that returns the player to the firmware it had.
# The page at http://<player>/ does the same from a browser.

set -euo pipefail

usage() { echo "usage: $0 <player> [dist/<stem>.ota.bin] [--factory] [--user NAME]" >&2; exit 2; }
[ $# -ge 1 ] || usage

player="$1"; shift
image=""
factory=0
user=""
while [ $# -gt 0 ]; do
  case "$1" in
    --factory) factory=1 ;;
    --user) shift; user="${1:-}"; [ -n "$user" ] || usage ;;
    -*) usage ;;
    *) image="$1" ;;
  esac
  shift
done

root="$(cd "$(dirname "$0")/.." && pwd)"
curl=(curl --silent --show-error --fail-with-body --max-time 600)
if [ -n "$user" ]; then
  # Digest: the password never crosses the network in clear. Given in
  # EVO_WEB_PASSWORD, or curl asks on the terminal.
  if [ -n "${EVO_WEB_PASSWORD:-}" ]; then
    curl+=(--digest --user "$user:$EVO_WEB_PASSWORD")
  else
    curl+=(--digest --user "$user")
  fi
fi

if [ "$factory" -eq 1 ]; then
  "${curl[@]}" -X POST "http://$player/factory"
  exit
fi

if [ -z "$image" ]; then
  image=$(find "$root/dist" -maxdepth 1 -name '*.ota.bin' -printf '%T@ %p\n' 2>/dev/null | sort -rn | head -1 | cut -d' ' -f2-)
  [ -n "$image" ] || { echo "no dist/*.ota.bin: run scripts/image.sh first" >&2; exit 1; }
fi
[ -f "$image" ] || { echo "no such image: $image" >&2; exit 1; }

# The status first: a locked page refuses here, before 4 MB are sent.
before=$("${curl[@]}" "http://$player/status") || {
  [ -n "$user" ] || echo "the page is locked: pass --user NAME (web_user on the card)" >&2
  exit 1
}
echo "before: $(echo "$before" | head -1)"
echo "sending $(basename "$image") ($(stat -c %s "$image") bytes) to $player"
"${curl[@]}" -X POST -H 'Content-Type: application/octet-stream' \
  --data-binary "@$image" "http://$player/update"
