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
#   ./run.sh web                              backend (:8000) + production Next UI (:3001)
#   ./run.sh web --backend-only               backend only (:8000)
#   ./run.sh web --port 9000                  override backend port
#   ./run.sh web --ui-port 3002               override UI port
#   ./run.sh web --host 0.0.0.0               bind address (default 127.0.0.1).
#                                             Non-loopback REQUIRES MUTEKI_WEB_PASSWORD
#                                             (the backend refuses to start otherwise).
#
# Auth: set MUTEKI_WEB_PASSWORD to require a login password for the web deck.
# When set, open http://localhost:3001 and enter it. Leave unset only for a
# loopback-only (127.0.0.1) single-operator setup.
#
# Secrets: a repo-root .env is auto-loaded (see .env.example). A shell-exported
# var always wins. --swarm needs MUTEKI_DEEPSEEK_API_KEY.
set -euo pipefail

cd "$(dirname "$0")"

# zbar shared library for pyzbar (QR). macOS finds it via this DYLD path; on Linux
# it loads from the system linker cache (apt: libzbar0). Harmless no-op off macOS.
export DYLD_LIBRARY_PATH="${DYLD_LIBRARY_PATH:-}:/opt/homebrew/lib:/usr/local/lib"
# A non-login shell may not have the uv install dir on PATH yet.
export PATH="$HOME/.local/bin:$PATH"

usage() {
  sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

require_uv() {
  if ! command -v uv >/dev/null 2>&1; then
    echo "==> 'uv' not found — installing from https://astral.sh/uv …" >&2
    if command -v curl >/dev/null 2>&1; then
      curl -LsSf https://astral.sh/uv/install.sh | sh
    elif command -v wget >/dev/null 2>&1; then
      wget -qO- https://astral.sh/uv/install.sh | sh
    else
      echo "ERROR: need 'curl' or 'wget' to install uv. See https://docs.astral.sh/uv/" >&2
      exit 1
    fi
    export PATH="$HOME/.local/bin:$PATH"
  fi
  command -v uv >/dev/null 2>&1 || {
    echo "ERROR: 'uv' still not on PATH after install. Add ~/.local/bin to PATH." >&2; exit 1; }
}

run_tui() {
  require_uv
  echo "==> Launching TUI  (Ctrl+C to quit, Esc to interrupt a run)"
  exec uv run python -m apps.tui "$@"
}

run_web() {
  require_uv
  local backend_only=0 port=8000 host=127.0.0.1 ui_port="${MUTEKI_UI_PORT:-3001}"
  local rebuild_ui="${MUTEKI_UI_REBUILD:-auto}"
  local passthru=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --backend-only) backend_only=1; shift ;;
      --port) port="${2:?--port needs a value}"; shift 2 ;;
      --port=*) port="${1#*=}"; shift ;;
      --ui-port) ui_port="${2:?--ui-port needs a value}"; shift 2 ;;
      --ui-port=*) ui_port="${1#*=}"; shift ;;
      --rebuild-ui) rebuild_ui=1; shift ;;
      --no-rebuild-ui) rebuild_ui=0; shift ;;
      --host) host="${2:?--host needs a value}"; shift 2 ;;
      --host=*) host="${1#*=}"; shift ;;
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
    local backend_host=127.0.0.1
    if [ "$host" = "127.0.0.1" ] || [ "$host" = "localhost" ]; then
      backend_host="$host"
    elif [ "$host" != "0.0.0.0" ] && [ "$host" != "::" ]; then
      backend_host="$host"
    fi
    local backend_url="${MUTEKI_BACKEND:-http://${backend_host}:${port}}"
    local build_id="$ui_dir/.next/BUILD_ID"
    local backend_marker="$ui_dir/.next/MUTEKI_BACKEND"
    local need_build=0
    case "$rebuild_ui" in
      1|true|yes|always) need_build=1 ;;
      0|false|no|never) need_build=0 ;;
      auto|"")
        if [ ! -f "$build_id" ]; then
          need_build=1
        elif [ ! -f "$backend_marker" ] || [ "$(cat "$backend_marker" 2>/dev/null || true)" != "$backend_url" ]; then
          need_build=1
        elif find "$ui_dir/app" "$ui_dir/components" "$ui_dir/lib" \
             "$ui_dir/package.json" "$ui_dir/next.config.mjs" \
             -type f -newer "$build_id" -print -quit 2>/dev/null | grep -q .; then
          need_build=1
        fi
        ;;
      *) echo "ERROR: invalid MUTEKI_UI_REBUILD/--rebuild setting: $rebuild_ui" >&2; exit 1 ;;
    esac
    if [ "$need_build" -eq 1 ]; then
      echo "==> Building production Next UI (MUTEKI_BACKEND=$backend_url)…"
      ( cd "$ui_dir" && MUTEKI_BACKEND="$backend_url" npm run build )
      printf '%s\n' "$backend_url" > "$backend_marker"
    fi
    if [ -f "$ui_dir/.next/standalone/server.js" ]; then
      mkdir -p "$ui_dir/.next/standalone/.next"
      if [ -d "$ui_dir/.next/static" ]; then
        rm -rf "$ui_dir/.next/standalone/.next/static"
        cp -R "$ui_dir/.next/static" "$ui_dir/.next/standalone/.next/static"
      fi
      if [ -d "$ui_dir/public" ]; then
        rm -rf "$ui_dir/.next/standalone/public"
        cp -R "$ui_dir/public" "$ui_dir/.next/standalone/public"
      fi
    fi
    echo "==> Starting production Next UI on http://${host}:${ui_port}"
    echo "    UI proxies /api to $backend_url; browser traffic stays same-origin."
    if [ -f "$ui_dir/.next/standalone/server.js" ]; then
      ( cd "$ui_dir" && MUTEKI_BACKEND="$backend_url" PORT="$ui_port" HOSTNAME="$host" node .next/standalone/server.js ) &
    else
      ( cd "$ui_dir" && MUTEKI_BACKEND="$backend_url" npx next start -p "$ui_port" -H "$host" ) &
    fi
    ui_pid=$!
  fi

  echo "==> Starting FastAPI backend on http://${host}:${port}"
  if [ "$want_ui" -eq 1 ]; then
    echo "    Open the UI at  http://${host}:${ui_port}"
  else
    echo "    Static UI (if built) served at  http://localhost:${port}/"
  fi
  # exec would drop the trap; run in foreground so cleanup fires on Ctrl+C.
  # Export the bind host so create_app can see it (uvicorn's --host is NOT
  # visible to the app) and fail-fast on a non-loopback bind with no password.
  export MUTEKI_WEB_BIND="$host"
  # Linux worker containers reach the host-side reverse control plane through
  # host.docker.internal:host-gateway, which cannot hit a receiver bound only to
  # 127.0.0.1. Docker compose already sets this explicitly; for bare-metal
  # `run.sh web` choose the reachable default unless the operator overrode it.
  if [ -z "${MUTEKI_CONTROL_BIND+x}" ] && [ "$(uname -s 2>/dev/null || true)" = "Linux" ]; then
    export MUTEKI_CONTROL_BIND=0.0.0.0
  fi
  uv run uvicorn apps.web.server:create_app --factory \
      --host "$host" --port "$port" "${passthru[@]+"${passthru[@]}"}"
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
