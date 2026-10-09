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
curl -fsSL "https://github.com/$REPO/releases/download/$VERSION/oxdock-$TARGET.tar.gz" | tar -xz -C "$tmp"
install -m 755 "$tmp"/oxdock* "$INSTALL_DIR/"
echo "installed oxdock $VERSION to $INSTALL_DIR"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "note: $INSTALL_DIR is not on PATH" >&2 ;;
esac
