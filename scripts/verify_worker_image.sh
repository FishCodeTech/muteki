#!/usr/bin/env bash
# Verify a Muteki Worker image (full or slim) against the eight container engines
# and core tools. Optional tools are probed and reported, but missing optionals
# do not fail the check.
#
# Usage:
#   ./scripts/verify_worker_image.sh --image <ref> --variant slim|full [--platform linux/amd64|linux/arm64]
#   ./scripts/verify_worker_image.sh --in-container --variant slim|full
#
# Exit 0 only when all eight engines start (--version) and every required core
# tool is present and runnable. Prints a version inventory to stdout.
set -euo pipefail

IMAGE=""
VARIANT=""
PLATFORM=""
IN_CONTAINER=0
VERSIONS_FILE=""

usage() {
  sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --image) IMAGE="${2:-}"; shift 2 ;;
    --variant) VARIANT="${2:-}"; shift 2 ;;
    --platform) PLATFORM="${2:-}"; shift 2 ;;
    --in-container) IN_CONTAINER=1; shift ;;
    --versions-file) VERSIONS_FILE="${2:-}"; shift 2 ;;
    -h|--help)
      sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unknown arg: $1" >&2; usage ;;
  esac
done

case "${VARIANT}" in
  slim|full) ;;
  *) echo "--variant must be slim or full" >&2; exit 2 ;;
esac

# Eight container engines — matches muteki/solver/container_exec._CONTAINER_BIN.
# Devin is host-local only; DeepSeek Harness (dsh) is temporarily disabled.
ENGINE_CHECKS=(
  "claude:claude"
  "codex:codex"
  "cursor:/home/kali/.local/bin/cursor-agent"
  "pi:pi"
  "omp:/home/kali/.local/bin/omp"
  "kimi:kimi"
  "grok:/home/kali/.grok/bin/grok"
  "opencode:opencode"
  "droid:droid"
)

CORE_BOTH=(
  "/opt/muteki/runtime_agent"
  "/usr/local/bin/blackboard.py"
  "/opt/muteki/offline_acp_bridge.py"
  "/opt/muteki/omp_offline_config.yml"
  "/opt/muteki/kimi_offline_agent.md"
  "/opt/muteki/grok_offline_agent.md"
  "/opt/muteki/AGENTS.md"
  "/opt/muteki/CLAUDE.md"
)

CORE_CMDS_BOTH=(python3 curl git jq rg)

# Full-image core CTF toolchain. These are hard requirements of docker/worker/Dockerfile.
# Arch-specific or best-effort packages belong in OPTIONAL_* below.
CORE_CMDS_FULL=(
  nmap masscan ffuf gobuster sqlmap nikto whatweb
  tshark binwalk foremost exiftool steghide
  hydra john hashcat socat ncat proxychains4 chisel
  ssh sshpass openvpn nuclei gh jwt_tool
  php_filter_chain_generator cloudfox cdk
  forge cast anvil ilspycmd stegsolve
  sage vol radare2 gdb gdb-multiarch objdump strings
)

CORE_FILES_FULL=(
  /opt/gef/gef.py
  /opt/jwt_tool/jwt_tool.py
  /opt/tools/ysoserial.jar
  /opt/tools/marshalsec.jar
  /opt/tools/JNDI-Injection-Exploit.jar
  /opt/tools/stegsolve.jar
  /usr/bin/chisel
  /usr/share/chisel-common-binaries/chisel-linux-amd64
  /usr/share/chisel-common-binaries/chisel-linux-arm64
)

# Optional / best-effort: report actual launchability, never treat mere file
# existence as success, never fail the build when absent.
OPTIONAL_CMDS_FULL=(
  jadx apktool aapt apksigner zipalign adb dex2jar
  one_gadget seccomp-tools zsteg
  GoReSym
  playwright
)

