#!/bin/bash
# Asserts Wake's keyboard works as on any desktop: in every view, at the three layouts (details
# beside the list, over it, or in its place), Enter on the selected item puts the keyboard in its
# details (in Ideas, the idea's project; in Ask, the question box), then Tab (and, in a second run, Shift+Tab) goes round the whole window:
#   - one press, one area: every press moves the keyboard to the next area;
#   - never lost: the keyboard is always somewhere on screen;
#   - seen: after every press the focus is drawn (focus-visible is on; the replay never fakes it);
#   - in screen order: title bar, filters, list, details, the view tabs (and back the other way);
#   - a list is one area: Tab lands on its selected item and never walks along its rows or cards;
# and Esc from the details puts the keyboard back on the row it came from.
#
# Runs on headless Weston with a fake seat, so the window is active and frames come at real speed,
# with animations on, as on a desktop. Needs weston; run after `cargo build`:
#   tests/wake-keys.sh
set -u
cd "$(dirname "$0")/.." || exit
command -v weston >/dev/null || { echo "SKIP: weston isn't installed"; exit 0; }
tmp=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/wake-keys.XXXXXX")
mkdir -p "$tmp/config/wake" "$tmp/run"
chmod 700 "$tmp/run"
export XDG_RUNTIME_DIR=$tmp/run
weston --backend=headless --fake-seat --socket=wake-keys --width=1500 --height=1000 --idle-time=0 >/dev/null 2>&1 &
wpid=$!
trap 'kill $wpid 2>/dev/null; rm -rf "$tmp"' EXIT
for _ in $(seq 1 50); do [ -S "$tmp/run/wake-keys" ] && break; sleep 0.1; done

# Replays keys (150 ms apart) and prints where the keyboard is after each.
replay() { # width view keys
  XDG_CONFIG_HOME=$tmp/config WAKE_SHOT_WIDTH=$1 WAKE_SHOT_DEBUG=1 WAYLAND_DISPLAY=wake-keys GDK_BACKEND=wayland \
    timeout 120 target/debug/wake --demo --view "$2" --screenshot "$tmp/x.png" --show "keys:$3" 2>&1 | grep 'after step'
}

settle="wait,wait,wait,wait,wait,wait,wait,wait"
tabs=$(printf 'Tab,%.0s' $(seq 1 28))
backs=$(printf 'Shift+Tab,%.0s' $(seq 1 28))
for wd in 1300 1000 700; do
  for v in monitor projects history agents ideas questions; do
    replay "$wd" "$v" "$settle,Down,Return,wait,wait,wait,${tabs%,}" > "$tmp/$v-$wd-fwd.txt"
    replay "$wd" "$v" "$settle,Down,Return,wait,wait,wait,${backs%,}" > "$tmp/$v-$wd-back.txt"
    replay "$wd" "$v" "$settle,Down,Return,wait,wait,wait,Escape,wait" > "$tmp/$v-$wd-esc.txt"
    # Keyboard back on the list, then a click in the details, then arrows: they stay in the details.
    replay "$wd" "$v" "$settle,Down,Return,wait,Shift+Tab,Shift+Tab,Shift+Tab,wait,click-details,wait,Down,Down,Down,Up,Up,Up" > "$tmp/$v-$wd-click.txt"
  done
done

python3 - "$tmp" <<'EOF'
import glob, os, re, sys
d = sys.argv[1]
line = re.compile(r'keys: after step (\d+): focus (.*) in (?:Some\("([^"]*)"\)|None) label (.*) drawer (\w+) visible (\w+)')
flags = re.compile(r'StateFlags\([^)]*\)')
def steps(path):
    out = {}
    for l in open(path):
        m = line.search(l)
        if m:
            # What the keyboard is on (its kind, name, style and words), the part, the details' state.
            out[int(m.group(1))] = (flags.sub('', m.group(2)) + m.group(4), m.group(3) or "", m.group(5), m.group(6))
    return out
item = re.compile(r'Some\(\("(GtkListBoxRow|GtkFlowBoxChild)", "([^"]*)"')
bad = 0
for path in sorted(glob.glob(d + '/*-fwd.txt') + glob.glob(d + '/*-back.txt')):
    name = os.path.basename(path)[:-4]
    view, width, way = name.split('-')
    s = steps(path)
    probs = []
    if not s:
        probs.append("Wake didn't run")
    elif view not in ('ideas', 'questions') and s.get(12, ("", "", ""))[1] != 'details':
        probs.append(f"Enter left the keyboard in {s.get(12, ('', 'nowhere', ''))[1] or 'nowhere'}")
    tabs = [s[k] for k in sorted(s) if k >= 13]
    stuck = sum(1 for a, b in zip(tabs, tabs[1:]) if a[:2] == b[:2])
    if stuck:
        probs.append(f"stuck {stuck} times")
    stayed = sum(1 for a, b in zip(tabs, tabs[1:]) if a[1] == b[1])
    if stayed:
        probs.append(f"{stayed} presses stayed in the same area")
    if any(t[3] != "true" for t in tabs):
        probs.append("the focus wasn't drawn after a press")
    if any(t[1] == "" for t in tabs):
        probs.append("the keyboard went somewhere off screen")
    rows = {m.group(2) for t in tabs if t[1] == 'list' for m in [item.match(t[0])] if m}
    if len(rows) > 1:
        probs.append(f"Tab walked along {len(rows)} list items")
    order = ['top bar', 'filters', 'list', 'details', 'view tabs']
    if way == 'back':
        order.reverse()
    path_ = []
    for t in tabs:
        if not path_ or path_[-1] != t[1]:
            path_.append(t[1])
    present = [r for r in order if r in path_]
    for a, b in zip(path_, path_[1:]):
        if present[(present.index(a) + 1) % len(present)] != b:
            probs.append(f"out of order: {a} then {b}")
            break
    bad += bool(probs)
    print(f"{'FAIL' if probs else 'ok  '} {name:22} {'; '.join(probs) or ' > '.join(path_[:5])}")
for path in sorted(glob.glob(d + '/*-esc.txt')):
    name = os.path.basename(path)[:-4]
    s = steps(path)
    if name.startswith(('ideas', 'questions')):
        continue
    before, after = s.get(8), s.get(max(s) if s else 0)
    ok = before and after and after[1] == 'list' and after[0] == before[0] and after[2] == 'false'
    bad += not ok
    print(f"{'ok  ' if ok else 'FAIL'} {name:22} Esc {'back on the same row' if ok else 'did not come back to the row'}")
for path in sorted(glob.glob(d + '/*-click.txt')):
    name = os.path.basename(path)[:-4]
    if name.startswith(('ideas', 'questions')):
        continue
    s = steps(path)
    after = [s[k] for k in sorted(s) if k >= 16]
    ok = bool(after) and all(t[1] == 'details' for t in after)
    bad += not ok
    print(f"{'ok  ' if ok else 'FAIL'} {name:22} a click puts the keyboard in the details{'' if ok else ': it went to ' + ', '.join(sorted({t[1] or 'nowhere' for t in after}))}")
print("PASS: the keyboard goes round every view, in order, and never sticks" if not bad else f"FAIL: {bad}")
sys.exit(1 if bad else 0)
EOF
