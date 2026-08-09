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
RESOLVED_VERSION=""
SWITCHED="0"
CURRENT_LINK=""
PREVIOUS_CURRENT=""
PREVIOUS_CLI=""
PREVIOUS_DAEMON=""
PREVIOUS_CLI_LINK=""
PREVIOUS_DAEMON_LINK=""
OLD_DAEMON_ACTIVE="0"
OLD_DAEMON_VERSION=""
OLD_APP_STOPPED="0"
ACTIVE_STAGE=""
INSTALL_LOCK_FILE=""
INSTALL_LOCK_HELD="0"
INSTALL_LOCK_KIND=""
COMMAND_MIGRATION_DIR=""

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
  if [ "$SWITCHED" = "1" ]; then
    rollback_switch || note "automatic rollback could not restore the previous installation"
  elif [ "$OLD_APP_STOPPED" = "1" ]; then
    open "$APP_DIR/Bowline.app" >/dev/null 2>&1 ||
      note "automatic recovery could not relaunch the previous Bowline.app"
  fi
  if [ -n "$ACTIVE_STAGE" ]; then
    rm -rf "$ACTIVE_STAGE"
  fi
  if [ "$INSTALL_LOCK_HELD" = "1" ]; then
    if [ "$INSTALL_LOCK_KIND" = "flock" ]; then
      exec 9>&-
    else
      rm -f "$INSTALL_LOCK_FILE" ||
        note "install lock cleanup failed at $INSTALL_LOCK_FILE"
    fi
  fi
  rm -rf "$TMPDIR"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

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
      RESOLVED_VERSION="$(version_without_prefix "$resolved_version")"
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
      RESOLVED_VERSION="$(version_without_prefix "$resolved_version")"
      ;;
    *)
      RELEASE_BASE="$RELEASE_HOST/releases/v$VERSION"
      manifest="$TMPDIR/release-manifest.json"
      download_verified_manifest "$RELEASE_BASE/release-manifest.json" "$manifest"
      RELEASE_MANIFEST="$manifest"
      resolved_version="$(manifest_version "$manifest")"
      validate_manifest_version "$resolved_version"
      verify_requested_version "$VERSION" "$resolved_version"
      RESOLVED_VERSION="$(version_without_prefix "$resolved_version")"
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

# An flock is held by the open file description, not by the process that took
# it, so every descendant that inherits fd 9 keeps the install lock after this
# script exits. Bowline commands can leave a daemon running, and a daemon
# holding fd 9 would fail every later install with "another Bowline installer
# still holds" for as long as it lives — an upgrade path that breaks itself.
# Run Bowline binaries through here so the lock never escapes into a child.
# Closing an unopened descriptor is not an error, so this is also correct on
# the shlock path, which holds the lock as a file rather than a descriptor.
run_bowline() {
  binary="$1"
  shift
  "$binary" "$@" 9>&-
}