run_verify() {
  local arch
  arch="$(uname -m)"
  echo "== muteki worker image verify (variant=${VARIANT}, arch=${arch}) =="

  local versions=()
  local name bin out
  for entry in "${ENGINE_CHECKS[@]}"; do
    name="${entry%%:*}"
    bin="${entry#*:}"
    if ! out="$(timeout 60 "$bin" --version 2>&1)"; then
      echo "FAIL: engine ${name} (${bin}) did not start via --version" >&2
      exit 1
    fi
    if [[ -z "${out// }" ]]; then
      echo "FAIL: engine ${name} (${bin}) returned empty --version" >&2
      exit 1
    fi
    echo "OK engine ${name}: ${out}"
    versions+=("${name}=${out//$'\n'/ }")
  done
  local engine_count="${#ENGINE_CHECKS[@]}"
  if [[ "$engine_count" -ne 9 ]]; then
    echo "FAIL: expected 9 engines, got ${engine_count}" >&2
    exit 1
  fi
  echo "OK: all 9 container engines started"

  local path
  for path in "${CORE_BOTH[@]}"; do
    if [[ ! -e "$path" ]]; then
      echo "FAIL: missing core path ${path}" >&2
      exit 1
    fi
    echo "OK core path ${path}"
  done
  if [[ ! -x /opt/muteki/runtime_agent ]]; then
    echo "FAIL: /opt/muteki/runtime_agent not executable" >&2
    exit 1
  fi
  if [[ ! -x /usr/local/bin/blackboard.py ]]; then
    echo "FAIL: /usr/local/bin/blackboard.py not executable" >&2
    exit 1
  fi
  # Blackboard must actually run, not only exist on disk.
  if ! out="$(timeout 20 python3 /usr/local/bin/blackboard.py --help 2>&1)"; then
    echo "FAIL: blackboard.py --help failed" >&2
    exit 1
  fi
  echo "OK blackboard help: ${out}"

  local cmd
  for cmd in "${CORE_CMDS_BOTH[@]}"; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
      echo "FAIL: missing core command ${cmd}" >&2
      exit 1
    fi
  done

  # Commands safe to exercise with --version in headless CI (no GUI).
  SAFE_VERSION_CMDS=(nmap masscan ffuf gobuster sqlmap nuclei gh jwt_tool cloudfox
    forge cast anvil sage vol radare2 gdb gdb-multiarch chisel ssh openvpn)
  if [[ "$VARIANT" == "full" ]]; then
    for cmd in "${CORE_CMDS_FULL[@]}"; do
      if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "FAIL: missing full core command ${cmd}" >&2
        exit 1
      fi
      echo "OK full core cmd present ${cmd}"
    done
    # Primary probe flag per command; `-h` is the fallback. Some tools exit non-zero
    # on --version/-h even when healthy (masscan exits 1 on both; ssh only knows -V).
    probe_flag() {
      case "$1" in
        ssh) echo "-V" ;;
        masscan) echo "--selftest" ;;
        *) echo "--version" ;;
      esac
    }
    local probe_failed=()
    for cmd in "${SAFE_VERSION_CMDS[@]}"; do
      if ! command -v "$cmd" >/dev/null 2>&1; then
        continue
      fi
      # Capture output so a failure shows the real error (e.g. exec EPERM from file
      # capabilities outside Docker's default bounding set) instead of a bare FAIL.
      local flag ver_rc=0 help_rc=0 ver_out="" help_out=""
      flag="$(probe_flag "$cmd")"
      ver_out="$(timeout 20 "$cmd" "$flag" 2>&1)" || ver_rc=$?
      if [[ "$ver_rc" -ne 0 ]]; then
        help_out="$(timeout 20 "$cmd" -h 2>&1)" || help_rc=$?
      fi
      if [[ "$ver_rc" -ne 0 && "$help_rc" -ne 0 ]]; then
        echo "FAIL: full core command ${cmd} failed a bounded ${flag}/-h probe" >&2
        echo "  ${cmd} ${flag} exit=${ver_rc}; first lines:" >&2
        printf '%s\n' "$ver_out" | head -n 5 | sed 's/^/    | /' >&2 || true
        echo "  ${cmd} -h exit=${help_rc}; first lines:" >&2
        printf '%s\n' "$help_out" | head -n 5 | sed 's/^/    | /' >&2 || true
        if command -v getcap >/dev/null 2>&1; then
          # Kali wraps some tools (e.g. /usr/bin/nmap -> /usr/lib/nmap/nmap).
          getcap "$(command -v "$cmd")" "/usr/lib/${cmd}/${cmd}" 2>/dev/null | sed 's/^/  getcap: /' >&2 || true
        fi
        probe_failed+=("$cmd")
        continue
      fi
      echo "OK full core cmd runnable ${cmd} (user=$(id -un) ${flag} exit=${ver_rc}${help_out:+ -h exit=${help_rc}})"
    done
    # Evidence for #173 review: file caps must stay inside Docker's default set.
    if command -v getcap >/dev/null 2>&1; then
      echo "-- file capabilities (getcap -r /usr /opt) --"
      getcap -r /usr /opt 2>/dev/null | sed 's/^/getcap: /' || true
    fi
    # Probe every command before failing so one CI run reports all broken tools.
    if [[ "${#probe_failed[@]}" -gt 0 ]]; then
      echo "FAIL: full core commands not runnable: ${probe_failed[*]}" >&2
      exit 1
    fi
    for path in "${CORE_FILES_FULL[@]}"; do
      if [[ ! -s "$path" ]]; then
        echo "FAIL: missing/empty full core file ${path}" >&2
        exit 1
      fi
      echo "OK full core file ${path}"
    done

    echo "-- optional tools (informational; absence is OK) --"
    optional_probe() {
      case "$1" in
        aapt)
          timeout 20 aapt version >/dev/null 2>&1
          ;;
        zipalign)
          # zipalign exits 2 for its help flags, so exercise a real ZIP instead.
          local probe_dir
          probe_dir="$(mktemp -d)" || return 1
          printf 'probe' > "$probe_dir/data" || return 1
          (cd "$probe_dir" && zip -q input.zip data &&
            timeout 20 zipalign -f 4 input.zip output.zip &&
            timeout 20 zipalign -c 4 output.zip) >/dev/null 2>&1
          ;;
        *)
          timeout 20 "$1" --version >/dev/null 2>&1 ||
            timeout 20 "$1" --help >/dev/null 2>&1 ||
            timeout 20 "$1" -h >/dev/null 2>&1
          ;;
      esac
    }
    for cmd in "${OPTIONAL_CMDS_FULL[@]}"; do
      if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "OPTIONAL_MISSING ${cmd}"
        continue
      fi
      if optional_probe "$cmd"; then
        echo "OPTIONAL_OK ${cmd}"
      else
        # Binary on PATH but could not be exercised — do not claim usable.
        echo "OPTIONAL_UNUSABLE ${cmd} (on PATH but --version/--help failed)"
      fi
    done
    # Playwright Chromium: presence of the python module is not enough.
    if python3 -c 'import playwright' >/dev/null 2>&1; then
      if python3 -c 'from playwright.sync_api import sync_playwright' >/dev/null 2>&1; then
        echo "OPTIONAL_OK playwright-python-import"
      else
        echo "OPTIONAL_UNUSABLE playwright-python"
      fi
    else
      echo "OPTIONAL_MISSING playwright-python"
    fi
  else
    echo "slim variant: skipping Kali full core/optional probes"
  fi

  if [[ -n "$VERSIONS_FILE" ]]; then
    printf '%s\n' "${versions[@]}" > "$VERSIONS_FILE"
    echo "wrote engine versions to ${VERSIONS_FILE}"
  fi
  echo "== verify passed (8 engines + core tools) =="
}

