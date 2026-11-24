#!/usr/bin/env bash
# Verify a Muteki Worker image (curated, heavy, or legacy full) against the registered container engines
# and core tools. Optional tools are probed and reported, but missing optionals
# do not fail the check.
#
# Usage:
#   ./scripts/verify_worker_image.sh --image <ref> --variant curated|heavy|full [--platform linux/amd64|linux/arm64]
#   ./scripts/verify_worker_image.sh --in-container --variant curated|heavy|full
#
# Exit 0 only when all registered engines meet the runtime minimum versions,
# and every required core tool is present and runnable. Prints a version inventory.
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
  curated|heavy|full) ;;
  *) echo "--variant must be curated, heavy or full" >&2; exit 2 ;;
esac

registry="/opt/muteki/engines.json"
if [[ "$IN_CONTAINER" -eq 0 ]]; then
  registry="$(cd "$(dirname "${BASH_SOURCE[0]}")/../docker/worker-common" && pwd)/engines.json"
fi
minimums="/opt/muteki/minimum_versions.json"
capabilities="/opt/muteki/capabilities.py"
if [[ "$IN_CONTAINER" -eq 0 ]]; then
  minimums="$(cd "$(dirname "${BASH_SOURCE[0]}")/../crates/server/src/agent_runtime" && pwd)/minimum_versions.json"
  capabilities="$(cd "$(dirname "${BASH_SOURCE[0]}")/../docker/worker-common" && pwd)/capabilities.py"
fi

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

OPTIONAL_CMDS_FULL=(
  jadx apktool aapt apksigner zipalign adb dex2jar
  one_gadget seccomp-tools zsteg
  GoReSym
  playwright
)

if [[ "$VARIANT" == "curated" || "$VARIANT" == "heavy" ]]; then
  CORE_CMDS_FULL=(nmap masscan ffuf gobuster sqlmap nikto whatweb tshark binwalk foremost exiftool steghide hydra john hashcat socat ncat proxychains4 chisel ssh sshpass openvpn nuclei radare2 gdb gdb-multiarch objdump strings)
  CORE_FILES_FULL=(/opt/muteki/browser_coord.py /opt/muteki/agent-browser/SKILL.md /opt/muteki/IMAGE_CAPABILITIES.json /opt/muteki/packages.tsv /opt/muteki/npm-packages.json /usr/share/chisel-common-binaries/chisel-linux-amd64 /usr/share/chisel-common-binaries/chisel-linux-arm64)
fi

if [[ "$VARIANT" == "heavy" ]]; then
  CORE_CMDS_FULL+=(sage vol)
  CORE_FILES_FULL+=(/opt/muteki/heavy-conda-packages.txt /opt/muteki/heavy-python-packages.txt /opt/muteki/heavy-python/bin/python)
fi

