#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 || ! -f "$1" ]]; then
  echo "usage: docker/web/build.sh Muteki-Web-<version>-linux-<arch>.tar.gz" >&2
  exit 2
fi
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
archive="$(realpath "$1")"
case "$(basename "$archive")" in
  Muteki-Web-*-linux-arm64.tar.gz) platform=linux/arm64 ;;
  Muteki-Web-*-linux-amd64.tar.gz) platform=linux/amd64 ;;
  *) echo "Web image needs a Linux Web release tar" >&2; exit 2 ;;
esac
cp "$archive" "$here/web-release.tar.gz"
trap 'rm "$here/web-release.tar.gz"' EXIT
docker build --pull --platform "$platform" --build-arg RELEASE_ASSET=web-release.tar.gz --load -t muteki-web:latest "$here"
echo "built muteki-web:latest ($platform)"
