#!/bin/sh
set -eu

RELEASE_HOST="${BOWLINE_RELEASE_HOST:-https://install.bowline.sh}"
VERSION="latest"
CLI_ONLY="0"
INSTALL_DIR="${BOWLINE_INSTALL_DIR:-$HOME/.local/bin}"
APP_DIR="${BOWLINE_APP_DIR:-$HOME/Applications}"
RELEASE_SIGNING_IDENTITY="bowline-release"
RELEASE_SIGNING_NAMESPACE="bowline-release"
# Pinned release key; scripts/check-install-script.mjs enforces pubkey parity.
RELEASE_SIGNING_PUBKEY="ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF4Nfjn9iT+NwvF2JpRj9GQAwkjv0Cpp16LXmA+AzBwP bowline-release-2026-07-23"
RELEASE_MANIFEST=""

usage() {
  cat <<'EOF'
Usage: install.sh [--cli-only] [--version <version>]

Installs Bowline for the current user.

Options:
  --cli-only          Install only bowline and bowline-daemon.
  --version VERSION   Install a specific release version, for example 0.1.3.
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
  Linux:aarch64 | Linux:arm64)
    PLATFORM="linux"
    TARGET="aarch64-unknown-linux-gnu"
    ;;
  Darwin:*)
    fail "Bowline on macOS requires Apple silicon; $UNAME_M is not built. See $RELEASE_HOST"
    ;;
  *)
    fail "unsupported platform $UNAME_S/$UNAME_M; see $RELEASE_HOST"
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

verify_signature() {
  file="$1"
  sig="$2"
  need ssh-keygen
  allowed_signers="$TMPDIR/allowed-signers"
  printf "%s %s\n" "$RELEASE_SIGNING_IDENTITY" "$RELEASE_SIGNING_PUBKEY" >"$allowed_signers"
  if ! ssh-keygen -Y verify -f "$allowed_signers" -I "$RELEASE_SIGNING_IDENTITY" -n "$RELEASE_SIGNING_NAMESPACE" -s "$sig" <"$file" >/dev/null 2>&1; then
    fail "signature verification failed for $(basename "$file")"
  fi
}

download_verified_manifest() {
  manifest_url="$1"
  manifest="$2"
  download "$manifest_url" "$manifest"
  download "$manifest_url.sig" "$manifest.sig"
  verify_signature "$manifest" "$manifest.sig"
}

manifest_version() {
  sed -nE 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$1" | awk 'NR == 1 { print }'
}

manifest_artifact_sha() {
  manifest="$1"
  artifact_key="$2"
  awk -v key="\"$artifact_key\"" '
    $0 ~ key { in_artifact = 1; next }
    in_artifact && /"sha256"[[:space:]]*:/ {
      value = $0
      sub(/.*"sha256"[[:space:]]*:[[:space:]]*"/, "", value)
      sub(/".*/, "", value)
      print value
      exit
    }
  ' "$manifest"
}

version_without_prefix() {
  printf "%s" "$1" | sed 's/^v//'
}

validate_manifest_version() {
  resolved_version="$1"
  [ -n "$resolved_version" ] || fail "release manifest is missing version"
  echo "$resolved_version" | grep -Eq '^v?[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$' ||
    fail "release manifest version is invalid: $resolved_version"
}

verify_requested_version() {
  requested="$1"
  resolved="$2"
  [ "$(version_without_prefix "$requested")" = "$(version_without_prefix "$resolved")" ] ||
    fail "release manifest version $resolved does not match requested $requested"
}

resolve_release_base() {
  case "$VERSION" in
    latest)
      manifest="$TMPDIR/release-manifest.json"
      download_verified_manifest "$RELEASE_HOST/release-manifest.json" "$manifest"
      RELEASE_MANIFEST="$manifest"
      resolved_version="$(manifest_version "$manifest")"
      validate_manifest_version "$resolved_version"
      case "$resolved_version" in
        v*) RELEASE_BASE="$RELEASE_HOST/releases/$resolved_version" ;;
        *) RELEASE_BASE="$RELEASE_HOST/releases/v$resolved_version" ;;
      esac
      ;;
    v*)
      RELEASE_BASE="$RELEASE_HOST/releases/$VERSION"
      manifest="$TMPDIR/release-manifest.json"
      download_verified_manifest "$RELEASE_BASE/release-manifest.json" "$manifest"
      RELEASE_MANIFEST="$manifest"
      resolved_version="$(manifest_version "$manifest")"
      validate_manifest_version "$resolved_version"
      verify_requested_version "$VERSION" "$resolved_version"
      ;;
    *)
      RELEASE_BASE="$RELEASE_HOST/releases/v$VERSION"
      manifest="$TMPDIR/release-manifest.json"
      download_verified_manifest "$RELEASE_BASE/release-manifest.json" "$manifest"
      RELEASE_MANIFEST="$manifest"
      resolved_version="$(manifest_version "$manifest")"
      validate_manifest_version "$resolved_version"
      verify_requested_version "$VERSION" "$resolved_version"
      ;;
  esac
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

verify_manifest_bound_file() {
  artifact_key="$1"
  file="$2"
  expected="$(manifest_artifact_sha "$RELEASE_MANIFEST" "$artifact_key")"
  [ -n "$expected" ] || fail "release manifest missing artifact hash for $artifact_key"
  actual="$(sha256 "$file")"
  [ "$actual" = "$expected" ] || fail "release manifest hash mismatch for $(basename "$file")"
}

