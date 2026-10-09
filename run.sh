#!/usr/bin/env bash
# run.sh — launch Project Muteki.
#
#   ./run.sh web [web-opts...]      Web command deck (FastAPI backend + Next UI).
#   ./run.sh upgrade [version]      Check and install a verified release bundle.
#   ./run.sh rollback               Switch back to the previous installed version.
#
# Web options:
#   ./run.sh web                              backend (:8000) + production Next UI (:3001)
#   ./run.sh web --dev                        backend hot reload + Next UI hot reload
#   ./run.sh web --backend-only               backend only (:8000)
#   ./run.sh web --port 9000                  override backend port
#   ./run.sh web --ui-port 3002               override UI port
#   ./run.sh web --sessions-root /tmp/muteki-a/sessions isolate Run workspaces
#   ./run.sh web --state-root /tmp/muteki-a/state       isolate service state
#   ./run.sh web --control-root /tmp/muteki-a/control  override coordinator control state
#   ./run.sh web --control-port 9299           isolate reverse control receiver
#   ./run.sh web --host 0.0.0.0               bind address (default 127.0.0.1).
#                                             Non-loopback requires an access password
#                                             (the backend refuses to start otherwise).
#   ./run.sh web --ui-host 0.0.0.0            expose only the UI; keep the API on --host.
#                                             Use Settings or MUTEKI_WEB_PASSWORD.
#   MUTEKI_UI_DEV_BUNDLER=webpack ./run.sh web --dev
#                                             use the legacy Webpack dev bundler.
#
# Auth: use Settings > Access and sign-in, or set MUTEKI_WEB_PASSWORD.
# When set, open http://localhost:3001 and enter it. Leave unset only for a
# loopback-only (127.0.0.1) single-operator setup.
#
# Secrets: a repo-root .env is auto-loaded (see .env.example). A shell-exported
# var always wins.
set -euo pipefail

cd "$(dirname "$0")"

# All service, model and target traffic uses the host network stack directly.
# V2Ray TUN remains part of macOS routing; these variables only disable explicit
# HTTP/SOCKS proxy inheritance from the launching shell.
unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy
export NO_PROXY='*'
export no_proxy='*'
# The TSEC VPN advertises a DNS server that answers TCP queries while UDP
# queries time out.  Make the host resolver use that working transport so Pi
# can resolve its model endpoint without introducing an HTTP/SOCKS proxy.
export RES_OPTIONS="${RES_OPTIONS:+${RES_OPTIONS} }use-vc"

# A prepared ctf-tools directory supplies the native Worker toolchain.
# Local workers inherit this process environment and host routing.
local_tool_root="${MUTEKI_LOCAL_WORKER_ROOT:-$PWD/ctf-tools}"
if [ -f "$local_tool_root/.ready" ] && [ -f "$local_tool_root/env.sh" ]; then
  # shellcheck source=ctf-tools/env.sh
  source "$local_tool_root/env.sh"
fi
unset local_tool_root

# zbar shared library for pyzbar (QR). macOS finds it via this DYLD path; on Linux
# it loads from the system linker cache (apt: libzbar0). Harmless no-op off macOS.
export DYLD_LIBRARY_PATH="${DYLD_LIBRARY_PATH:-}:/opt/homebrew/lib:/usr/local/lib"
# A non-login shell may not have the uv install dir on PATH yet.
export PATH="$HOME/.local/bin:$PATH"

