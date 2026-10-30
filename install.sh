#!/usr/bin/env bash
# Install the OxDock binary from GitHub releases. Thin fetcher only:
# every install decision lives in install.oxfile, executed below with
# the fetched binary.
#
# Piped stdin is saved to a scratch file and bridged to install.oxfile
# as OXDOCK_PIPED_SCRIPT.
#
# Content-addressed local cache: the first run downloads and verifies,
# later runs reuse the cached tarball after re-verifying its hash, so
# reruns need no network once warm (pin VERSION to stay fully offline).
# Override the cache root with OXDOCK_CACHE_DIR.

#
#   curl -fsSL https://raw.githubusercontent.com/jzombie/rust-oxdock/main/install.sh | bash
#
# Pin explicitly with VERSION (a tag: the API's "latest" skips
# pre-releases, and every release here is `-alpha` until stable):
#
#   (export VERSION=v0.32.3-alpha; curl -fsSL https://raw.githubusercontent.com/jzombie/rust-oxdock/main/install.sh | bash)
#
# Pass INSTALL_DIR to choose the destination; the installer defaults
# it when absent.
#
# Pass OXDOCK_BINARY_NAME to choose the installed stem (default
# `oxdock`; the installer appends `.exe` on Windows). CI passes
# `oxdock-engine` to free the bare name for a binary under test.
#
# Point OXDOCK_ENGINE at a branch-built binary to run the installer
# with it instead of the downloaded release (CI smoke proves behavior
# fixes before any release ships them). The asset still downloads and
# verifies; only the execution engine swaps.
#
# Point OXDOCK_RELEASE_BASE at a mirror root (no trailing slash) to
# fetch the asset and checksums from elsewhere; CI smoke serves a
# staged branch build over loopback through it.

set -euo pipefail

REPO="jzombie/rust-oxdock"

VERSION="${VERSION:-}"

CACHE_DIR="${OXDOCK_CACHE_DIR:-$HOME/.cache/oxdock}"

# Shared runner IPs burn the anonymous API quota: authenticate reads
# when a token is present (CI bridges GITHUB_TOKEN; users never need
# one). A function, not a flags string: quoted flag strings word-split
# into garbage on expansion.
gh_curl() {
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    curl -fsSL -H "Authorization: Bearer $GITHUB_TOKEN" "$@"
  else
    curl -fsSL "$@"

  fi
}

if [ -z "$VERSION" ] || [ "$VERSION" = "latest" ]; then

  # No `grep -m1`: every stage consumes the body to EOF, else curl
  # fails writing to the closed pipe under `pipefail` (exit 23).
  # The releases endpoint yields exactly one `tag_name` per page entry.
  VERSION=$(gh_curl "https://api.github.com/repos/$REPO/releases?per_page=1" | grep '"tag_name"' | cut -d'"' -f4)

fi

# Install name and expected version ride through untouched: the
# single strip lives in install.oxfile (TEXT_TRIM_SUFFIX/PREFIX),
# never here.

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) TARGET="aarch64-apple-darwin" ;;

  Linux-x86_64) TARGET="x86_64-unknown-linux-musl" ;;

  Linux-aarch64) TARGET="aarch64-unknown-linux-musl" ;;

  # Emulated shells lie about -m: x64 Git Bash on ARM64 Windows reports
  # x86_64, and msys does not forward PROCESSOR_ARCHITEW6432 into the
  # shell, so the usual variable tells are absent. Two independent host
  # signals remain: the kernel release carries an -ARM64 suffix, and the
  # SysArm32 system directory exists only on ARM64 Windows (checked
  # under both msys and Cygwin drive prefixes). W6432 stays as a
  # fallback for shells that forward it. The installed binary runs
  # outside the shell, so native always wins.
  MINGW64*-x86_64 | MSYS*-x86_64 | CYGWIN*-x86_64)
    if [ "${PROCESSOR_ARCHITEW6432:-}" = "ARM64" ]; then
      TARGET="aarch64-pc-windows-msvc"
    else
      case "$(uname -s)" in
        *-ARM64) TARGET="aarch64-pc-windows-msvc" ;;

        *)
          if [ -d "/c/Windows/SysArm32" ] || [ -d "/cygdrive/c/Windows/SysArm32" ]; then
            TARGET="aarch64-pc-windows-msvc"
          else
            TARGET="x86_64-pc-windows-msvc"
          fi ;;
      esac
    fi ;;

  MINGW64*-aarch64 | MSYS*-aarch64 | CYGWIN*-aarch64) TARGET="aarch64-pc-windows-msvc" ;;

  *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

