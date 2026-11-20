#!/bin/bash
# Asserts nothing in Wake's window is cut off at any width: runs `wake --show clips` (every view,
# side panel closed and open, the switcher, a dialog, the `?` Keys sheet) on an offscreen Broadway display at each
# width and fails if any line says CLIPPED. Needs gtk4-broadwayd; run after `cargo build`:
#   tests/wake-clips.sh [WIDTH ...]      (default 480 to 1600)
# Uses a scratch config with reduced motion (Broadway's frame clock doesn't run animations).
set -u
cd "$(dirname "$0")/.." || exit
widths=("$@")
[ ${#widths[@]} -eq 0 ] && widths=(480 560 640 700 760 820 900 1000 1075 1180 1300 1600)
cfg=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/wake-clips.XXXXXX")
mkdir -p "$cfg/wake"
printf '[look]\nmotion = "reduced"\n' > "$cfg/wake/settings.toml"
display=:$((20 + RANDOM % 20))
gtk4-broadwayd "$display" >/dev/null 2>&1 &
bpid=$!
trap 'kill $bpid 2>/dev/null; rm -rf "$cfg"' EXIT
sleep 1
bad=0
for wd in "${widths[@]}"; do
  out=$(XDG_CONFIG_HOME=$cfg WAKE_SHOT_WIDTH=$wd GDK_BACKEND=broadway BROADWAY_DISPLAY=$display \
        timeout 180 target/debug/wake --demo --screenshot "$cfg/x.png" --show clips 2>/dev/null)
  echo "$out" | grep -E '^(clips|    )'
  if ! echo "$out" | grep -q '^clips done'; then
    echo "clips width=$wd: the check didn't finish"; bad=1
  fi
  echo "$out" | grep -q 'CLIPPED' && bad=1
done
[ $bad -eq 0 ] && echo "PASS: nothing cut off at ${widths[*]}" || echo "FAIL"
exit $bad
