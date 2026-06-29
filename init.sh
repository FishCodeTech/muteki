#!/usr/bin/env bash
# init.sh — bootstrap + verify. Run at the start of every session.
# Fails fast: any step that fails stops the script with a clear message.
set -euo pipefail

cd "$(dirname "$0")"

echo "==> [1/3] Python toolchain (uv)"
if ! command -v uv >/dev/null 2>&1; then
  # ~/.local/bin is where the official installer drops uv but a non-login shell
  # often doesn't have it on PATH yet — add it before giving up.
  export PATH="$HOME/.local/bin:$PATH"
fi
if ! command -v uv >/dev/null 2>&1; then
  echo "    'uv' not found — installing from https://astral.sh/uv …"
  if command -v curl >/dev/null 2>&1; then
    curl -LsSf https://astral.sh/uv/install.sh | sh
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- https://astral.sh/uv/install.sh | sh
  else
    echo "ERROR: need 'curl' or 'wget' to install uv. Install uv manually: https://docs.astral.sh/uv/" >&2
    exit 1
  fi
  export PATH="$HOME/.local/bin:$PATH"
  command -v uv >/dev/null 2>&1 || {
    echo "ERROR: uv install did not put 'uv' on PATH. Add ~/.local/bin to PATH and re-run." >&2
    exit 1; }
fi

echo "==> [2/3] Sync deps (core + dev test tools; no optional pwn deps)"
uv sync --extra dev --quiet

# zbar shared library for pyzbar (QR decoding). macOS finds it via DYLD path;
# Linux loads libzbar.so from the system linker cache (apt: libzbar0). The export
# is a harmless no-op off macOS, and we only *warn* on Linux because nothing in the
# default test path imports pyzbar — a QR challenge is the only thing that needs it.
export DYLD_LIBRARY_PATH="${DYLD_LIBRARY_PATH:-}:/opt/homebrew/lib:/usr/local/lib"
if [[ "$(uname -s)" == "Linux" ]] && ! ldconfig -p 2>/dev/null | grep -qi 'libzbar'; then
  echo "    (note) libzbar not found — QR-decode helpers need it. Install with:" >&2
  echo "           sudo apt-get install -y libzbar0    # Debian/Ubuntu" >&2
fi

PYTEST_ARGS=(-q)

# The pwn SDK is optional and depends on pwntools / the muteki-pwn container.
# Keep the default session bootstrap lean: pwn-specific tests only run when the
# operator explicitly opts in after installing those tools.
if [[ "${MUTEKI_RUN_PWN_TESTS:-0}" != "1" ]]; then
  PYTEST_ARGS+=(--ignore=tests/test_kit_pwn.py)
fi

echo "==> [3/3] Fast test suite (unit + scripted-loop; pwn optional; live tests skip without API key)"
# The web entrypoint auto-loads a repo-root .env (dotenv_boot.load_env). If the
# operator has filled MUTEKI_WEB_PASSWORD there, the web app would start with auth
# ON and the unauthenticated web-server tests would all 401. Exporting it EMPTY for
# the test run wins over the file (load_dotenv override=False) and keeps auth off —
# without an empty var, `env -u` would let .env supply the value again. Scoped to
# this command only, so it never leaks into a later `./run.sh web`.
MUTEKI_WEB_PASSWORD= uv run pytest "${PYTEST_ARGS[@]}"
echo
echo "OK — suite green. See README.md to get started; AGENTS.md for the dev map."

# Optional pwn SDK verification:
#   MUTEKI_RUN_PWN_TESTS=1 ./init.sh
# Requires pwntools (and dynamic tests may require the muteki-pwn image).
#
# To run a real challenge (needs an API key), use the web deck:
#   ./run.sh web   → create a run, flip the offline toggle for a clean black-box.
# A solve is real only when the flag appears in actual worker output (the gate).
