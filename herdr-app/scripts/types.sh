#!/bin/sh
# Copies the Herdr app's own type files (rewritten by the app to match its
# version) into types/ for `npm run check`. Not committed: they are the app's.
set -eu
src=$(cd "$(dirname "$0")/.." && pwd)
app=${HERDR_APP_HOME:-$HOME/.config/herdr-app}
mkdir -p "$src/types"
cp "$app/gpui-kit.d.ts" "$src/types/gpui-kit.d.ts"
# herdr.d.ts is written as a module file; wrap it so `import ... from "herdr"` resolves.
{ echo 'declare module "herdr" {'; cat "$app/herdr.d.ts"; echo '}'; } > "$src/types/herdr.d.ts"
