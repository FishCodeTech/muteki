#!/usr/bin/env bash
# Optional native toolbox for Ubuntu 24.04 local Workers. The full container
# image remains the complete CTF toolchain; this installs what Ubuntu supplies.
set -euo pipefail

source_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tool_root="${MUTEKI_LOCAL_WORKER_ROOT:-$source_root}"
mkdir -p "$tool_root"
tool_root="$(cd "$tool_root" && pwd)"
if [ "$tool_root" != "$source_root" ]; then
  cp "$source_root/env.sh" "$source_root/tool-list.txt" \
    "$source_root/python-requirements.txt" "$tool_root/"
fi
# shellcheck disable=SC1091
source /etc/os-release
[ "${ID:-}" = ubuntu ] && [ "${VERSION_ID:-}" = 24.04 ] || {
  echo 'ERROR: only Ubuntu 24.04 is supported' >&2; exit 1;
}
[ "$(id -u)" -ne 0 ] || { echo 'ERROR: run as the operator' >&2; exit 1; }
command -v uv >/dev/null 2>&1 || { echo 'ERROR: uv is required' >&2; exit 1; }

echo '==> Installing native CTF core tools'
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
  file jq ripgrep tmux nmap socat gdb binutils python3-venv python3-pip \
  netcat-openbsd openssl xxd zip unzip

# Optional packages change with Ubuntu mirrors and architecture. Report what
# was actually installed; do not claim full-image parity after an apt miss.
optional_packages=(
  masscan ffuf gobuster nikto sqlmap hydra john hashcat proxychains4
  rlwrap sshpass openvpn radare2 patchelf qemu-user-static upx-ucl
  binwalk foremost libimage-exiftool-perl sleuthkit poppler-utils
  squashfs-tools tesseract-ocr tshark jadx apktool libzbar0t64
)
for package in "${optional_packages[@]}"; do
  if ! apt-cache show "$package" >/dev/null 2>&1; then
    echo "optional Ubuntu package unavailable: $package"
    continue
  fi
  if ! sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$package"; then
    echo "optional Ubuntu package failed: $package"
  fi
done

mkdir -p "$tool_root/bin" "$tool_root/python" "$tool_root/ruby"
uv venv --python 3.13 "$tool_root/python"
if ! uv pip install --python "$tool_root/python/bin/python" -r "$tool_root/python-requirements.txt"; then
  echo 'optional Python set incomplete; installing core analysis modules'
  uv pip install --python "$tool_root/python/bin/python" \
    requests beautifulsoup4 lxml pycryptodome pwntools scapy
fi

export PATH="$tool_root/python/bin:$tool_root/ruby/bin:$PATH"
while IFS= read -r name; do
  [ -n "$name" ] || continue
  path="$(command -v "$name" 2>/dev/null || true)"
  if [ -n "$path" ] && [ "$path" != "$tool_root/bin/$name" ]; then
    ln -sfn "$path" "$tool_root/bin/$name"
  fi
done < "$tool_root/tool-list.txt"

touch "$tool_root/.ready"
echo '==> Native CTF tools prepared. Muteki will project only paths that exist.'
