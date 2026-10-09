#!/usr/bin/env bash
# Install the oxdock binary from GitHub releases.
#
#   curl -fsSL https://raw.githubusercontent.com/jzombie/rust-oxdock/main/install.sh | bash
#
# Pin explicitly with VERSION (a tag: the API's "latest" skips
# pre-releases, and every release here is `-alpha` until stable):
#
#   VERSION=v0.24.1-alpha curl -fsSL ... | bash
#
# Choose the destination with INSTALL_DIR (default ~/.local/bin).
set -euo pipefail

REPO="jzombie/rust-oxdock"
VERSION="${VERSION:-}"

if [ -z "$VERSION" ]; then
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases?per_page=1" | grep -m1 '"tag_name"' | cut -d'"' -f4)
fi

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) TARGET="aarch64-apple-darwin" ;;
  Linux-x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
  MINGW64*-x86_64 | MSYS*-x86_64 | CYGWIN*-x86_64) TARGET="x86_64-pc-windows-msvc" ;;
  MINGW64*-aarch64 | MSYS*-aarch64 | CYGWIN*-aarch64) TARGET="aarch64-pc-windows-msvc" ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$INSTALL_DIR"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
base="https://github.com/$REPO/releases/download/$VERSION"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"
curl -fsSL "$base/oxdock-$TARGET.tar.gz" -o "$tmp/oxdock-$TARGET.tar.gz"
# Fail closed: no checksums file (releases before checksums shipped),
# no install. sha256sum is GNU coreutils; macOS falls back to shasum.
# The asset keeps its release filename so `-c` resolves it.
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$tmp" && grep "oxdock-$TARGET.tar.gz$" SHA256SUMS | sha256sum -c -)
else
  (cd "$tmp" && grep "oxdock-$TARGET.tar.gz$" SHA256SUMS | shasum -a 256 -c -)
fi
mkdir -p "$tmp/x"
tar -xzf "$tmp/oxdock-$TARGET.tar.gz" -C "$tmp/x"
install -m 755 "$tmp"/x/oxdock* "$INSTALL_DIR/"
echo "installed oxdock $VERSION to $INSTALL_DIR"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "note: $INSTALL_DIR is not on PATH" >&2 ;;
esac
