#!/bin/sh
set -eu

REPO="${BOWLINE_RELEASE_REPO:-bowline-sh/bowline}"
VERSION="latest"
CLI_ONLY="0"
INSTALL_DIR="${BOWLINE_INSTALL_DIR:-$HOME/.local/bin}"
APP_DIR="${BOWLINE_APP_DIR:-$HOME/Applications}"

usage() {
  cat <<'EOF'
Usage: install.sh [--cli-only] [--version <version>]

Installs Bowline for the current user.

Options:
  --cli-only          Install only bowline and bowline-daemon.
  --version VERSION   Install a specific release version, for example 0.1.0.
  -h, --help          Show this help.
EOF
}

fail() {
  echo "bowline install failed: $*" >&2
  exit 1
}

note() {
  echo "bowline install: $*" >&2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --cli-only)
      CLI_ONLY="1"
      shift
      ;;
    --version)
      [ "$#" -ge 2 ] || fail "--version requires a value"
      VERSION="$2"
      shift 2
      ;;
    --version=*)
      VERSION="${1#--version=}"
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

need() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required"
}

need curl
need mktemp

UNAME_S="$(uname -s)"
UNAME_M="$(uname -m)"

case "$UNAME_S:$UNAME_M" in
  Darwin:arm64)
    PLATFORM="macos"
    TARGET="aarch64-apple-darwin"
    ;;
  Linux:x86_64)
    PLATFORM="linux"
    TARGET="x86_64-unknown-linux-gnu"
    ;;
  *)
    fail "unsupported platform $UNAME_S/$UNAME_M; see https://github.com/$REPO/releases"
    ;;
esac

case "$VERSION" in
  latest)
    RELEASE_BASE="https://github.com/$REPO/releases/latest/download"
    ;;
  v*)
    RELEASE_BASE="https://github.com/$REPO/releases/download/$VERSION"
    ;;
  *)
    RELEASE_BASE="https://github.com/$REPO/releases/download/v$VERSION"
    ;;
esac

TMPDIR="$(mktemp -d 2>/dev/null || mktemp -d -t bowline-install)"
cleanup() {
  rm -rf "$TMPDIR"
}
trap cleanup EXIT INT TERM

download() {
  url="$1"
  dest="$2"
  note "download $(basename "$dest")"
  curl -fL --retry 3 --retry-delay 1 -o "$dest" "$url"
}

sha256() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{ print $1 }'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    fail "shasum or sha256sum is required"
  fi
}

verify_checksum() {
  file="$1"
  name="$(basename "$file")"
  expected="$(
    awk -v name="$name" '$2 == name { print $1; found = 1 } END { if (!found) exit 1 }' \
      "$TMPDIR/checksums.txt" || true
  )"
  [ -n "$expected" ] || fail "missing checksum for $name"
  actual="$(sha256 "$file")"
  [ "$actual" = "$expected" ] || fail "checksum mismatch for $name"
}

install_cli_archive() {
  archive="$TMPDIR/bowline-$TARGET.tar.xz"
  need tar
  download "$RELEASE_BASE/bowline-$TARGET.tar.xz" "$archive"
  verify_checksum "$archive"
  mkdir -p "$TMPDIR/cli" "$INSTALL_DIR"
  tar -xJf "$archive" -C "$TMPDIR/cli"
  install -m 0755 "$TMPDIR/cli/bowline" "$INSTALL_DIR/bowline"
  install -m 0755 "$TMPDIR/cli/bowline-daemon" "$INSTALL_DIR/bowline-daemon"
}

install_macos_app() {
  app_zip="$TMPDIR/Bowline-$TARGET.app.zip"
  need ditto
  download "$RELEASE_BASE/Bowline-$TARGET.app.zip" "$app_zip"
  verify_checksum "$app_zip"
  mkdir -p "$APP_DIR" "$INSTALL_DIR"
  rm -rf "$APP_DIR/Bowline.app"
  ditto -x -k "$app_zip" "$APP_DIR"
  [ -x "$APP_DIR/Bowline.app/Contents/Resources/bin/bowline" ] ||
    fail "downloaded app is missing bundled bowline"
  ln -sf "$APP_DIR/Bowline.app/Contents/Resources/bin/bowline" "$INSTALL_DIR/bowline"
  ln -sf "$APP_DIR/Bowline.app/Contents/Resources/bin/bowline-daemon" "$INSTALL_DIR/bowline-daemon"
}

install_daemon() {
  if ! "$INSTALL_DIR/bowline" daemon install; then
    note "installed binaries, but daemon setup failed; run '$INSTALL_DIR/bowline daemon install' for details"
  fi
}

download "$RELEASE_BASE/checksums.txt" "$TMPDIR/checksums.txt"

if [ "$PLATFORM" = "macos" ] && [ "$CLI_ONLY" = "0" ]; then
  install_macos_app
else
  install_cli_archive
fi

install_daemon

if [ "$PLATFORM" = "macos" ] && [ "$CLI_ONLY" = "0" ]; then
  open "$APP_DIR/Bowline.app" >/dev/null 2>&1 || true
fi

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    note "add $INSTALL_DIR to PATH, then restart your shell"
    ;;
esac

echo
echo "Bowline installed."
echo "Next: bowline login"
