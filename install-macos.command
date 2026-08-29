#!/bin/zsh
set -euo pipefail

readonly PLUGIN_NAME="srtla-output.plugin"
readonly SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)"
readonly INSTALL_DIR="${OBS_SRTLA_PLUGIN_DIR:-$HOME/Library/Application Support/obs-studio/plugins}"
readonly INSTALL_PATH="$INSTALL_DIR/$PLUGIN_NAME"

TEMP_DIR=""

cleanup() {
  if [[ -n "$TEMP_DIR" && -d "$TEMP_DIR" ]]; then
    rm -rf "$TEMP_DIR" 2>/dev/null || true
  fi
}

on_exit() {
  exit_code=$?
  cleanup
  if [[ -t 0 && -t 1 ]]; then
    if [[ "$exit_code" -eq 0 ]]; then
      print -- "Installation completed. Press Return to close this window."
    else
      print -u2 -- "Installation failed (exit code $exit_code). Press Return to close this window."
    fi
    read -r
  fi
  trap - EXIT
  exit "$exit_code"
}

trap on_exit EXIT
trap 'exit 130' INT TERM

die() {
  print -u2 -- "install-macos: $*"
  exit 1
}

usage() {
  cat <<'EOF'
Usage: install-macos.command [PLUGIN_BUNDLE|RELEASE_ZIP]

Installs srtla-output.plugin into the current user's OBS plugin directory,
removes the macOS quarantine flag, and applies an ad-hoc code signature.

With no argument, the script expects srtla-output.plugin next to itself.
Set OBS_SRTLA_PLUGIN_DIR to use a different OBS plugin directory.
EOF
}

for command_name in ditto xattr codesign mktemp; do
  command -v "$command_name" >/dev/null 2>&1 ||
    die "required command not found: $command_name"
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  die "this installer must be run on macOS"
fi

# The user must approve an unnotarized .command file once before it can run.
# After that approval, remove the flag from the helper itself as well.
xattr -dr com.apple.quarantine "$0" >/dev/null 2>&1 || true

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

source_path="${1:-$SCRIPT_DIR/$PLUGIN_NAME}"

if [[ -f "$source_path" && "$source_path" == *.zip ]]; then
  TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/srtla-output.XXXXXX")"
  ditto -x -k "$source_path" "$TEMP_DIR"
  source_path="$(find "$TEMP_DIR" -type d -name "$PLUGIN_NAME" -print -quit)"
elif [[ -d "$source_path" && "$(basename -- "$source_path")" != "$PLUGIN_NAME" &&
        -d "$source_path/$PLUGIN_NAME" ]]; then
  source_path="$source_path/$PLUGIN_NAME"
fi

[[ -d "$source_path" ]] ||
  die "plugin bundle not found: $source_path"
[[ "$(basename -- "$source_path")" == "$PLUGIN_NAME" ]] ||
  die "expected a $PLUGIN_NAME bundle or a release ZIP"

source_path="$(cd -- "$source_path" && pwd -P)"
mkdir -p "$INSTALL_DIR"

same_bundle=false
if [[ -d "$INSTALL_PATH" ]]; then
  install_real_path="$(cd -- "$INSTALL_PATH" && pwd -P)"
  if [[ "$source_path" == "$install_real_path" ]]; then
    same_bundle=true
  fi
fi

if [[ "$same_bundle" != true ]]; then
  if [[ -e "$INSTALL_PATH" || -L "$INSTALL_PATH" ]]; then
    backup_path="$INSTALL_PATH.backup.$(date +%Y%m%d-%H%M%S)"
    suffix=0
    while [[ -e "$backup_path" || -L "$backup_path" ]]; do
      suffix=$((suffix + 1))
      backup_path="$INSTALL_PATH.backup.$(date +%Y%m%d-%H%M%S)-$suffix"
    done
    mv "$INSTALL_PATH" "$backup_path"
    print -- "Existing plugin moved to: $backup_path"
  fi
  ditto --rsrc --extattr --acl "$source_path" "$INSTALL_PATH"
fi

# An unsigned download may carry this flag on the bundle or one of its files.
# xattr returns non-zero when there is no such attribute; that is harmless.
if ! xattr -dr com.apple.quarantine "$INSTALL_PATH"; then
  print -- "No quarantine attribute was removed; continuing."
fi

codesign --force --deep --sign - "$INSTALL_PATH"
codesign --verify --deep --strict --verbose=2 "$INSTALL_PATH"

print -- "Installed and ad-hoc signed: $INSTALL_PATH"
print -- "Restart OBS before using the plugin."
