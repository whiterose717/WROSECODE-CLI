#!/usr/bin/env sh
set -eu
base=${WROSECODE_RELEASE_BASE:?Set WROSECODE_RELEASE_BASE to the published release-asset URL}
os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
case "$arch" in x86_64|amd64) arch=x86_64;; aarch64|arm64) arch=aarch64;; *) echo "unsupported architecture: $arch" >&2; exit 1;; esac
case "$os" in linux|darwin) ;; *) echo "unsupported OS: $os" >&2; exit 1;; esac
name="wrosecode-${os}-${arch}"
dest=${WROSECODE_INSTALL_DIR:-"$HOME/.local/bin"}
mkdir -p "$dest"
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
curl -fsSL "$base/$name" -o "$tmp"
chmod 755 "$tmp"
mv "$tmp" "$dest/wrosecode"
printf 'Installed %s\n' "$dest/wrosecode"
