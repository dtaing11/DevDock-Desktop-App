#!/bin/sh
# Converts a PNG into a .icns using macOS's iconutil (sips for resizing).
# Usage: make-icns.sh input.png output.icns
set -e
IN="$1"; OUT="$2"
TMP=$(mktemp -d)/DevDock.iconset
mkdir -p "$TMP"
for size in 16 32 64 128 256; do
  sips -z $size $size "$IN" --out "$TMP/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  if [ $double -le 512 ]; then
    sips -z $double $double "$IN" --out "$TMP/icon_${size}x${size}@2x.png" >/dev/null
  fi
done
iconutil -c icns "$TMP" -o "$OUT"
rm -rf "$(dirname "$TMP")"
