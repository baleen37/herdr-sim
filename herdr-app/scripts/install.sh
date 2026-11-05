#!/bin/sh
# Links the sim-prs mod into the Herdr app's home, so a `git pull` here updates
# it, and adds the main-sim layout. Never overwrites: a layout already there is
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
echo "Switch to it with: herdr-app do layout.switch --name main-sim"
