#!/bin/sh
# Run by `herdr plugin install`: download the release binary matching this
# manifest's version into $HERDR_PLUGIN_ROOT/bin, where the hooks run it.
# `herdr plugin link` skips build steps; see README for local builds.
set -eu

root="${HERDR_PLUGIN_ROOT:-$(pwd)}"
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/herdr-plugin.toml" | head -n 1)

case "$(uname -s)-$(uname -m)" in
Darwin-arm64) target=aarch64-apple-darwin ;;
Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
*)
	echo "herdr-sim: no prebuilt binary for $(uname -sm); build it with cargo into $root/bin" >&2
	exit 1
	;;
esac

archive="herdr-sim-$target.tar.xz"
url="https://github.com/baleen37/herdr-sim/releases/download/v$version/$archive"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -fsSL "$url" -o "$tmp/$archive"
curl -fsSL "$url.sha256" -o "$tmp/$archive.sha256"
expected=$(cut -d ' ' -f 1 "$tmp/$archive.sha256")
if command -v sha256sum >/dev/null 2>&1; then
	actual=$(sha256sum "$tmp/$archive" | cut -d ' ' -f 1)
else
	actual=$(shasum -a 256 "$tmp/$archive" | cut -d ' ' -f 1)
fi
if [ "$expected" != "$actual" ]; then
	echo "herdr-sim: checksum mismatch for $archive" >&2
	exit 1
fi

mkdir -p "$tmp/x" "$root/bin"
tar -xJf "$tmp/$archive" -C "$tmp/x" --strip-components=1
mv "$tmp/x/herdr-sim" "$root/bin/herdr-sim"
chmod +x "$root/bin/herdr-sim"
