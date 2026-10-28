#!/bin/sh
# Install rgtop from GitHub releases:
#   curl -fsSL https://raw.githubusercontent.com/xianull/rgtop/main/install.sh | sh
# Options (environment variables):
#   RGTOP_VERSION=v4.0.0       a specific release instead of the latest
#   RGTOP_INSTALL_DIR=/usr/local/bin   where to put the binary (default ~/.local/bin)
set -eu

repo=xianull/rgtop
version=${RGTOP_VERSION:-latest}
dir=${RGTOP_INSTALL_DIR:-$HOME/.local/bin}

die() { echo "rgtop install: $*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) os=unknown-linux-musl ;;
  Darwin) os=apple-darwin ;;
  *) die "unsupported OS: $(uname -s)" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) die "unsupported CPU: $(uname -m)" ;;
esac
asset="rgtop-$arch-$os.tar.gz"
if [ "$version" = latest ]; then
  base="https://github.com/$repo/releases/latest/download"
else
  base="https://github.com/$repo/releases/download/$version"
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  die "need curl or wget"
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha() { sha256sum "$1" | cut -d' ' -f1; }
else
  sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "downloading $base/$asset"
fetch "$base/$asset" "$tmp/$asset" || die "download failed"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "checksum download failed"
want=$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
[ -n "$want" ] && [ "$want" = "$(sha "$tmp/$asset")" ] || die "checksum mismatch for $asset"

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$dir"
rm -f "$dir/rgtop" # replace a symlink (e.g. an old pip/uv install) instead of writing through it
cp "$tmp/rgtop" "$dir/rgtop"
chmod 755 "$dir/rgtop"
echo "installed $("$dir/rgtop" --version) to $dir/rgtop"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "note: $dir is not on your PATH; add:  export PATH=\"$dir:\$PATH\"" ;;
esac