usage() {
  sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
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

run_web() {
  require_uv
  local backend_only=0 dev_mode=0 port=8000 host=127.0.0.1 ui_host="" ui_port="${MUTEKI_UI_PORT:-3001}"
  local sessions_root="" state_root="" control_root="" control_port=""
  local rebuild_ui="${MUTEKI_UI_REBUILD:-auto}"
  local passthru=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --dev) dev_mode=1; shift ;;
      --backend-only) backend_only=1; shift ;;
      --port) port="${2:?--port needs a value}"; shift 2 ;;
      --port=*) port="${1#*=}"; shift ;;
      --ui-port) ui_port="${2:?--ui-port needs a value}"; shift 2 ;;
      --ui-port=*) ui_port="${1#*=}"; shift ;;
      --rebuild-ui) rebuild_ui=1; shift ;;
      --no-rebuild-ui) rebuild_ui=0; shift ;;
      --host) host="${2:?--host needs a value}"; shift 2 ;;
      --host=*) host="${1#*=}"; shift ;;
      --ui-host) ui_host="${2:?--ui-host needs a value}"; shift 2 ;;
      --ui-host=*) ui_host="${1#*=}"; shift ;;
      --sessions-root) sessions_root="${2:?--sessions-root needs a value}"; shift 2 ;;
      --sessions-root=*) sessions_root="${1#*=}"; shift ;;
      --state-root) state_root="${2:?--state-root needs a value}"; shift 2 ;;
      --state-root=*) state_root="${1#*=}"; shift ;;
      --control-root) control_root="${2:?--control-root needs a value}"; shift 2 ;;
      --control-root=*) control_root="${1#*=}"; shift ;;
      --control-port) control_port="${2:?--control-port needs a value}"; shift 2 ;;
      --control-port=*) control_port="${1#*=}"; shift ;;
      *) passthru+=("$1"); shift ;;
    esac
  done
  ui_host="${ui_host:-$host}"
  export MUTEKI_UI_PORT="$ui_port"
  export MUTEKI_UI_HOST="$ui_host"
  case "$ui_host" in
    127.0.0.1|localhost|::1) ;;
    *)
      if ! MUTEKI_WEB_BIND="$ui_host" uv run --no-sync python -c 'import sys; from muteki.core.dotenv_boot import load_env; load_env(); from apps.web.auth import AuthConfig; AuthConfig.from_env(sys.argv[1]).fail_fast_check()' "${state_root:-${MUTEKI_STATE_ROOT:-state}}"; then
        echo "ERROR: exposing the UI requires an access password from Settings or MUTEKI_WEB_PASSWORD." >&2
        return 1
      fi
      ;;
  esac

  local ui_dir="apps/web/ui"
  local next_dist_dir="${MUTEKI_NEXT_DIST_DIR:-.next}"
  local ui_build_dir="$ui_dir/$next_dist_dir"
  local backend_host=127.0.0.1
  if [ "$host" = "127.0.0.1" ] || [ "$host" = "localhost" ]; then
    backend_host="$host"
  elif [ "$host" != "0.0.0.0" ] && [ "$host" != "::" ]; then
    backend_host="$host"
  fi
  local backend_url="${MUTEKI_BACKEND:-http://${backend_host}:${port}}"
  local want_ui=1
  if [ "$backend_only" -eq 1 ]; then want_ui=0; fi
  if [ ! -f "$ui_dir/package.json" ]; then want_ui=0; fi
  command -v npm >/dev/null 2>&1 || { [ "$want_ui" -eq 1 ] && \
    echo "(note) npm not found — starting backend only; install Node to run the Next UI."; want_ui=0; }

  local ui_pid="" backend_pid=""
  # Durable service logs under state/_logs so a revoked PTY cannot erase the
  # only crash evidence (#216). Prefer MUTEKI_STATE_ROOT when the operator
  # isolated state; fall back to ./state.
  local service_state_root="${state_root:-${MUTEKI_STATE_ROOT:-state}}"
  local service_log_dir="$service_state_root/_logs"
  mkdir -p "$service_log_dir"
  local backend_log="$service_log_dir/backend.log"
  local ui_log="$service_log_dir/ui.log"
  local backend_exit_log="$service_log_dir/backend.exit.log"
  local ui_exit_log="$service_log_dir/ui.exit.log"
  : >>"$backend_log" >>"$ui_log"
  echo "==> Service logs: $service_log_dir (backend.log / ui.log / *.exit.log)"

  # Give each background service its own process group. Next and Uvicorn both
  # spawn children; terminating only their wrapper leaves orphan processes
  # holding the ports and makes a later launch look wedged.
  set -m
  stop_process_group() {
    local pid="${1:-}"
    [ -n "$pid" ] || return 0
    kill -TERM -- "-$pid" 2>/dev/null || true
    # Some macOS shells cannot create the requested child process group.
    # Stop the direct children and wrapper as well in that case.
    local child
    while IFS= read -r child; do
      [ -n "$child" ] && kill -TERM "$child" 2>/dev/null || true
    done < <(pgrep -P "$pid" 2>/dev/null || true)
    kill -TERM "$pid" 2>/dev/null || true
  }
  record_service_exit() {
    # Persist exit code / inferred signal even after the launching PTY is gone.
    local name="$1" pid="$2" status="$3" dest="$4"
    local signal=""
    if [ -n "$status" ] && [ "$status" -gt 128 ] 2>/dev/null; then
      signal=$((status - 128))
    fi
    {
      printf 'service=%s\n' "$name"
      printf 'pid=%s\n' "$pid"
      printf 'status=%s\n' "$status"
      printf 'signal=%s\n' "${signal}"
      printf 'timestamp=%s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')"
      printf '%s\n' '---'
    } >>"$dest" 2>/dev/null || true
  }
  cleanup() {
    trap - EXIT INT TERM
    stop_process_group "${ui_pid:-}"
    stop_process_group "${backend_pid:-}"
    local attempt alive
    for attempt in 1 2 3 4 5 6 7 8 9 10; do
      alive=0
      [ -n "${ui_pid:-}" ] && kill -0 "$ui_pid" 2>/dev/null && alive=1
      [ -n "${backend_pid:-}" ] && kill -0 "$backend_pid" 2>/dev/null && alive=1
      [ "$alive" -eq 0 ] && break
      sleep 0.1
    done
    [ -n "${ui_pid:-}" ] && kill -KILL -- "-${ui_pid}" 2>/dev/null || true
    [ -n "${backend_pid:-}" ] && kill -KILL -- "-${backend_pid}" 2>/dev/null || true
    [ -n "${ui_pid:-}" ] && kill -KILL "$ui_pid" 2>/dev/null || true
    [ -n "${backend_pid:-}" ] && kill -KILL "$backend_pid" 2>/dev/null || true
    [ -n "${ui_pid:-}" ] && wait "$ui_pid" 2>/dev/null || true
    [ -n "${backend_pid:-}" ] && wait "$backend_pid" 2>/dev/null || true
    set +m
  }
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  if [ "$want_ui" -eq 1 ]; then
    # Prepare/build the UI in its own service group so the backend can start
    # immediately, and the supervisor can stop both services if either fails.
    (
      set +m  # Keep build tools and the UI server in this service's process group.
      if [ ! -d "$ui_dir/node_modules" ]; then
        echo "==> First run: installing Next UI deps (npm install in $ui_dir)…"
        ( cd "$ui_dir" && npm install )
      fi
      if [ "$dev_mode" -eq 1 ]; then
        local dev_bundler="${MUTEKI_UI_DEV_BUNDLER:-turbopack}"
        local next_dev_args=()
        case "$dev_bundler" in
          turbopack|turbo|"") next_dev_args+=(--turbopack); dev_bundler=turbopack ;;
          webpack) ;;
          *) echo "ERROR: invalid MUTEKI_UI_DEV_BUNDLER: $dev_bundler (expected turbopack or webpack)" >&2; exit 1 ;;
        esac
        echo "==> Starting hot-reload Next UI on http://${ui_host}:${ui_port}"
        local public_api="$backend_url"
        if [ "$ui_host" != "$host" ]; then public_api=""; fi
        echo "    Bundler: $dev_bundler; UI proxies /api to $backend_url when addresses differ."
        ( cd "$ui_dir" && \
          MUTEKI_BACKEND="$backend_url" \
          NEXT_PUBLIC_MUTEKI_API="${NEXT_PUBLIC_MUTEKI_API:-$public_api}" \
          npx next dev "${next_dev_args[@]}" -p "$ui_port" -H "$ui_host" )
      else
        local build_id="$ui_build_dir/BUILD_ID"
        local backend_marker="$ui_build_dir/MUTEKI_BACKEND"
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
                 "$ui_dir/package.json" "$ui_dir/next.config.mjs" "$ui_dir/middleware.ts" \
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
        if [ -f "$ui_build_dir/standalone/server.js" ]; then
          mkdir -p "$ui_build_dir/standalone/$next_dist_dir"
          if [ -d "$ui_build_dir/static" ]; then
            rm -rf "$ui_build_dir/standalone/$next_dist_dir/static"
            cp -R "$ui_build_dir/static" "$ui_build_dir/standalone/$next_dist_dir/static"
          fi
          if [ -d "$ui_dir/public" ]; then
            rm -rf "$ui_build_dir/standalone/public"
            cp -R "$ui_dir/public" "$ui_build_dir/standalone/public"
          fi
        fi
        echo "==> Starting production Next UI on http://${ui_host}:${ui_port}"
        echo "    UI proxies /api to $backend_url; browser traffic stays same-origin."
        if [ -f "$ui_build_dir/standalone/server.js" ]; then
          ( cd "$ui_dir" && MUTEKI_BACKEND="$backend_url" PORT="$ui_port" HOSTNAME="$ui_host" node "$next_dist_dir/standalone/server.js" )
        else
          ( cd "$ui_dir" && MUTEKI_BACKEND="$backend_url" npx next start -p "$ui_port" -H "$ui_host" )
        fi
      fi
    ) </dev/null >>"$ui_log" 2>&1 &
    ui_pid=$!
    echo "    UI log: $ui_log (pid $ui_pid)"
  fi

  if [ "$dev_mode" -eq 1 ]; then
    echo "==> Starting hot-reload FastAPI backend on http://${host}:${port}"
  else
    echo "==> Starting FastAPI backend on http://${host}:${port}"
  fi
  if [ "$want_ui" -eq 1 ]; then
    echo "    Open the UI at  http://${ui_host}:${ui_port}"
  else
    echo "    Static UI (if built) served at  http://localhost:${port}/"
  fi
  # Keep run.sh as the foreground supervisor so cleanup fires on Ctrl+C.
  # Export the bind host so create_app can see it (uvicorn's --host is NOT
  # visible to the app) and fail-fast on a non-loopback bind with no password.
  export MUTEKI_WEB_BIND="$host"
  export MUTEKI_WEB_PORT="$port"
  export MUTEKI_BACKEND_URL="$backend_url"
  export MUTEKI_CAPABILITY_GATEWAY_ENDPOINT="${MUTEKI_CAPABILITY_GATEWAY_ENDPOINT:-${backend_url}/api/capability}"
  if [ "$dev_mode" -eq 1 ]; then
    # Runtime capability probes can each wait on a CLI startup timeout. Running
    # all of them inside FastAPI's lifespan used to hold every first-page API
    # request for roughly 80-110 seconds. Dev mode restores cached health and
    # leaves fresh probes to the existing background/on-demand refresh paths.
    export MUTEKI_STARTUP_RUNTIME_PROBE="${MUTEKI_STARTUP_RUNTIME_PROBE:-0}"
  fi
  [ -n "$sessions_root" ] && export MUTEKI_SESSIONS_ROOT="$sessions_root"
  [ -n "$state_root" ] && export MUTEKI_STATE_ROOT="$state_root"
  [ -n "$control_root" ] && export MUTEKI_COORDINATOR_CONTROL_ROOT="$control_root"
  [ -n "$control_port" ] && export MUTEKI_CONTROL_PORT="$control_port"
  echo "    Readiness: ${backend_url}/api/readiness"
  echo "    Sessions: ${MUTEKI_SESSIONS_ROOT:-sessions}"
  echo "    State: ${MUTEKI_STATE_ROOT:-state}"
  echo "    Control root: ${MUTEKI_COORDINATOR_CONTROL_ROOT:-<state>/control}"
  echo "    Control port: ${MUTEKI_CONTROL_PORT:-9100}"
  # Linux worker containers reach the host-side reverse control plane through
  # host.docker.internal:host-gateway, which cannot hit a receiver bound only to
  # 127.0.0.1. Docker compose already sets this explicitly; for bare-metal
  # `run.sh web` choose the reachable default unless the operator overrode it.
  if [ -z "${MUTEKI_CONTROL_BIND+x}" ] && [ "$(uname -s 2>/dev/null || true)" = "Linux" ]; then
    export MUTEKI_CONTROL_BIND=0.0.0.0
  fi
  local uvicorn_args=(apps.web.server:create_app --factory --host "$host" --port "$port")
  if [ "$dev_mode" -eq 1 ]; then
    # The repository also contains large, frequently-changing sessions and
    # Next build caches. Watching the entire tree makes WatchFiles process
    # unrelated events indefinitely, so only watch backend source roots.
    uvicorn_args+=(
      --reload
      --reload-dir apps/web
      --reload-dir muteki
      --reload-exclude "$ui_dir"
      # SSE clients stay connected across edits; bound their drain so reload
      # reaches lifespan shutdown instead of leaving a non-serving listener.
      --timeout-graceful-shutdown 5
    )
  fi
  # Detach from the launching PTY so a revoked terminal cannot silently drop
  # the only copy of crash output; durable file under state/_logs keeps it.
  uv run uvicorn "${uvicorn_args[@]}" "${passthru[@]+"${passthru[@]}"}"     </dev/null >>"$backend_log" 2>&1 &
  backend_pid=$!
  echo "    Backend log: $backend_log (pid $backend_pid)"

  # Bash 3.2 (the macOS system Bash) has no `wait -n`, so poll the two job
  # leaders. If either service exits, fail the launch and let cleanup stop its
  # peer; keeping only the UI after a backend SIGTRAP previously spun Next at
  # 100% CPU against a dead upstream (#216).
  while true; do
    if ! kill -0 "$backend_pid" 2>/dev/null; then
      local backend_status=0
      wait "$backend_pid" || backend_status=$?
      record_service_exit backend "$backend_pid" "$backend_status" "$backend_exit_log"
      echo "ERROR: FastAPI backend exited (status $backend_status). See $backend_log and $backend_exit_log" >&2
      # Tear down the UI process group immediately so Next cannot spin forever
      # retrying a dead :8000; cleanup also runs via EXIT trap after return.
      stop_process_group "${ui_pid:-}"
      cleanup
      [ "$backend_status" -ne 0 ] && return "$backend_status"
      return 1
    fi
    if [ -n "$ui_pid" ] && ! kill -0 "$ui_pid" 2>/dev/null; then
      local ui_status=0
      wait "$ui_pid" || ui_status=$?
      record_service_exit ui "$ui_pid" "$ui_status" "$ui_exit_log"
      echo "ERROR: Next UI exited (status $ui_status). See $ui_log and $ui_exit_log" >&2
      stop_process_group "${backend_pid:-}"
      cleanup
      [ "$ui_status" -ne 0 ] && return "$ui_status"
      return 1
    fi
    sleep 1
  done
}

main() {
  [ $# -ge 1 ] || usage 1
  local mode="$1"; shift || true
  case "$mode" in
    web) run_web "$@" ;;
    version|status|upgrade|install|rollback)
      require_uv
      exec uv run python -m muteki.cli "$mode" "$@"
      ;;
    -h|--help|help) usage 0 ;;
    *) echo "ERROR: unknown mode '$mode' (expected: web | version | status | upgrade | install | rollback)" >&2; usage 1 ;;
  esac
}

main "$@"
