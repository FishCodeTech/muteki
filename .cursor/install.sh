#!/usr/bin/env bash
# Cloud Agent install phase for Project Muteki.
#
# Idempotent dependency refresh run after the repository is checked out. It must
# terminate: no dev servers, tests, or long-running processes belong here (the
# web deck is launched from the `terminals` entry in .cursor/environment.json).
set -euo pipefail

cd "$(dirname "$0")/.."

# ── System dependency: libzbar (pyzbar QR decoding) ──────────────────────────
# Only a QR challenge needs it; the default test path never imports pyzbar, so a
# failure here is a warning, not a hard stop (mirrors init.sh's behaviour).
if ! ldconfig -p 2>/dev/null | grep -qi 'libzbar'; then
  if command -v sudo >/dev/null 2>&1; then
    sudo apt-get update -qq \
      && sudo apt-get install -y -qq libzbar0 \
      || echo "(warn) could not install libzbar0 — QR-decode helpers will be unavailable." >&2
  else
    echo "(warn) no sudo — skipping libzbar0; QR-decode helpers will be unavailable." >&2
  fi
fi

# ── Python toolchain (uv) ────────────────────────────────────────────────────
# uv provisions the pinned Python (>=3.13 per pyproject.toml) itself.
export PATH="$HOME/.local/bin:$PATH"
if ! command -v uv >/dev/null 2>&1; then
  echo "==> installing uv from https://astral.sh/uv …"
  curl -LsSf https://astral.sh/uv/install.sh | sh
  export PATH="$HOME/.local/bin:$PATH"
fi

# ── Python dependencies (core + dev test tools) ──────────────────────────────
uv sync --extra dev

# ── Next.js command-deck dependencies ────────────────────────────────────────
# Only the local web UI needs Node; skip cleanly if npm is unavailable.
if [ -f apps/web/ui/package.json ] && command -v npm >/dev/null 2>&1; then
  ( cd apps/web/ui && npm install )
fi

echo "OK — Muteki install complete."