# Platform forensics: which detection inputs chose the asset below, so a
# mistargeted download names its cause instead of surfacing as a bare 404.
echo "platform: $(uname -s)-$(uname -m) target: $TARGET" >&2

ASSET="oxdock-$TARGET.tar.gz"

VDIR="$CACHE_DIR/$VERSION"

mkdir -p "$VDIR"

verify_cached() {
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$VDIR" && grep "$ASSET\$" SHA256SUMS 2>/dev/null | sha256sum -c - >/dev/null 2>&1)
  else
    (cd "$VDIR" && grep "$ASSET\$" SHA256SUMS 2>/dev/null | shasum -a 256 -c - >/dev/null 2>&1)

  fi
}

# Same hash as last time means same bytes: reuse the cached tarball
# after re-verifying it. Anything missing or mismatched falls through
# to a fresh download, so a corrupt cache heals itself. The executing
# stub itself is part of the warm contract: a cache warmed by the other
# stub, or by a release predating bundled installers, repopulates
# instead of handing off to a file that is not there.
if [ -f "$VDIR/$ASSET" ] && [ -f "$VDIR/SHA256SUMS" ] && [ -f "$VDIR/install.oxfile" ] && [ -f "$VDIR/install.sh" ] && verify_cached; then
  echo "using cached oxdock $VERSION" >&2
else
  echo "downloading oxdock $VERSION" >&2

  tmp=$(mktemp -d)

  trap 'rm -rf "$tmp"' EXIT

  base="${OXDOCK_RELEASE_BASE:-https://github.com/$REPO/releases/download/$VERSION}"

  base="${base%/}"

  # Download under the release filename: `sha256sum -c` resolves names
  # from the checksums file, so a renamed download would never verify.
  gh_curl "$base/$ASSET" -o "$tmp/$ASSET"

  gh_curl "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"

  # Installer logic rides with the release bundle. An explicit
  # OXDOCK_OXFILE_URL wins over the bundle (pre-merge testing,
  # mirrors). Fetched here, inside population, so warm runs never
  # touch network.
  if [ -n "${OXDOCK_OXFILE_URL:-}" ]; then
    gh_curl "$OXDOCK_OXFILE_URL" -o "$tmp/install.oxfile"
  else
    gh_curl "$base/install.oxfile" -o "$tmp/install.oxfile"

  fi

  # The bundled stubs travel beside everything else so execution below
  # hands off to logic matching the engine version, and so a cache warmed
  # by one stub serves the other: every supported release lists both. A
  # missing entry fails closed at verification.
  gh_curl "$base/install.sh" -o "$tmp/install.sh"

  gh_curl "$base/install.ps1" -o "$tmp/install.ps1"
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp" && grep "$ASSET\$" SHA256SUMS | sha256sum -c - >/dev/null)
  else
    (cd "$tmp" && grep "$ASSET\$" SHA256SUMS | shasum -a 256 -c - >/dev/null)

  fi

  mv "$tmp/$ASSET" "$VDIR/$ASSET"

  mv "$tmp/SHA256SUMS" "$VDIR/SHA256SUMS"

  mv "$tmp/install.oxfile" "$VDIR/install.oxfile"

  mv "$tmp/install.sh" "$VDIR/install.sh"

  mv "$tmp/install.ps1" "$VDIR/install.ps1"

  trap - EXIT

  rm -rf "$tmp"

fi

# Provenance for smoke forensics: which oxfile payload executes below
# and its digest. A version-skewed payload names its source here
# instead of surfacing as a bare parse error downstream.
oxfile_src="${OXDOCK_OXFILE_URL:-release $VERSION}"
if command -v sha256sum >/dev/null 2>&1; then
  oxfile_digest=$(sha256sum "$VDIR/install.oxfile" | cut -d' ' -f1)
