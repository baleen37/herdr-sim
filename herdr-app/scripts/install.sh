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

for f in main-sim.json main-sim.svg; do
  if [ -e "$app/layouts/$f" ]; then
    echo "layout: $f already there, kept (diff it with $src/layouts/$f)"
  else
    cp "$src/layouts/$f" "$app/layouts/$f"
    echo "layout: added $f"
  fi
done

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