if [[ "$IN_CONTAINER" -eq 1 ]]; then
  run_verify
  exit 0
fi

if [[ -z "$IMAGE" ]]; then
  echo "--image is required unless --in-container" >&2
  exit 2
fi

script_host="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/verify_worker_image.sh"
docker_args=(run --rm --user kali --security-opt no-new-privileges=true -e HOME=/home/kali --entrypoint bash)
if [[ -n "$PLATFORM" ]]; then
  docker_args+=(--platform "$PLATFORM")
fi
docker_args+=(-v "${script_host}:/tmp/verify_worker_image.sh:ro")

inner=(/tmp/verify_worker_image.sh --in-container --variant "$VARIANT")
if [[ -n "$VERSIONS_FILE" ]]; then
  versions_dir="$(cd "$(dirname "$VERSIONS_FILE")" && pwd)"
  versions_base="$(basename "$VERSIONS_FILE")"
  versions_host="${versions_dir}/${versions_base}"
  : > "$versions_host"
  # The container writes this as `kali`, whose uid differs per image (1000 in the
  # Kali-based full image, 1001 in the Ubuntu-based slim image) and need not match
  # the host/runner uid: make the host file world-writable, and mount it outside
  # /tmp — with fs.protected_regular (Ubuntu default), O_CREAT on a file owned by
  # another uid inside a sticky world-writable dir fails with EACCES even at 0666.
  chmod 0666 "$versions_host"
  docker_args+=(-v "${versions_host}:/opt/muteki-verify/engine-versions.txt")
  inner+=(--versions-file /opt/muteki-verify/engine-versions.txt)
fi
docker_args+=("$IMAGE" "${inner[@]}")
exec docker "${docker_args[@]}"