else
  oxfile_digest=$(shasum -a 256 "$VDIR/install.oxfile" | cut -d' ' -f1)

fi

echo "installer oxfile: $oxfile_src $oxfile_digest" >&2

EXPECTED=$(grep "$ASSET\$" "$VDIR/SHA256SUMS" | cut -d' ' -f1)

# The installer runs from the cache, never the network: population
# above stored the release's own logic beside its bytes.
tmp=$(mktemp -d)

trap 'rm -rf "$tmp"' EXIT

cp "$VDIR/install.oxfile" "$tmp/install.oxfile"

mkdir -p "$tmp/x"

tar -xzf "$VDIR/$ASSET" -C "$tmp/x"

chmod +x "$tmp"/x/oxdock*

# Assignment words never glob-expand: resolve the unpacked binary
# explicitly, and fail closed when the tarball held nothing expected.
BIN=$(echo "$tmp"/x/oxdock*)

[ -f "$BIN" ] || { echo "no binary unpacked from $ASSET" >&2; exit 1; }

# INSTALL_DIR passes through untouched (possibly unset): the name
# mapping lives here because only the stub knows both sides.
# An explicit OXDOCK_ENGINE runs the installer instead of the unpacked
# release; it must name an executable file, and fails closed otherwise.
ENGINE="$BIN"
if [ -n "${OXDOCK_ENGINE:-}" ]; then

  # Resolve relative to the caller's directory: execution below runs
  # from the scratch dir, where a relative path would no longer point
  # at the engine.
  case "$OXDOCK_ENGINE" in
    /*|[A-Za-z]:*) ENGINE="$OXDOCK_ENGINE" ;;

    *) ENGINE="$PWD/$OXDOCK_ENGINE" ;;
  esac
fi

[ -x "$ENGINE" ] || { echo "installer engine is not executable: $ENGINE" >&2; exit 1; }

# Destination default lives here in native shell; empty keeps the
# installer's loud gate instead of rooted garbage. Under Git Bash the
# msys HOME shadows the real profile, so Windows takes the same
# WindowsApps default as install.ps1 instead of $HOME/.local/bin.
OXDOCK_DIR="${INSTALL_DIR:-}"
case "$(uname -s)" in
  MINGW64*|MSYS*|CYGWIN*)
    if [ -z "$OXDOCK_DIR" ] && [ -n "${LOCALAPPDATA:-}" ]; then
      OXDOCK_DIR="$LOCALAPPDATA\Microsoft\WindowsApps"

    fi

    ;;

  *)
    if [ -z "$OXDOCK_DIR" ] && [ -n "${HOME:-}" ]; then
      OXDOCK_DIR="$HOME/.local/bin"

    fi

    ;;
esac

# A function so piped delivery is fully read before the drain runs;
# straight-line code would let it swallow unread script tail. The
# handoff lives in here, after the drain, for the same reason: a
# top-level exec would re-run the prologue below against a stdin
# offset in the middle of the script, and the re-executed copy would
# bridge stub tail as a user script.
run_installer() {

# Stdin capture, no decisions: a TTY is never drained. Under handoff
# re-entry the outer copy already drained, so inherit its capture
# instead of reading the pipe twice.
if [ -z "${OXDOCK_STUB_ACTIVE:-}" ]; then
  OXDOCK_PIPED_SCRIPT=""
  if [ ! -t 0 ]; then
    stdin_tmp=$(mktemp)

    trap 'rm -rf "$tmp" "$stdin_tmp"' EXIT

    cat > "$stdin_tmp"
    if [ -s "$stdin_tmp" ]; then
      OXDOCK_PIPED_SCRIPT="$stdin_tmp"
    else
      rm -f "$stdin_tmp"

    fi
  fi
fi

# Interactive consoles confirm the destination in the oxfile: export the
# flag only with a console behind stdin. Re-probed, never inherited:
# the descriptor survives re-entry unchanged, so the answer matches.
OXDOCK_INTERACTIVE=""
if [ -t 0 ]; then
  OXDOCK_INTERACTIVE=true

fi

# Hand off to the release-bundled installer: the executed logic always
# matches the engine version, and warm runs keep working because the
# bundled copy lives in the cache beside everything else. The flag
# prevents re-entry: the re-executed copy skips this block and the
# drain above. Digest verification fails closed when the entry is
# missing or mismatched.
if [ -z "${OXDOCK_STUB_ACTIVE:-}" ]; then
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$VDIR" && grep " install.sh$" SHA256SUMS | sha256sum -c - >/dev/null) || { echo "installer stub failed verification" >&2; exit 1; }
  else
    (cd "$VDIR" && grep " install.sh$" SHA256SUMS | shasum -a 256 -c - >/dev/null) || { echo "installer stub failed verification" >&2; exit 1; }

  fi

  OXDOCK_STUB_ACTIVE=1

  export OXDOCK_STUB_ACTIVE

  export OXDOCK_PIPED_SCRIPT

  export OXDOCK_INTERACTIVE

  # The outer unpack stays behind: exec discards its EXIT trap, and the
  # re-executed copy unpacks the cache into its own directory below.
  rm -rf "$tmp"

  exec bash "$VDIR/install.sh"

fi

# Re-entry owns the inherited capture now: the outer trap died with
# the exec, so this process unlinks the bridged file on exit. Only
# re-entry reaches past the handoff above with a capture present.
if [ -n "${OXDOCK_PIPED_SCRIPT:-}" ] && [ -f "$OXDOCK_PIPED_SCRIPT" ]; then
  trap 'rm -rf "$tmp" "$OXDOCK_PIPED_SCRIPT"' EXIT

fi

(cd "$tmp" && \
  OXDOCK_ASSET="$VDIR/$ASSET" \
  OXDOCK_SHA="$EXPECTED" \
  OXDOCK_BIN="$BIN" \
  OXDOCK_DIR="$OXDOCK_DIR" \
  OXDOCK_VERSION="$VERSION" \
  OXDOCK_BINARY_NAME="${OXDOCK_BINARY_NAME:-}" \
  OXDOCK_EXPECTED_VERSION="$VERSION" \
  OXDOCK_SKIP_DOWNLOAD=false \
  OXDOCK_INTERPRETER="$ENGINE" \
  OXDOCK_PIPED_SCRIPT="$OXDOCK_PIPED_SCRIPT" \
  OXDOCK_INTERACTIVE="$OXDOCK_INTERACTIVE" \
  "$ENGINE" "$tmp/install.oxfile") || {
  code=$?

  echo "installer failed with exit code $code" >&2

  exit "$code"

}

# CI tail under the fresh binary: the oxfile above verified and placed
# under the old engine; re-running the same file with SKIP=1 proves the
# fresh binary runs DSL before its directory lands on PATH. Gated here
# on the PATH file alone (the oxfile owns the version gate), and never
# in piped-run mode: nothing was placed there, so no fresh binary
# exists. Off-runner installs skip silently either way.
if [ -z "${OXDOCK_PIPED_SCRIPT:-}" ] && [ -n "${GITHUB_PATH:-}" ] && [ -f "$GITHUB_PATH" ]; then
  RAW_NAME="${OXDOCK_BINARY_NAME:-oxdock}"

  case "$RAW_NAME" in
    *.exe) FRESH="$OXDOCK_DIR/$RAW_NAME" ;;
    *)
      FRESH="$OXDOCK_DIR/$RAW_NAME"

      case "$(uname -s)" in
        MINGW64*|MSYS*|CYGWIN*) FRESH="$FRESH.exe" ;;
      esac
      ;;
  esac

  (cd "$tmp" && \
    OXDOCK_DIR="$OXDOCK_DIR" \
    OXDOCK_VERSION="$VERSION" \
    OXDOCK_BINARY_NAME="$RAW_NAME" \
    OXDOCK_EXPECTED_VERSION="$VERSION" \
    OXDOCK_SKIP_DOWNLOAD=true \
    "$FRESH" "$tmp/install.oxfile") || {
    code=$?

    echo "installer failed with exit code $code (fresh tail)" >&2

    exit "$code"

  }
fi
}

run_installer
