#!/usr/bin/env bash
# init.sh — bootstrap + verify. Run at the start of every session.
# Fails fast: any step that fails stops the script with a clear message.
set -euo pipefail

cd "$(dirname "$0")"

echo "==> [1/3] Python toolchain (uv)"
if ! command -v uv >/dev/null 2>&1; then
  echo "ERROR: 'uv' not found. Install: https://docs.astral.sh/uv/  (or 'pip install uv')" >&2
  exit 1
fi

echo "==> [2/3] Sync deps (core + dev test tools; no optional pwn deps)"
uv sync --extra dev --quiet

# zbar dylib path for pyzbar (QR) on macOS; harmless elsewhere.
export DYLD_LIBRARY_PATH="${DYLD_LIBRARY_PATH:-}:/opt/homebrew/lib:/usr/local/lib"

PYTEST_ARGS=(-q)

# The pwn SDK is optional and depends on pwntools / the muteki-pwn container.
# Keep the default session bootstrap lean: pwn-specific tests only run when the
# operator explicitly opts in after installing those tools.
if [[ "${MUTEKI_RUN_PWN_TESTS:-0}" != "1" ]]; then
  PYTEST_ARGS+=(--ignore=tests/test_kit_pwn.py)
fi

echo "==> [3/3] Fast test suite (unit + scripted-loop; pwn optional; live tests skip without API key)"
uv run pytest "${PYTEST_ARGS[@]}"
echo
echo "OK — suite green. See README.md to get started; AGENTS.md for the dev map."

# Optional pwn SDK verification:
#   MUTEKI_RUN_PWN_TESTS=1 ./init.sh
# Requires pwntools (and dynamic tests may require the muteki-pwn image).
#
# To run a real challenge (needs an API key), use the web deck:
#   ./run.sh web   → create a run, flip the offline toggle for a clean black-box.
# A solve is real only when the flag appears in actual worker output (the gate).