install_cli_archive() {
  archive="$TMPDIR/bowline-$TARGET.tar.gz"
  need tar
  download "$RELEASE_BASE/bowline-$TARGET.tar.gz" "$archive"
  verify_checksum "$archive"
  mkdir -p "$TMPDIR/cli" "$INSTALL_DIR"
  tar -xzf "$archive" -C "$TMPDIR/cli"
  install -m 0755 "$TMPDIR/cli/bowline" "$INSTALL_DIR/bowline"
  install -m 0755 "$TMPDIR/cli/bowline-daemon" "$INSTALL_DIR/bowline-daemon"
}

# The replaced app bundle keeps running from the bytes we are about to delete,
# so quit it before the swap and relaunch after; otherwise the old menu bar app
# and daemon outlive the new CLI they must agree with.
quit_macos_app() {
  [ -d "$APP_DIR/Bowline.app" ] || return 0
  command -v osascript >/dev/null 2>&1 || return 0
  osascript -e 'quit app "Bowline"' >/dev/null 2>&1 || true
  attempt=0
  while [ "$attempt" -lt 20 ]; do
    pgrep -f "$APP_DIR/Bowline.app" >/dev/null 2>&1 || return 0
    sleep 0.25
    attempt=$((attempt + 1))
  done
  note "Bowline.app did not quit in time; restart it manually after the upgrade"
}

# An upgrade replaces bowline-daemon on disk while the old process keeps
# serving. Restarting the managed service is not service installation: the
# service is only ever created by authenticated setup.
restart_installed_daemon() {
  [ -x "$INSTALL_DIR/bowline" ] || return 0
  if "$INSTALL_DIR/bowline" daemon restart >/dev/null 2>&1; then
    note "restarted the bowline daemon on the newly installed build"
  fi
}

install_macos_app() {
  app_zip="$TMPDIR/Bowline-$TARGET.app.zip"
  need ditto
  download "$RELEASE_BASE/Bowline-$TARGET.app.zip" "$app_zip"
  verify_checksum "$app_zip"
  mkdir -p "$APP_DIR" "$INSTALL_DIR"
  quit_macos_app
  rm -rf "$APP_DIR/Bowline.app"
  ditto -x -k "$app_zip" "$APP_DIR"
  [ -x "$APP_DIR/Bowline.app/Contents/Resources/bin/bowline" ] ||
    fail "downloaded app is missing bundled bowline"
  ln -sf "$APP_DIR/Bowline.app/Contents/Resources/bin/bowline" "$INSTALL_DIR/bowline"
  ln -sf "$APP_DIR/Bowline.app/Contents/Resources/bin/bowline-daemon" "$INSTALL_DIR/bowline-daemon"
}

resolve_release_base
download "$RELEASE_BASE/checksums.txt" "$TMPDIR/checksums.txt"
download "$RELEASE_BASE/checksums.txt.sig" "$TMPDIR/checksums.txt.sig"
verify_manifest_bound_file checksums "$TMPDIR/checksums.txt"
verify_manifest_bound_file checksums_sig "$TMPDIR/checksums.txt.sig"
verify_signature "$TMPDIR/checksums.txt" "$TMPDIR/checksums.txt.sig"

if [ "$PLATFORM" = "macos" ] && [ "$CLI_ONLY" = "0" ]; then
  install_macos_app
else
  install_cli_archive
fi

restart_installed_daemon

if [ "$PLATFORM" = "macos" ] && [ "$CLI_ONLY" = "0" ]; then
  open "$APP_DIR/Bowline.app" >/dev/null 2>&1 || true
fi

shell_rc_path() {
  case "$(basename "${SHELL:-/bin/sh}")" in
    zsh) printf "%s/.zshrc" "${ZDOTDIR:-$HOME}" ;;
    bash) printf "%s/.bashrc" "$HOME" ;;
    fish) printf "%s/.config/fish/config.fish" "$HOME" ;;
    *) printf "" ;;
  esac
}

# $PATH must reach the rc file unexpanded; it is expanded by the shell that
# later sources it, not by this installer.
# shellcheck disable=SC2016
shell_rc_path_line() {
  case "$(basename "${SHELL:-/bin/sh}")" in
    fish) printf 'fish_add_path %s' "$1" ;;
    *) printf 'export PATH="%s:$PATH"' "$1" ;;
  esac
}

# Editing a shell rc is the one thing that makes the printed next command run,
# so do it for the user instead of asking them to.
persist_install_dir_on_path() {
  rc="$(shell_rc_path)"
  [ -n "$rc" ] || return 1
  line="$(shell_rc_path_line "$INSTALL_DIR")"
  if [ -f "$rc" ] && grep -Fqs "$line" "$rc"; then
    return 0
  fi
  mkdir -p "$(dirname "$rc")" || return 1
  {
    printf '\n# added by bowline install\n'
    printf '%s\n' "$line"
  } >>"$rc" || return 1
  note "added $INSTALL_DIR to PATH in $rc"
  return 0
}

NEXT_COMMAND="bowline setup --root ~/Code"
NEXT_HINT=""
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    NEXT_COMMAND="$INSTALL_DIR/bowline setup --root ~/Code"
    if persist_install_dir_on_path; then
      NEXT_HINT="New shells pick up $INSTALL_DIR automatically."
    else
      NEXT_HINT="Add $INSTALL_DIR to PATH to use the short 'bowline' command."
    fi
    ;;
esac

echo
echo "Bowline installed."
echo "Next: $NEXT_COMMAND"
[ -z "$NEXT_HINT" ] || echo "$NEXT_HINT"