# Read one string field from a JSON document on stdin. The CLI reports itself
# as the JSON command contract, so versions are read the same way daemon status
# already was, from one reader rather than a copy per call site.
json_string_field() {
  sed -nE "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"([^\"]+)\".*/\1/p" |
    awk 'NR == 1 { print }'
}

# `bowline version` emits that JSON contract, while `bowline-daemon --version`
# emits clap's plain "<name> <version> (<protocol>)" line. A single parser for
# both took the last whitespace-separated word, which is the whole document for
# the CLI and the protocol suffix for the daemon, so the staged-binary check
# compared a JSON blob against a version string and could never pass. Each
# output shape gets the reader its own contract needs.
cli_reported_version() {
  run_bowline "$1" version 2>/dev/null | json_string_field cliVersion
}

cli_reported_daemon_version() {
  run_bowline "$1" version 2>/dev/null | json_string_field daemonVersion
}

daemon_reported_version() {
  run_bowline "$1" --version 2>/dev/null | awk 'NR == 1 { print $2 }'
}

validate_binaries() {
  bin_dir="$1"
  [ -x "$bin_dir/bowline" ] || fail "staged release is missing executable bowline"
  [ -x "$bin_dir/bowline-daemon" ] ||
    fail "staged release is missing executable bowline-daemon"
  cli_version="$(cli_reported_version "$bin_dir/bowline")"
  daemon_version="$(daemon_reported_version "$bin_dir/bowline-daemon")"
  [ "$cli_version" = "$RESOLVED_VERSION" ] ||
    fail "staged bowline reports $cli_version, expected $RESOLVED_VERSION"
  [ "$daemon_version" = "$RESOLVED_VERSION" ] ||
    fail "staged bowline-daemon reports $daemon_version, expected $RESOLVED_VERSION"
}

atomic_symlink() {
  target="$1"
  link="$2"
  temporary="${link}.new.$$"
  rm -f "$temporary"
  ln -s "$target" "$temporary"
  if ! mv -fT "$temporary" "$link" 2>/dev/null; then
    mv -fh "$temporary" "$link"
  fi
}

acquire_install_lock() {
  INSTALL_LOCK_FILE="$INSTALL_DIR/.bowline-install.lock"
  if command -v shlock >/dev/null 2>&1; then
    attempt=0
    while ! shlock -f "$INSTALL_LOCK_FILE" -p "$$"; do
      [ "$attempt" -lt 240 ] ||
        fail "another Bowline installer still holds $INSTALL_LOCK_FILE"
      sleep 0.25
      attempt=$((attempt + 1))
    done
    INSTALL_LOCK_KIND="shlock"
    INSTALL_LOCK_HELD="1"
    return
  fi
  if command -v flock >/dev/null 2>&1; then
    exec 9>"$INSTALL_LOCK_FILE"
    flock -w 60 9 ||
      fail "another Bowline installer still holds $INSTALL_LOCK_FILE"
    INSTALL_LOCK_KIND="flock"
    INSTALL_LOCK_HELD="1"
    return
  fi
  fail "shlock or flock is required for transactional installation"
}

# A daemon that is briefly slow to answer must not read as an absent daemon.
# One unanswered probe used to retire the restart, so a transiently busy daemon
# left the user installed on disk and still serving the previous version with
# nothing but a console note. Only an answered status decides, and a status that
# never answers is reported rather than assumed to mean nothing is running.
probe_daemon_status() {
  attempts=5
  delay=0.2
  if [ "${BOWLINE_INSTALL_TEST_HOOKS:-0}" = "1" ]; then
    attempts="${BOWLINE_INSTALL_TEST_PROBE_ATTEMPTS:-5}"
    delay="${BOWLINE_INSTALL_TEST_PROBE_DELAY:-0.02}"
  fi
  attempt=0
  while [ "$attempt" -lt "$attempts" ]; do
    if run_bowline "$INSTALL_DIR/bowline" daemon status --json \
      >"$TMPDIR/old-daemon-status.json" 2>/dev/null; then
      return 0
    fi
    sleep "$delay"
    attempt=$((attempt + 1))
  done
  return 1
}

capture_previous_install() {
  if [ -x "$INSTALL_DIR/bowline" ]; then
    PREVIOUS_CLI_LINK="$(readlink "$INSTALL_DIR/bowline" 2>/dev/null || true)"
    if [ -z "$PREVIOUS_CLI_LINK" ]; then
      PREVIOUS_CLI="$TMPDIR/previous-bowline"
      cp -p "$INSTALL_DIR/bowline" "$PREVIOUS_CLI"
    fi
    OLD_DAEMON_VERSION="$(cli_reported_daemon_version "$INSTALL_DIR/bowline")"
    if probe_daemon_status; then
      old_daemon_state="$(daemon_status_state "$TMPDIR/old-daemon-status.json")"
      case "$old_daemon_state" in
        running | starting | stopping | version-skew | unreachable)
          OLD_DAEMON_ACTIVE="1"
          ;;
      esac
    else
      note "daemon status never answered; installing without a daemon restart"
    fi
  fi
  if [ -x "$INSTALL_DIR/bowline-daemon" ]; then
    PREVIOUS_DAEMON_LINK="$(readlink "$INSTALL_DIR/bowline-daemon" 2>/dev/null || true)"
    if [ -z "$PREVIOUS_DAEMON_LINK" ]; then
      PREVIOUS_DAEMON="$TMPDIR/previous-bowline-daemon"
      cp -p "$INSTALL_DIR/bowline-daemon" "$PREVIOUS_DAEMON"
    fi
  fi
}

install_command_links() {
  cli_target="$1"
  daemon_target="$2"
  current_cli_target="$(readlink "$INSTALL_DIR/bowline" 2>/dev/null || true)"
  current_daemon_target="$(readlink "$INSTALL_DIR/bowline-daemon" 2>/dev/null || true)"
  if [ "$current_cli_target" = "$cli_target" ] &&
    [ "$current_daemon_target" = "$daemon_target" ]; then
    return
  fi

  COMMAND_MIGRATION_DIR="$INSTALL_DIR/.bowline-command-migration"
  rm -rf "$COMMAND_MIGRATION_DIR"
  mkdir "$COMMAND_MIGRATION_DIR"
  printf '%s\n' "$cli_target" >"$COMMAND_MIGRATION_DIR/cli-target"
  if [ "${BOWLINE_INSTALL_TEST_HOOKS:-0}:${BOWLINE_INSTALL_TEST_FAIL_AT:-}" = "1:kill-during-command-journal" ]; then
    kill -9 "$$"
  fi
  printf '%s\n' "$daemon_target" >"$COMMAND_MIGRATION_DIR/daemon-target"
  printf '%s\n' "ready-v1" >"$COMMAND_MIGRATION_DIR/ready"
  atomic_symlink "$cli_target" "$INSTALL_DIR/bowline"
  if [ "${BOWLINE_INSTALL_TEST_HOOKS:-0}:${BOWLINE_INSTALL_TEST_FAIL_AT:-}" = "1:between-command-links" ]; then
    fail "test interruption between command links"
  fi
  if [ "${BOWLINE_INSTALL_TEST_HOOKS:-0}:${BOWLINE_INSTALL_TEST_FAIL_AT:-}" = "1:kill-between-command-links" ]; then
    kill -9 "$$"
  fi
  atomic_symlink "$daemon_target" "$INSTALL_DIR/bowline-daemon"
  rm -rf "$COMMAND_MIGRATION_DIR"
  COMMAND_MIGRATION_DIR=""
}

recover_command_link_migration() {
  COMMAND_MIGRATION_DIR="$INSTALL_DIR/.bowline-command-migration"
  [ -d "$COMMAND_MIGRATION_DIR" ] || {
    COMMAND_MIGRATION_DIR=""
    return
  }
  if [ "$(sed -n '1p' "$COMMAND_MIGRATION_DIR/ready" 2>/dev/null || true)" != "ready-v1" ]; then
    rm -rf "$COMMAND_MIGRATION_DIR"
    COMMAND_MIGRATION_DIR=""
    note "discarded an incomplete command-link migration journal"
    return
  fi
  cli_target="$(sed -n '1p' "$COMMAND_MIGRATION_DIR/cli-target")"
  daemon_target="$(sed -n '1p' "$COMMAND_MIGRATION_DIR/daemon-target")"
  if [ -z "$cli_target" ] || [ -z "$daemon_target" ]; then
    fail "interrupted command migration is missing its targets"
  fi
  atomic_symlink "$cli_target" "$INSTALL_DIR/bowline"
  if [ "${BOWLINE_INSTALL_TEST_HOOKS:-0}:${BOWLINE_INSTALL_TEST_FAIL_AT:-}" = "1:between-recovery-command-links" ]; then
    fail "test interruption between recovered command links"
  fi
  atomic_symlink "$daemon_target" "$INSTALL_DIR/bowline-daemon"
  rm -rf "$COMMAND_MIGRATION_DIR"
  COMMAND_MIGRATION_DIR=""
  note "recovered an interrupted command-link migration"
}

daemon_status_version() {
  json_string_field daemonVersion <"$1"
}

daemon_status_state() {
  sed -nE 's/.*"daemon"[[:space:]]*:[[:space:]]*\{[^}]*"state"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$1" |
    awk 'NR == 1 { print }'
}

wait_for_daemon_version() {
  expected="$1"
  observed="unavailable"
  attempts=40
  delay=0.25
  # The test budget bounds a forked stub daemon's startup, not a real service
  # manager's. At 4x0.01s a loaded host missed the window, so a rollback restart
  # reported failure after it had already killed the previous daemon and the
  # suite continued with nothing serving. The ceiling is wall-clock tolerant
  # because the loop still returns on the first healthy probe.
  if [ "${BOWLINE_INSTALL_TEST_HOOKS:-0}" = "1" ]; then
    attempts="${BOWLINE_INSTALL_TEST_HEALTH_ATTEMPTS:-60}"
    delay="${BOWLINE_INSTALL_TEST_HEALTH_DELAY:-0.05}"
  fi
  attempt=0
  while [ "$attempt" -lt "$attempts" ]; do
    if run_bowline "$INSTALL_DIR/bowline" daemon status --json >"$TMPDIR/daemon-status.json" 2>/dev/null; then
      observed="$(daemon_status_version "$TMPDIR/daemon-status.json")"
      if [ "$observed" = "$expected" ]; then
        return 0
      fi
    fi
    sleep "$delay"
    attempt=$((attempt + 1))
  done
  note "daemon health/version handshake expected $expected, observed $observed"
  return 1
}

restart_and_verify_daemon() {
  expected="$1"
  run_bowline "$INSTALL_DIR/bowline" daemon restart >/dev/null 2>&1 || return 1
  wait_for_daemon_version "$expected"
}

rollback_switch() {
  rollback_ok="1"
  if [ -n "$PREVIOUS_CURRENT" ]; then
    atomic_symlink "$PREVIOUS_CURRENT" "$CURRENT_LINK" || rollback_ok="0"
  else
    rm -f "$CURRENT_LINK" || rollback_ok="0"
  fi
  rm -f "$INSTALL_DIR/bowline" || rollback_ok="0"
  rm -f "$INSTALL_DIR/bowline-daemon" || rollback_ok="0"
  if [ -n "$PREVIOUS_CLI_LINK" ]; then
    atomic_symlink "$PREVIOUS_CLI_LINK" "$INSTALL_DIR/bowline" || rollback_ok="0"
  elif [ -n "$PREVIOUS_CLI" ]; then
    cp -p "$PREVIOUS_CLI" "$INSTALL_DIR/bowline" || rollback_ok="0"
  fi
  if [ -n "$PREVIOUS_DAEMON_LINK" ]; then
    atomic_symlink "$PREVIOUS_DAEMON_LINK" "$INSTALL_DIR/bowline-daemon" || rollback_ok="0"
  elif [ -n "$PREVIOUS_DAEMON" ]; then
    cp -p "$PREVIOUS_DAEMON" "$INSTALL_DIR/bowline-daemon" || rollback_ok="0"
  fi
  if [ "$rollback_ok" = "1" ] && [ -n "$COMMAND_MIGRATION_DIR" ]; then
    rm -rf "$COMMAND_MIGRATION_DIR" || rollback_ok="0"
    if [ "$rollback_ok" = "1" ]; then
      COMMAND_MIGRATION_DIR=""
    fi
  fi
  SWITCHED="0"
  if [ "$OLD_DAEMON_ACTIVE" = "1" ]; then
    restart_and_verify_daemon "$OLD_DAEMON_VERSION" || rollback_ok="0"
  fi
  if [ "$OLD_APP_STOPPED" = "1" ]; then
    open "$APP_DIR/Bowline.app" >/dev/null 2>&1 || rollback_ok="0"
  fi
  [ "$rollback_ok" = "1" ]
}

install_cli_archive() {
  archive="$TMPDIR/bowline-$TARGET.tar.gz"
  need tar
  download "$RELEASE_BASE/bowline-$TARGET.tar.gz" "$archive"
  verify_checksum "$archive"

  versions_dir="$INSTALL_DIR/.bowline-versions"
  mkdir -p "$versions_dir" "$INSTALL_DIR"
  stage_dir="$(mktemp -d "$versions_dir/.stage-$RESOLVED_VERSION.XXXXXX")"
  ACTIVE_STAGE="$stage_dir"
  tar -xzf "$archive" -C "$stage_dir"
  validate_binaries "$stage_dir"
  release_name="$RESOLVED_VERSION-$TARGET-$(basename "$stage_dir" | sed 's/^.*\\.//')"
  final_dir="$versions_dir/$release_name"
  mv "$stage_dir" "$final_dir"
  ACTIVE_STAGE=""

  CURRENT_LINK="$INSTALL_DIR/.bowline-current"
  PREVIOUS_CURRENT="$(readlink "$CURRENT_LINK" 2>/dev/null || true)"
  atomic_symlink ".bowline-versions/$release_name" "$CURRENT_LINK"
  SWITCHED="1"
  install_command_links ".bowline-current/bowline" ".bowline-current/bowline-daemon"
}

# The running app must release the old bundle before the current link moves.
# Refusing the update is safer than deleting bytes under an old process.
quit_macos_app() {
  [ -e "$APP_DIR/Bowline.app" ] || return 0
  command -v osascript >/dev/null 2>&1 ||
    fail "cannot verify Bowline.app process state because osascript is unavailable"
  app_running="$(osascript -e 'application "Bowline" is running' 2>/dev/null)" ||
    fail "could not query whether Bowline.app is running"
  [ "$app_running" = "true" ] || return 0
  osascript -e 'quit app "Bowline"' >/dev/null 2>&1 ||
    fail "Bowline.app refused the quit request"
  OLD_APP_STOPPED="1"
  attempt=0
  while [ "$attempt" -lt 20 ]; do
    app_running="$(osascript -e 'application "Bowline" is running' 2>/dev/null)" ||
      fail "could not verify that Bowline.app exited"
    if [ "$app_running" = "false" ]; then
      return 0
    fi
    sleep 0.25
    attempt=$((attempt + 1))
  done
  fail "Bowline.app did not quit in time"
}

validate_macos_app() {
  app="$1"
  bin_dir="$app/Contents/Resources/bin"
  [ -d "$app/Contents" ] || fail "staged app bundle has no Contents directory"
  validate_binaries "$bin_dir"
  need codesign
  codesign --verify --deep --strict "$app" >/dev/null 2>&1 ||
    fail "staged app bundle signature verification failed"
}

install_macos_app() {
  app_zip="$TMPDIR/Bowline-$TARGET.app.zip"
  need ditto
  download "$RELEASE_BASE/Bowline-$TARGET.app.zip" "$app_zip"
  verify_checksum "$app_zip"

  versions_dir="$APP_DIR/.bowline-versions"
  mkdir -p "$versions_dir" "$INSTALL_DIR"
  stage_root="$(mktemp -d "$versions_dir/.stage-$RESOLVED_VERSION.XXXXXX")"
  ACTIVE_STAGE="$stage_root"
  ditto -x -k "$app_zip" "$stage_root"
  staged_app="$stage_root/Bowline.app"
  validate_macos_app "$staged_app"
  release_name="Bowline-$RESOLVED_VERSION-$(basename "$stage_root" | sed 's/^.*\\.//').app"
  final_app="$versions_dir/$release_name"
  mv "$staged_app" "$final_app"
  rmdir "$stage_root"
  ACTIVE_STAGE=""

  quit_macos_app
  CURRENT_LINK="$APP_DIR/Bowline.app"
  if [ -d "$CURRENT_LINK" ] && [ ! -L "$CURRENT_LINK" ]; then
    legacy_app="$versions_dir/Bowline-legacy-$$.app"
    mv "$CURRENT_LINK" "$legacy_app"
    PREVIOUS_CURRENT=".bowline-versions/$(basename "$legacy_app")"
  else
    PREVIOUS_CURRENT="$(readlink "$CURRENT_LINK" 2>/dev/null || true)"
  fi
  atomic_symlink ".bowline-versions/$release_name" "$CURRENT_LINK"
  SWITCHED="1"
  install_command_links \
    "$APP_DIR/Bowline.app/Contents/Resources/bin/bowline" \
    "$APP_DIR/Bowline.app/Contents/Resources/bin/bowline-daemon"
}

resolve_release_base
download "$RELEASE_BASE/checksums.txt" "$TMPDIR/checksums.txt"
download "$RELEASE_BASE/checksums.txt.sig" "$TMPDIR/checksums.txt.sig"
verify_manifest_bound_file checksums "$TMPDIR/checksums.txt"
verify_manifest_bound_file checksums_sig "$TMPDIR/checksums.txt.sig"
verify_signature "$TMPDIR/checksums.txt" "$TMPDIR/checksums.txt.sig"

mkdir -p "$INSTALL_DIR"
acquire_install_lock
recover_command_link_migration
capture_previous_install

if [ "$PLATFORM" = "macos" ] && [ "$CLI_ONLY" = "0" ]; then
  install_macos_app
else
  install_cli_archive
fi

if [ "$OLD_DAEMON_ACTIVE" = "1" ]; then
  if ! restart_and_verify_daemon "$RESOLVED_VERSION"; then
    rollback_switch ||
      fail "new daemon failed health verification and rollback also failed"
    fail "new daemon failed health/version verification; rolled back to $OLD_DAEMON_VERSION"
  fi
  note "installed and healthy: daemon $RESOLVED_VERSION completed its version handshake"
  INSTALL_RESULT="installed-and-healthy"
else
  note "installed on disk: daemon setup or restart is required before serving"
  INSTALL_RESULT="installed-on-disk-restart-required"
fi

if [ "$PLATFORM" = "macos" ] && [ "$CLI_ONLY" = "0" ]; then
  if ! open "$APP_DIR/Bowline.app" >/dev/null 2>&1; then
    rollback_switch ||
      fail "new app failed to relaunch and rollback also failed"
    fail "new app failed to relaunch; rolled back to the previous installation"
  fi
  OLD_APP_STOPPED="0"
fi

SWITCHED="0"
printf 'BOWLINE_INSTALL_RESULT=%s\n' "$INSTALL_RESULT"

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
