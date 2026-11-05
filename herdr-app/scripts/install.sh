#!/bin/sh
# Links the sim-prs mod (built from src/ by `npm run build`) into the Herdr
# app's home, so a `git pull` here updates it, and adds the main-sim layout. Never overwrites: a layout already there is
# left as it is (it may hold your edits), and a sim-prs folder that isn't this
# link stops the install.
set -eu
src=$(cd "$(dirname "$0")/.." && pwd)
app=${HERDR_APP_HOME:-$HOME/.config/herdr-app}
mod="$app/mods/sim-prs"

if [ -L "$mod" ] && [ "$(readlink "$mod")" = "$src/mods/sim-prs" ]; then
  echo "mod: already linked"
elif [ -e "$mod" ] || [ -L "$mod" ]; then
  echo "mod: $mod exists and isn't a link to this checkout; move it away first" >&2
  exit 1
else
  ln -s "$src/mods/sim-prs" "$mod"
  echo "mod: linked $mod"
fi

# A layout already there is replaced only when it is a version this repo
# committed (an earlier install, unedited); one with the person's edits stays.
for f in main-sim.json main-sim.svg; do
  dest="$app/layouts/$f"
  if [ ! -e "$dest" ]; then
    cp "$src/layouts/$f" "$dest"
    echo "layout: added $f"
  elif cmp -s "$src/layouts/$f" "$dest"; then
    echo "layout: $f up to date"
  elif git -C "$src" log --format=%H -- "layouts/$f" | while read -r rev; do
    git -C "$src" show "$rev:herdr-app/layouts/$f" 2>/dev/null | cmp -s - "$dest" && exit 0
  done; then
    cp "$src/layouts/$f" "$dest"
    echo "layout: updated $f"
  else
    echo "layout: $f has your edits, kept (diff it with $src/layouts/$f)"
  fi
done

# keys.json: add this layout's keys, never replacing one the person bound.
python3 - "$src/keys.json" "$app/keys.json" <<'PY'
import json, sys
ours, path = json.load(open(sys.argv[1])), sys.argv[2]
try:
    keys = json.load(open(path))
except FileNotFoundError:
    keys = {}
added = []
for section, binds in ours.items():
    have = keys.setdefault(section, {})
    for key, action in binds.items():
        if key not in have:
            have[key] = action
            added.append(f"{section} {key}")
        elif have[key] != action:
            print(f"keys: {section} {key} is yours ({have[key]}), kept")
if added:
    with open(path, "w") as f:
        json.dump(keys, f, indent=2, ensure_ascii=False)
        f.write("\n")
    print("keys: added " + ", ".join(added))
PY

herdr-app check "$mod"

# The panel runs `herdr-sim --prs-json`. An older herdr-sim ignores the flag and
# prints nothing, so look for the JSON rather than the exit code.
if ! command -v herdr-sim >/dev/null 2>&1; then
  echo "herdr-sim isn't on PATH: the panel will show an error until it is" >&2
  echo "  (cargo build --release here, then link target/release/herdr-sim into ~/.local/bin)" >&2
elif ! herdr-sim --prs-json 2>/dev/null | grep -q '"prs"'; then
  echo "herdr-sim on PATH ($(command -v herdr-sim)) doesn't know --prs-json: update it" >&2
fi
echo "Switch to it with: herdr-app do layout.switch --name main-sim"
