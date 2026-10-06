#!/usr/bin/env bash
# Wraps a built codex-gui binary into Codex.app.
#
# Usage: bundle-app.sh <codex-gui binary> <output dir> <version>
#                      [--bundle-id ID] [--helper PATH]...
#
# Each --helper executable (for example codex-code-mode-host) is copied next
# to codex-gui in Contents/MacOS, where the embedded Codex runtime looks for
# it. A fourth positional argument is still accepted as the bundle id.
#
# The bundle is unsigned; sign and notarize it (helpers included) with the
# same identity as the CLI binaries before distributing.
set -euo pipefail

binary="$1"
out_dir="$2"
version="$3"
shift 3
bundle_id="com.openai.codex.gui"
helpers=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --bundle-id)
            bundle_id="$2"
            shift 2
            ;;
        --helper)
            helpers+=("$2")
            shift 2
            ;;
        *)
            bundle_id="$1"
            shift
            ;;
    esac
done

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
icon_png="${here}/../../ui/assets/icon.png"
app="${out_dir}/Codex.app"

rm -rf "$app"
mkdir -p "${app}/Contents/MacOS" "${app}/Contents/Resources"
ditto "$binary" "${app}/Contents/MacOS/codex-gui"
for helper in ${helpers[@]+"${helpers[@]}"}; do
    ditto "$helper" "${app}/Contents/MacOS/$(basename "$helper")"
done
sed -e "s/@VERSION@/${version}/g" -e "s/@BUNDLE_ID@/${bundle_id}/g" \
    "${here}/Info.plist.in" > "${app}/Contents/Info.plist"

iconset="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$icon_png" --out "${iconset}/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" "$icon_png" --out "${iconset}/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "${app}/Contents/Resources/AppIcon.icns"
rm -rf "$(dirname "$iconset")"

plutil -lint "${app}/Contents/Info.plist" >/dev/null
echo "$app"
