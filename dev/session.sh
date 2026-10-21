#!/bin/sh
# Opens herdr-sim's dev session: a named Herdr session whose plugin hooks run
# this checkout's build and whose config is ~/.config/herdr/dev.toml, so a
# change can be tried there while the default session keeps the installed
# version and its own config.
#
#   dev/session.sh               build, then attach to (or start) "sim-dev"
#   herdr session stop sim-dev   needed for a new binary path or config path
#                                to reach the server: it reads them at start
#
# Relies on the installed manifest's ${HERDR_SIM_BIN:-...} commands; the
# plugin's state for the session lives under sessions/<name> of its state dir.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
config="${XDG_CONFIG_HOME:-$HOME/.config}/herdr"
session="${HERDR_SIM_SESSION:-sim-dev}"
cargo build --release --quiet --manifest-path "$root/Cargo.toml"
# A copy of the live config to start from; edit it freely.
[ -e "$config/dev.toml" ] || cp -L "$config/config.toml" "$config/dev.toml"
# Started from inside an agent's pane, the server would hand that agent's
# markers to every pane it opens (Claude Code then keeps no transcript, so
# sessions have no titles).
for var in $(env | sed -n 's/^\(CLAUDE[A-Z_]*\)=.*/\1/p'); do unset "$var"; done
export HERDR_CONFIG_PATH="$config/dev.toml"
export HERDR_SIM_BIN="$root/target/release/herdr-sim"
exec herdr --session "$session" "$@"