run_verify() {
  local arch
  arch="$(uname -m)"
  echo "== muteki worker image verify (variant=${VARIANT}, arch=${arch}) =="

  [[ "$(id -u)" == 0 ]] || { echo "FAIL: Worker Agent must run as root" >&2; exit 1; }
  echo "OK execution user root (uid=0)"

  local version_args=(--registry "$registry" --minimum-versions "$minimums" --check-only)
  if [[ -n "$VERSIONS_FILE" ]]; then
    version_args+=(--versions-file "$VERSIONS_FILE")
  fi
  python3 "$capabilities" "${version_args[@]}"
  local engine_count
  engine_count="$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$registry")"
  local out

  local path
  for path in "${CORE_BOTH[@]}"; do
    if [[ ! -e "$path" ]]; then
      echo "FAIL: missing core path ${path}" >&2
      exit 1
    fi
    echo "OK core path ${path}"
  done
  if [[ "$VARIANT" == "full" || "$VARIANT" == "curated" || "$VARIANT" == "heavy" ]]; then
    if ! out="$(timeout 20 agent-browser --version 2>&1)" || [[ -z "${out// }" ]]; then
      echo "FAIL: image agent-browser did not start via --version" >&2
      exit 1
    fi
    echo "OK agent-browser: ${out}"
    if ! out="$(timeout 20 /usr/bin/chromium --version 2>&1)" || [[ -z "${out// }" ]]; then
      echo "FAIL: image Chromium runtime did not start via --version" >&2
      exit 1
    fi
    echo "OK Chromium runtime: ${out}"
    if ! out="$(timeout 20 agent-browser skills get core 2>&1)" || [[ -z "${out// }" ]]; then
      echo "FAIL: image agent-browser core Skill is unavailable" >&2
      exit 1
    fi
    echo "OK agent-browser core Skill"
  fi
  if [[ ! -x /opt/muteki/runtime_agent ]]; then
    echo "FAIL: /opt/muteki/runtime_agent not executable" >&2
    exit 1
  fi
  if [[ ! -x /usr/local/bin/blackboard.py ]]; then
    echo "FAIL: /usr/local/bin/blackboard.py not executable" >&2
    exit 1
  fi
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

  SAFE_VERSION_CMDS=(nmap masscan ffuf gobuster sqlmap nuclei gh jwt_tool cloudfox
    forge cast anvil sage vol radare2 gdb gdb-multiarch chisel ssh openvpn)
  if [[ "$VARIANT" == "full" || "$VARIANT" == "curated" || "$VARIANT" == "heavy" ]]; then
    for cmd in "${CORE_CMDS_FULL[@]}"; do
      if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "FAIL: missing full core command ${cmd}" >&2
        exit 1
      fi
      echo "OK full core cmd present ${cmd}"
    done
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
          getcap "$(command -v "$cmd")" "/usr/lib/${cmd}/${cmd}" 2>/dev/null | sed 's/^/  getcap: /' >&2 || true
        fi
        probe_failed+=("$cmd")
        continue
      fi
      echo "OK full core cmd runnable ${cmd} (user=$(id -un) ${flag} exit=${ver_rc}${help_out:+ -h exit=${help_rc}})"
    done
    if command -v getcap >/dev/null 2>&1; then
      echo "-- file capabilities (getcap -r /usr /opt) --"
      getcap -r /usr /opt 2>/dev/null | sed 's/^/getcap: /' || true
    fi
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
        echo "OPTIONAL_UNUSABLE ${cmd} (on PATH but --version/--help failed)"
      fi
    done
    if python3 -c 'import playwright' >/dev/null 2>&1; then
      if python3 -c 'from playwright.sync_api import sync_playwright' >/dev/null 2>&1; then
        echo "OPTIONAL_OK playwright-python-import"
      else
        echo "OPTIONAL_UNUSABLE playwright-python"
      fi
    else
      echo "OPTIONAL_MISSING playwright-python"
    fi
  fi

  if [[ "$VARIANT" == "heavy" ]]; then
    timeout 60 sage -c 'assert list(factor(15)) == [(3, 1), (5, 1)]; print(version())'
    timeout 60 /opt/muteki/heavy-python/bin/python -c 'import angr, volatility3; print(angr.__version__)'
    echo "OK specialist Sage/angr/Volatility3 runtime"
  fi

  if [[ -n "$VERSIONS_FILE" ]]; then
    echo "wrote engine versions to ${VERSIONS_FILE}"
  fi
  echo "== verify passed (${engine_count} engines + core tools) =="
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
docker_args=(run --rm --user 0:0 --cap-add NET_ADMIN --cap-add SYS_PTRACE -e HOME=/home/kali -e USER=root -e LOGNAME=root -e IS_SANDBOX=1 --entrypoint bash)
if [[ -n "$PLATFORM" ]]; then
  docker_args+=(--platform "$PLATFORM")
fi
docker_args+=(-v "${script_host}:/tmp/verify_worker_image.sh:ro" -v "${registry}:/opt/muteki/engines.json:ro"
  -v "${minimums}:/opt/muteki/minimum_versions.json:ro" -v "${capabilities}:/opt/muteki/capabilities.py:ro")

inner=(/tmp/verify_worker_image.sh --in-container --variant "$VARIANT")
if [[ -n "$VERSIONS_FILE" ]]; then
  versions_dir="$(cd "$(dirname "$VERSIONS_FILE")" && pwd)"
  versions_base="$(basename "$VERSIONS_FILE")"
  versions_host="${versions_dir}/${versions_base}"
  : > "$versions_host"
  docker_args+=(-v "${versions_host}:/opt/muteki-verify/engine-versions.txt")
  inner+=(--versions-file /opt/muteki-verify/engine-versions.txt)
fi
docker_args+=("$IMAGE" "${inner[@]}")
exec docker "${docker_args[@]}"
