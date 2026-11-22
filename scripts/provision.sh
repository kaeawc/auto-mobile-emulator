#!/usr/bin/env bash
# Provision what emu-rs needs to boot an AVD, with no Java, sdkmanager, avdmanager or emulator
# package: a system image and platform-tools (adb), resolved from Google's repository
# manifests, size- and sha1-verified, unpacked into the standard SDK layout. Then create the AVD.
#
#   scripts/provision.sh <api> [tag] [avd-name]      e.g. scripts/provision.sh 36 google_apis small36
#
# Env: ANDROID_HOME (default ~/Library/Android/sdk), LCD (default 720x1280@320), CORES, MEMORY.
# Downloads are cached in $ANDROID_HOME/.downloads; packages already installed are skipped.
set -euo pipefail
api=${1:?usage: scripts/provision.sh <api> [tag] [avd-name]}
tag=${2:-google_apis}
name=${3:-emu-$tag-$api}
abi=arm64-v8a
repo=$(cd "$(dirname "$0")/.." && pwd)
sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}
dl="$sdk/.downloads"; mkdir -p "$dl"

manifest() { # <base-url> <file> -> local path (refreshed daily)
  local out="$dl/$(echo "$1" | tr '/:' '__')$2"
  if [ ! -f "$out" ] || [ -n "$(find "$out" -mtime +1 2>/dev/null)" ]; then curl -sSfL -o "$out" "$1$2"; fi
  echo "$out"
}

install() { # <manifest-base-url> <manifest-file> <package-path> <dest-dir>
  local base=$1 file=$2 pkg=$3 dest=$4
  if [ -f "$sdk/$dest/source.properties" ] || [ -f "$sdk/$dest/package.xml" ]; then echo "have $pkg"; return; fi
  local url size sha rev
  read -r url size sha rev < <(python3 -I "$repo/scripts/sdk-resolve.py" "$(manifest "$base" "$file")" "$base" "$pkg")
  local zip="$dl/$(basename "$url")"
  [ -f "$zip" ] || { curl -sSfL -o "$zip.part" "$url" && mv "$zip.part" "$zip"; }
  [ "$(stat -f %z "$zip")" = "$size" ] || { echo "$zip: size mismatch" >&2; rm -f "$zip"; exit 1; }
  [ "$(shasum -a 1 "$zip" | cut -d' ' -f1)" = "$sha" ] || { echo "$zip: sha1 mismatch" >&2; rm -f "$zip"; exit 1; }
  local tmp; tmp=$(mktemp -d "$dl/x.XXXX")
  unzip -q "$zip" -d "$tmp"
  mkdir -p "$(dirname "$sdk/$dest")"; rm -rf "${sdk:?}/$dest"
  mv "$(find "$tmp" -mindepth 1 -maxdepth 1 | head -1)" "$sdk/$dest"; rmdir "$tmp"
  echo "installed $pkg $rev ($(du -sh "$sdk/$dest" | cut -f1))"
}

repo_url=https://dl.google.com/android/repository/
install "$repo_url" repository2-3.xml "platform-tools" platform-tools
case "$tag" in
  default) img_url=${repo_url}sys-img/android/ ;;
  *) img_url=${repo_url}sys-img/$tag/ ;;
esac
install "$img_url" sys-img2-3.xml "system-images;android-$api;$tag;$abi" "system-images/android-$api/$tag/$abi"

cd "$repo"
cargo build -q --release -p emu
lcd_args=(); [ -n "${LCD:-}" ] && lcd_args=(-lcd "$LCD")
extra=(); [ -n "${CORES:-}" ] && extra+=(-cores "$CORES"); [ -n "${MEMORY:-}" ] && extra+=(-memory "$MEMORY")
ANDROID_HOME="$sdk" target/release/emu -create-avd "$name" -sysdir "system-images/android-$api/$tag/$abi" \
  "${lcd_args[@]+"${lcd_args[@]}"}" "${extra[@]+"${extra[@]}"}"
echo "boot it: cargo run -p emu -- -avd $name"
