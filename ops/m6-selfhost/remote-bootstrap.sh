#!/usr/bin/env bash
set -euo pipefail

root=${1:-/home/snowywar/muteki-m6-20260823}
secret_dir="$root/secrets"

umask 077
mkdir -p "$secret_dir" "$root/ctfd/data/mysql" "$root/ctfd/data/redis"
mkdir -p "$root/ctfd/data/uploads" "$root/ctfd/data/logs"
mkdir -p "$root/rctf/data/postgres" "$root/rctf/data/redis"
mkdir -p "$root/rctf/data/uploads" "$root/rctf/rctf.d"
mkdir -p "$root/gzctf/data/postgres" "$root/gzctf/data/files"

ensure_hex_secret() {
  path=$1
  bytes=$2
  if [[ ! -s "$path" ]]; then
    openssl rand -hex "$bytes" >"$path"
    chmod 600 "$path"
  fi
}

ensure_base64_secret() {
  path=$1
  bytes=$2
  if [[ ! -s "$path" ]]; then
    openssl rand -base64 "$bytes" | tr -d '\n' >"$path"
    chmod 600 "$path"
  fi
}

ensure_hex_secret "$secret_dir/ctfd-db-root-password" 24
ensure_hex_secret "$secret_dir/ctfd-db-password" 24
ensure_hex_secret "$secret_dir/ctfd-secret-key" 48
ensure_hex_secret "$secret_dir/rctf-db-password" 24
ensure_hex_secret "$secret_dir/rctf-redis-password" 24
ensure_base64_secret "$secret_dir/rctf-token-key" 32
ensure_hex_secret "$secret_dir/gzctf-db-password" 24
ensure_hex_secret "$secret_dir/gzctf-admin-password" 24
ensure_hex_secret "$secret_dir/gzctf-xor-key" 32

CTFD_DB_ROOT_PASSWORD=$(<"$secret_dir/ctfd-db-root-password")
CTFD_DB_PASSWORD=$(<"$secret_dir/ctfd-db-password")
CTFD_SECRET_KEY=$(<"$secret_dir/ctfd-secret-key")
export CTFD_DB_ROOT_PASSWORD CTFD_DB_PASSWORD CTFD_SECRET_KEY

RCTF_DATABASE_PASSWORD=$(<"$secret_dir/rctf-db-password")
RCTF_REDIS_PASSWORD=$(<"$secret_dir/rctf-redis-password")
RCTF_TOKEN_KEY=$(<"$secret_dir/rctf-token-key")
export RCTF_DATABASE_PASSWORD RCTF_REDIS_PASSWORD RCTF_TOKEN_KEY

GZCTF_DATABASE_PASSWORD=$(<"$secret_dir/gzctf-db-password")
GZCTF_ADMIN_PASSWORD=$(<"$secret_dir/gzctf-admin-password")
GZCTF_XOR_KEY=$(<"$secret_dir/gzctf-xor-key")
export GZCTF_DATABASE_PASSWORD GZCTF_ADMIN_PASSWORD GZCTF_XOR_KEY

python3 - "$root" <<'PY'
from __future__ import annotations

import os
from pathlib import Path
import sys

root = Path(sys.argv[1])

def write_private(path: Path, content: str) -> None:
    path.write_text(content, encoding="utf-8")
    path.chmod(0o600)

write_private(
    root / "ctfd" / ".env",
    "\n".join(
        [
            f"CTFD_DB_ROOT_PASSWORD={os.environ['CTFD_DB_ROOT_PASSWORD']}",
            f"CTFD_DB_PASSWORD={os.environ['CTFD_DB_PASSWORD']}",
            f"CTFD_SECRET_KEY={os.environ['CTFD_SECRET_KEY']}",
            "",
        ]
    ),
)

write_private(
    root / "rctf" / ".env",
    "\n".join(
        [
            f"RCTF_DATABASE_PASSWORD={os.environ['RCTF_DATABASE_PASSWORD']}",
            f"RCTF_REDIS_PASSWORD={os.environ['RCTF_REDIS_PASSWORD']}",
            "RCTF_GIT_REF=v2.1.3",
            "",
        ]
    ),
)
rctf_template = (root / "rctf" / "00-m6.yaml.template").read_text(encoding="utf-8")
rctf_config = (
    rctf_template
    .replace("__RCTF_TOKEN_KEY__", os.environ["RCTF_TOKEN_KEY"])
    .replace("__RCTF_DATABASE_PASSWORD__", os.environ["RCTF_DATABASE_PASSWORD"])
    .replace("__RCTF_REDIS_PASSWORD__", os.environ["RCTF_REDIS_PASSWORD"])
)
rctf_config_path = root / "rctf" / "rctf.d" / "00-m6.yaml"
write_private(rctf_config_path, rctf_config)
rctf_config_path.chmod(0o644)

write_private(
    root / "gzctf" / ".env",
    "\n".join(
        [
            f"GZCTF_DATABASE_PASSWORD={os.environ['GZCTF_DATABASE_PASSWORD']}",
            f"GZCTF_ADMIN_PASSWORD={os.environ['GZCTF_ADMIN_PASSWORD']}",
            "",
        ]
    ),
)
gzctf_template = (root / "gzctf" / "appsettings.json.template").read_text(encoding="utf-8")
gzctf_config = (
    gzctf_template
    .replace("__GZCTF_DATABASE_PASSWORD__", os.environ["GZCTF_DATABASE_PASSWORD"])
    .replace("__GZCTF_XOR_KEY__", os.environ["GZCTF_XOR_KEY"])
)
gzctf_config_path = root / "gzctf" / "appsettings.json"
write_private(gzctf_config_path, gzctf_config)
gzctf_config_path.chmod(0o644)
PY

chmod_if_owned() {
  mode=$1
  shift
  for path in "$@"; do
    if [[ -O "$path" ]]; then
      chmod "$mode" "$path"
    fi
  done
}

chmod_if_owned 700 "$secret_dir" "$root/ctfd" "$root/rctf" "$root/gzctf"
chmod_if_owned 755 "$root/rctf/rctf.d"
chmod_if_owned 777 "$root/ctfd/data/uploads" "$root/ctfd/data/logs"
chmod_if_owned 777 "$root/rctf/data/postgres" "$root/rctf/data/uploads"
chmod_if_owned 777 "$root/gzctf/data/postgres" "$root/gzctf/data/files"
