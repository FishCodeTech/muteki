#!/usr/bin/env bash
# run.sh — launch a Project Muteki frontend.
#
#   ./run.sh tui [tui-args...]      Textual TUI command deck (in-process).
#   ./run.sh web [web-opts...]      Web command deck (FastAPI backend + Next UI).
#
# TUI examples:
#   ./run.sh tui                              mock event stream (UI demo, no key)
#   ./run.sh tui --swarm --key 2020f-cry-hybrid2     solve for real (needs key)
#   ./run.sh tui --swarm --desc "..." --target http://host --category web
#
# Web options:
#   ./run.sh web                              backend (:8000) + Next UI (:3001)
#   ./run.sh web --backend-only               backend only (:8000)
#   ./run.sh web --port 9000                  override backend port
#
# Secrets: a repo-root .env is auto-loaded (see .env.example). A shell-exported
# var always wins. --swarm needs MUTEKI_DEEPSEEK_API_KEY.
set -euo pipefail

cd "$(dirname "$0")"

# zbar dylib path for pyzbar (QR) on macOS; harmless elsewhere.
export DYLD_LIBRARY_PATH="${DYLD_LIBRARY_PATH:-}:/opt/homebrew/lib:/usr/local/lib"

usage() {
  sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

require_uv() {
  command -v uv >/dev/null 2>&1 || {
    echo "ERROR: 'uv' not found. Install: https://docs.astral.sh/uv/" >&2; exit 1; }
}

run_tui() {
  require_uv
  echo "==> Launching TUI  (Ctrl+C to quit, Esc to interrupt a run)"
  exec uv run python -m apps.tui "$@"
}

run_web() {
  require_uv
  local backend_only=0 port=8000
  local passthru=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --backend-only) backend_only=1; shift ;;
      --port) port="${2:?--port needs a value}"; shift 2 ;;
      --port=*) port="${1#*=}"; shift ;;
      *) passthru+=("$1"); shift ;;
    esac
  done

  local ui_dir="apps/web/ui"
  local want_ui=1
  if [ "$backend_only" -eq 1 ]; then want_ui=0; fi
  if [ ! -f "$ui_dir/package.json" ]; then want_ui=0; fi
  command -v npm >/dev/null 2>&1 || { [ "$want_ui" -eq 1 ] && \
    echo "(note) npm not found — starting backend only; install Node to run the Next UI."; want_ui=0; }

  local ui_pid=""
  cleanup() {
    [ -n "${ui_pid:-}" ] && kill "$ui_pid" 2>/dev/null || true
  }
  trap cleanup EXIT INT TERM

  if [ "$want_ui" -eq 1 ]; then
    if [ ! -d "$ui_dir/node_modules" ]; then
      echo "==> First run: installing Next UI deps (npm install in $ui_dir)…"
      ( cd "$ui_dir" && npm install )
    fi
    echo "==> Starting Next UI on http://localhost:3001"
    # Point the browser's EventSource straight at the backend: the Next dev proxy
    # BUFFERS SSE (a live run looks frozen until it ends). CORS on the backend
    # allows this; prod serves the static UI same-origin so this is dev-only.
    ( cd "$ui_dir" && NEXT_PUBLIC_MUTEKI_API="http://127.0.0.1:${port}" npm run dev ) &
    ui_pid=$!
  fi

  echo "==> Starting FastAPI backend on http://localhost:${port}"
  if [ "$want_ui" -eq 1 ]; then
    echo "    Open the UI at  http://localhost:3001  (it talks to the backend)."
  else
    echo "    Static UI (if built) served at  http://localhost:${port}/"
  fi
  # exec would drop the trap; run in foreground so cleanup fires on Ctrl+C.
  uv run uvicorn apps.web.server:create_app --factory \
      --host 127.0.0.1 --port "$port" "${passthru[@]+"${passthru[@]}"}"
}

main() {
  [ $# -ge 1 ] || usage 1
  local mode="$1"; shift || true
  case "$mode" in
    tui) run_tui "$@" ;;
    web) run_web "$@" ;;
    -h|--help|help) usage 0 ;;
    *) echo "ERROR: unknown mode '$mode' (expected: tui | web)" >&2; usage 1 ;;
  esac
}

main "$@"
