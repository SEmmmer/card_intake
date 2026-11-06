#!/bin/sh
# Downloads ExifTool from exiftool.org into apps/desktop/resources/exiftool,
# after checking its SHA-256 against exiftool.org's published checksums.
#
# macOS and Linux use the Perl that ships with the system.
# Usage: scripts/fetch-exiftool.sh [version]   (default: latest)
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
dest="$root/apps/desktop/resources/exiftool"
version=${1:-$(curl -fsSL https://exiftool.org/ver.txt)}
archive="Image-ExifTool-$version.tar.gz"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "Downloading ExifTool $version"
# exiftool.org links its downloads to SourceForge; checksums still come from exiftool.org.
curl -fsSL -o "$work/$archive" "https://sourceforge.net/projects/exiftool/files/$archive/download"
curl -fsSL -o "$work/checksums.txt" "https://exiftool.org/checksums.txt"

expected=$(grep "SHA2-256($archive)" "$work/checksums.txt" | sed 's/.*= *//')
actual=$(shasum -a 256 "$work/$archive" | cut -d' ' -f1)
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
  echo "Checksum mismatch for $archive (expected '$expected', got '$actual')" >&2
  exit 1
fi

tar -xzf "$work/$archive" -C "$work"
rm -rf "$dest"
mkdir -p "$dest"
cp "$work/Image-ExifTool-$version/exiftool" "$dest/exiftool"
cp -R "$work/Image-ExifTool-$version/lib" "$dest/lib"
chmod +x "$dest/exiftool"
echo "$version" > "$dest/VERSION"
echo "Installed ExifTool $("$dest/exiftool" -ver) to $dest"
