#!/usr/bin/env bash
# Downloads static ffmpeg + ffprobe binaries and installs them as Tauri
# sidecar binaries in src-tauri/binaries/, named <name>-<target-triple>
# as required by the `externalBin` entries in tauri.conf.json.
#
# Usage:
#   ./scripts/fetch-ffmpeg.sh            # fetch for the current host
#   ./scripts/fetch-ffmpeg.sh all        # fetch for every supported target
#   ./scripts/fetch-ffmpeg.sh aarch64-apple-darwin x86_64-unknown-linux-gnu ...

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$SCRIPT_DIR/../src-tauri/binaries"
mkdir -p "$BIN_DIR"

FFMPEG_STATIC_BASE="https://github.com/eugeneware/ffmpeg-static/releases/latest/download"
EVERMEET_FFPROBE="https://evermeet.cx/pub/ffprobe/ffprobe-7.1.1.zip"
OSXEXPERTS_FFPROBE_ARM="https://www.osxexperts.net/ffprobe9arm.zip"
BTBN_BASE="https://github.com/BtbN/FFmpeg-Builds/releases/latest/download"

host_triple() {
  local arch os
  arch="$(uname -m)"
  case "$(uname -s)" in
    Darwin) os="apple-darwin" ;;
    Linux) os="unknown-linux-gnu" ;;
    *) echo "unsupported host os" >&2; return 1 ;;
  esac
  [ "$arch" = "amd64" ] && arch="x86_64"
  echo "$arch-$os"
}

fetch() { # url dest
  echo "  downloading $(basename "$2") from $1"
  curl -fSL --retry 3 -o "$2" "$1"
}

install_from_dir() { # src_dir exe_name triple [ext]
  local src="$1" name="$2" triple="$3" ext="${4:-}"
  cp "$src/$name$ext" "$BIN_DIR/$name-$triple$ext"
  chmod +x "$BIN_DIR/$name-$triple$ext"
}

fetch_darwin() { # triple (x86_64|aarch64)
  local triple="$1" arch_tag
  case "$triple" in
    x86_64-apple-darwin) arch_tag="x64" ;;
    aarch64-apple-darwin) arch_tag="arm64" ;;
  esac
  fetch "$FFMPEG_STATIC_BASE/ffmpeg-darwin-$arch_tag" "$BIN_DIR/ffmpeg-$triple"
  chmod +x "$BIN_DIR/ffmpeg-$triple"
  local tmp
  tmp="$(mktemp -d)"
  if [ "$arch_tag" = "x64" ]; then
    fetch "$EVERMEET_FFPROBE" "$tmp/ffprobe.zip"
  else
    fetch "$OSXEXPERTS_FFPROBE_ARM" "$tmp/ffprobe.zip"
  fi
  (cd "$tmp" && unzip -q ffprobe.zip)
  install_from_dir "$tmp" ffprobe "$triple"
  rm -rf "$tmp"
  codesign --force --sign - "$BIN_DIR/ffmpeg-$triple" "$BIN_DIR/ffprobe-$triple" 2>/dev/null || true
}

fetch_linux() { # triple
  local triple="$1" tag
  case "$triple" in
    x86_64-unknown-linux-gnu) tag="linux64" ;;
    aarch64-unknown-linux-gnu) tag="linuxarm64" ;;
  esac
  local tmp
  tmp="$(mktemp -d)"
  fetch "$BTBN_BASE/ffmpeg-n7.1-latest-$tag-gpl-7.1.tar.xz" "$tmp/ffmpeg.tar.xz"
  (cd "$tmp" && tar -xJf ffmpeg.tar.xz)
  install_from_dir "$tmp/$tag-gpl" ffmpeg "$triple"
  install_from_dir "$tmp/$tag-gpl" ffprobe "$triple"
  rm -rf "$tmp"
}

fetch_windows() {
  local triple="x86_64-pc-windows-msvc" tmp
  tmp="$(mktemp -d)"
  fetch "$BTBN_BASE/ffmpeg-n7.1-latest-win64-gpl-7.1.zip" "$tmp/ffmpeg.zip"
  (cd "$tmp" && unzip -q ffmpeg.zip)
  local inner
  inner="$(find "$tmp" -type d -name bin | head -1)"
  install_from_dir "$inner" ffmpeg "$triple" ".exe"
  install_from_dir "$inner" ffprobe "$triple" ".exe"
  rm -rf "$tmp"
}

SUPPORTED="x86_64-apple-darwin aarch64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-pc-windows-msvc"

targets=("$@")
if [ ${#targets[@]} -eq 0 ]; then
  targets=("$(host_triple)")
elif [ "${targets[0]}" = "all" ]; then
  targets=($SUPPORTED)
fi

for t in "${targets[@]}"; do
  echo "==> $t"
  case "$t" in
    *-apple-darwin) fetch_darwin "$t" ;;
    *-unknown-linux-gnu) fetch_linux "$t" ;;
    *-pc-windows-msvc) fetch_windows ;;
    *) echo "unknown target: $t (supported: $SUPPORTED)" >&2; exit 1 ;;
  esac
done

echo
ls -lh "$BIN_DIR"
echo "done"
