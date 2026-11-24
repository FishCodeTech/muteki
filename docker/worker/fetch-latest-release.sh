#!/bin/sh
set -eu

repo="$1"
asset_pattern="$2"
destination="$3"

release="$(curl --fail --location --silent --show-error --retry 8 --retry-all-errors \
  "https://api.github.com/repos/${repo}/releases/latest")"
url="$(printf '%s' "$release" | jq -r --arg pattern "$asset_pattern" \
  '.assets[] | select(.name | test($pattern)) | .browser_download_url' | head -n 1)"
if [ -z "$url" ] || [ "$url" = null ]; then
  echo "No current release asset matched ${repo}: ${asset_pattern}" >&2
  exit 1
fi
curl --fail --location --silent --show-error --retry 8 --retry-all-errors \
  --retry-delay 3 --connect-timeout 30 -o "$destination" "$url"
