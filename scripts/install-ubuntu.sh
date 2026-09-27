#!/usr/bin/env bash
# Single-node Ubuntu 24.04 installer. Run as the operator, never as root.
# The checkout is the initial application source; --managed switches to the
# existing verified release installer once a release with --ui-host exists.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
backend=container
managed=0
preflight=0
check_only=0
no_service=0
with_ctf_tools=0
ui_port=3001
release_version=""
worker_image="${MUTEKI_WORKER_IMAGE:-ghcr.io/fishcodetech/muteki-worker:latest}"
node_version=22.19.0
install_root="$HOME/.local/share/muteki"
local_tool_root="$install_root/data/ctf-tools"
config_dir="$HOME/.config/muteki"
config_file="$config_dir/ubuntu.env"
service_file=/etc/systemd/system/muteki-web.service
node_root="$install_root/toolchains/node-$node_version"

usage() {
  cat <<'USAGE'
Usage: ./scripts/install-ubuntu.sh [options]
  --backend local|container|both  Worker backends to prepare (default: container)
  --with-ctf-tools               Install optional native CTF tools for local Workers
  --worker-image REF              Full Worker image to pull and verify
  --ui-port PORT                  UI port exposed by the VM (default: 3001)
  --managed [VERSION]             Use the existing release installer and updater
  --no-service                    Prepare dependencies without a systemd service
  --preflight                     Check Ubuntu and architecture, change nothing
  --check                         Check the selected installation, change nothing
  -h, --help                      Show this help

The UI listens on 0.0.0.0; the API listens on 127.0.0.1. A generated Web
password is kept in ~/.config/muteki/ubuntu.env. Put additional service
variables in ~/.config/muteki/operator.env; the installer never rewrites it.
Native CLI sign-in is done separately by the operator or Credential Accounts.
USAGE
}

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
note() { printf '==> %s\n' "$*"; }
vm_address() {
  local address
  address="$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' | head -n 1)"
  if [ -z "$address" ]; then address="$(hostname -I 2>/dev/null | awk '{print $1}')"; fi
  printf '%s' "$address"
}
node_compatible() {
  local version major minor
  command -v node >/dev/null 2>&1 && command -v npm >/dev/null 2>&1 || return 1
  version="$(node --version 2>/dev/null | sed -n 's/^v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\).*/\1/p')"
  [ -n "$version" ] || return 1
  major="${version%%.*}"
  minor="${version#*.}"
  minor="${minor%%.*}"
  [ "$major" -gt 22 ] || { [ "$major" -eq 22 ] && [ "$minor" -ge 19 ]; }
}

check_vm_ports() {
  local address
  address="$(vm_address)"
  [ -n "$address" ] || { echo 'VM address not detected; check host port forwarding manually'; return 0; }
  curl --silent --show-error --fail --max-time 5 "http://$address:$ui_port/" >/dev/null || {
    echo "UI is not reachable at http://$address:$ui_port/" >&2; return 1;
  }
  if nc -z -w 2 "$address" 8000 >/dev/null 2>&1; then
    echo "API is unexpectedly reachable at $address:8000" >&2; return 1
  fi
  if [ "$backend" = container ] || [ "$backend" = both ]; then
    if nc -z -w 2 "$address" 9100 >/dev/null 2>&1; then
      echo "control port is unexpectedly reachable at $address:9100" >&2; return 1
    fi
  fi
  echo "UI reachable on VM address $address; API limited to loopback"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --backend) backend="${2:?--backend needs a value}"; shift 2 ;;
    --with-ctf-tools) with_ctf_tools=1; shift ;;
    --worker-image) worker_image="${2:?--worker-image needs a value}"; shift 2 ;;
    --ui-port) ui_port="${2:?--ui-port needs a value}"; shift 2 ;;
    --managed)
      managed=1
      shift
      if [ "$#" -gt 0 ] && [[ "$1" != --* ]]; then release_version="$1"; shift; fi
      ;;
    --no-service) no_service=1; shift ;;
    --preflight) preflight=1; shift ;;
    --check) check_only=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

case "$backend" in local|container|both) ;; *) die "--backend must be local, container, or both" ;; esac
[[ "$ui_port" =~ ^[0-9]+$ ]] && [ "$ui_port" -ge 1 ] && [ "$ui_port" -le 65535 ] || die "invalid --ui-port"
if [ "$with_ctf_tools" -eq 1 ] && [ "$backend" = container ]; then
  die "--with-ctf-tools requires local or both backends"
fi
[ -f /etc/os-release ] || die "cannot identify this Linux distribution"
# shellcheck disable=SC1091
source /etc/os-release
[ "${ID:-}" = ubuntu ] && [ "${VERSION_ID:-}" = 24.04 ] || die "only Ubuntu 24.04 is supported"
command -v dpkg >/dev/null 2>&1 || die "dpkg is missing"
architecture="$(dpkg --print-architecture)"
case "$architecture" in amd64|arm64) ;; *) die "unsupported architecture: $architecture" ;; esac

note "Ubuntu 24.04 $architecture; backend=$backend; UI port=$ui_port"
if [ "$preflight" -eq 1 ]; then
  note "preflight passed (no changes made)"
  exit 0
fi
[ "$(id -u)" -ne 0 ] || die "run as the operator account; apt actions use sudo"

export PATH="$node_root/bin:$HOME/.local/bin:$HOME/.grok/bin:$PATH"

check_engine() {
  local name="$1" binary="$2"
  if ! command -v "$binary" >/dev/null 2>&1; then
    printf 'MISSING %-9s %s\n' "$name" "$binary" >&2
    return 1
  fi
  if ! timeout 30 "$binary" --version >/dev/null 2>&1; then
    printf 'BROKEN  %-9s %s\n' "$name" "$binary" >&2
    return 1
  fi
  printf 'OK      %-9s %s\n' "$name" "$(command -v "$binary")"
}

check_local_engines() {
  local failed=0
  check_engine claude claude || failed=1
  check_engine codex codex || failed=1
  check_engine cursor cursor-agent || failed=1
  check_engine pi pi || failed=1
  check_engine omp omp || failed=1
  check_engine kimi kimi || failed=1
  check_engine grok grok || failed=1
  check_engine opencode opencode || failed=1
  return "$failed"
}

check_installation() {
  local failed=0 app_root="$project_root"
  if [ "$managed" -eq 1 ]; then app_root="$install_root/current"; fi
  command -v uv >/dev/null 2>&1 || { echo 'MISSING uv' >&2; failed=1; }
  node_compatible || { echo 'MISSING Node 22.19+ and npm' >&2; failed=1; }
  [ -x "$app_root/.venv/bin/python" ] || { echo "MISSING $app_root/.venv" >&2; failed=1; }
  if [ "$backend" = local ] || [ "$backend" = both ]; then
    check_local_engines || failed=1
  fi
  if [ "$backend" = container ] || [ "$backend" = both ]; then
    { docker info >/dev/null 2>&1 || sudo -n docker info >/dev/null 2>&1; } || {
      echo 'MISSING usable Docker daemon' >&2; failed=1;
    }
    if [ -f "$config_file" ]; then
      worker_image="$(sed -n 's/^MUTEKI_WORKER_IMAGE=//p' "$config_file" | tail -n 1)"
    fi
    if [ -z "$worker_image" ] || ! { docker image inspect "$worker_image" >/dev/null 2>&1 || \
      sudo -n docker image inspect "$worker_image" >/dev/null 2>&1; }; then
      echo "MISSING Worker image: $worker_image" >&2; failed=1;
    fi
  fi
  if [ "$no_service" -eq 0 ]; then
    systemctl is-active --quiet muteki-web.service || { echo 'MISSING active muteki-web.service' >&2; failed=1; }
    curl --silent --show-error --fail --max-time 5 http://127.0.0.1:8000/api/health >/dev/null || failed=1
    curl --silent --show-error --fail --max-time 5 "http://127.0.0.1:$ui_port/" >/dev/null || failed=1
    check_vm_ports || failed=1
  fi
  return "$failed"
}

if [ "$check_only" -eq 1 ]; then
  check_installation
  exit $?
fi

command -v sudo >/dev/null 2>&1 || die "sudo is required to install Ubuntu packages"
note 'installing application prerequisites'
sudo apt-get update
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
  ca-certificates curl git xz-utils unzip coreutils openssl build-essential python3 ripgrep iproute2 netcat-openbsd

if ! command -v uv >/dev/null 2>&1; then
  note 'installing uv'
  curl -LsSf https://astral.sh/uv/install.sh | sh
fi
export PATH="$HOME/.local/bin:$PATH"
command -v uv >/dev/null 2>&1 || die "uv installation did not provide an executable"

if ! node_compatible; then
  note "installing Node $node_version for $architecture"
  case "$architecture" in amd64) node_arch=x64 ;; arm64) node_arch=arm64 ;; esac
  node_archive="node-v$node_version-linux-$node_arch.tar.xz"
  temporary_dir="$(mktemp -d)"
  trap 'rm -rf "$temporary_dir"' EXIT
  curl -fsSL "https://nodejs.org/dist/v$node_version/$node_archive" -o "$temporary_dir/$node_archive"
  curl -fsSL "https://nodejs.org/dist/v$node_version/SHASUMS256.txt" -o "$temporary_dir/SHASUMS256.txt"
  awk -v name="$node_archive" '$2 == name {print}' "$temporary_dir/SHASUMS256.txt" > "$temporary_dir/expected"
  [ -s "$temporary_dir/expected" ] || die "Node archive absent from official checksum list"
  ( cd "$temporary_dir" && sha256sum -c expected )
  mkdir -p "$node_root"
  tar -xJf "$temporary_dir/$node_archive" -C "$node_root" --strip-components=1
  rm -rf "$temporary_dir"
  trap - EXIT
fi
export PATH="$node_root/bin:$HOME/.local/bin:$PATH"
node --version
npm --version

if [ "$backend" = local ] || [ "$backend" = both ]; then
  install_npm_engine() {
    local binary="$1" package="$2"
    if command -v "$binary" >/dev/null 2>&1 && timeout 30 "$binary" --version >/dev/null 2>&1; then
      return 0
    fi
    note "installing $binary"
    npm install --global --prefix "$HOME/.local" --no-audit --no-fund "$package"
  }
  install_npm_engine claude '@anthropic-ai/claude-code@2.1.257'
  install_npm_engine codex '@openai/codex@0.146.0'
  install_npm_engine pi '@earendil-works/pi-coding-agent@0.84.1'
  install_npm_engine kimi '@moonshot-ai/kimi-code@0.40.1'
  install_npm_engine opencode 'opencode-ai@1.18.22'

  install_script_engine() {
    local binary="$1" url="$2" interpreter="$3"
    shift 3
    if command -v "$binary" >/dev/null 2>&1 && timeout 30 "$binary" --version >/dev/null 2>&1; then
      return 0
    fi
    note "installing $binary"
    local installer
    installer="$(mktemp)"
    curl -fsSL "$url" -o "$installer"
    "$interpreter" "$installer" "$@"
    rm -f "$installer"
  }
  install_script_engine cursor-agent https://cursor.com/install bash
  install_script_engine omp https://omp.sh/install sh --binary --ref v17.3.5
  install_script_engine grok https://x.ai/cli/install.sh bash 1.0.5
  check_local_engines || die "one or more local engine CLIs failed validation"
fi

pinned_image=""
control_bind=127.0.0.1
if [ "$backend" = container ] || [ "$backend" = both ]; then
  if ! command -v docker >/dev/null 2>&1; then
    note 'installing Docker engine'
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends docker.io
  fi
  if [ -d /run/systemd/system ]; then
    sudo systemctl enable --now docker
  elif [ "$no_service" -eq 0 ]; then
    die "systemd is unavailable; use --no-service only for an existing Docker daemon"
  fi
  if ! id -nG | tr ' ' '\n' | rg -qx docker; then
    sudo usermod -aG docker "$(id -un)"
    note 'Docker group added; the system service receives it immediately. Your shell receives it after the next login.'
  fi
  docker_command=(docker)
  if ! docker info >/dev/null 2>&1; then docker_command=(sudo docker); fi
  "${docker_command[@]}" info >/dev/null 2>&1 || die "Docker daemon is unavailable"
  control_bind="$("${docker_command[@]}" network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}')"
  [ -n "$control_bind" ] && [ "$control_bind" != '<no value>' ] || die "Docker bridge gateway is unavailable"
  note "pulling Worker image $worker_image"
  "${docker_command[@]}" pull "$worker_image"
  pinned_image="$("${docker_command[@]}" image inspect --format '{{index .RepoDigests 0}}' "$worker_image")"
  [ -n "$pinned_image" ] && [ "$pinned_image" != '<no value>' ] || die "Worker image has no pullable digest"
  note "verifying $pinned_image"
  if [ "${docker_command[0]}" = sudo ]; then
    sudo "$project_root/scripts/verify_worker_image.sh" --image "$pinned_image" --variant full --platform "linux/$architecture"
  else
    "$project_root/scripts/verify_worker_image.sh" --image "$pinned_image" --variant full --platform "linux/$architecture"
  fi
fi

if [ "$with_ctf_tools" -eq 1 ]; then
  MUTEKI_LOCAL_WORKER_ROOT="$local_tool_root" "$project_root/ctf-tools/setup-ubuntu.sh"
fi

if [ "$managed" -eq 1 ]; then
  note 'installing verified application release'
  if [ -n "$release_version" ]; then
    "$project_root/run.sh" install "$release_version"
  else
    "$project_root/run.sh" install
  fi
  app_root="$install_root/current"
  [ -f "$app_root/run.sh" ] || die "managed application install is incomplete"
  rg -q -- '--ui-host' "$app_root/run.sh" || die "installed release predates independent UI binding; publish a newer release or omit --managed"
  launcher="$HOME/.local/bin/muteki"
else
  note 'installing application dependencies from this checkout'
  app_root="$project_root"
  ( cd "$app_root" && uv sync --frozen --no-dev )
  launcher="$app_root/run.sh"
fi
( cd "$app_root/apps/web/ui" && npm ci --no-audit --no-fund )

mkdir -p "$install_root/data/sessions" "$install_root/data/state/_secrets" "$config_dir"
chmod 700 "$config_dir" "$install_root/data/sessions" "$install_root/data/state" "$install_root/data/state/_secrets"
existing_password="$(sed -n 's/^MUTEKI_WEB_PASSWORD=//p' "$config_file" 2>/dev/null | tail -n 1 || true)"
web_password="${MUTEKI_WEB_PASSWORD:-$existing_password}"
if [ -z "$web_password" ]; then web_password="$(openssl rand -hex 24)"; fi
[[ "$web_password" =~ ^[A-Za-z0-9_-]+$ ]] || die "MUTEKI_WEB_PASSWORD must use letters, numbers, _ or - in the generated service file"
umask 077
{
  printf 'MUTEKI_WEB_PASSWORD=%s\n' "$web_password"
  printf 'MUTEKI_SESSIONS_ROOT=%s\n' "$install_root/data/sessions"
  printf 'MUTEKI_STATE_ROOT=%s\n' "$install_root/data/state"
  printf 'MUTEKI_COORDINATOR_CONTROL_ROOT=%s\n' "$install_root/data/state/control"
  printf 'MUTEKI_CONTROL_BIND=%s\n' "$control_bind"
  printf 'MUTEKI_UBUNTU_BACKEND=%s\n' "$backend"
  if [ -f "$local_tool_root/.ready" ]; then printf 'MUTEKI_LOCAL_WORKER_ROOT=%s\n' "$local_tool_root"; fi
  if [ -n "$pinned_image" ]; then printf 'MUTEKI_WORKER_IMAGE=%s\n' "$pinned_image"; fi
} > "$config_file"
chmod 600 "$config_file"

if [ "$no_service" -eq 0 ]; then
  command -v systemctl >/dev/null 2>&1 || die "systemd is required unless --no-service is set"
  service_tmp="$(mktemp)"
  cat > "$service_tmp" <<UNIT
[Unit]
Description=Muteki single-node Web workbench
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$(id -un)
Group=$(id -gn)
WorkingDirectory="$app_root"
EnvironmentFile="$config_file"
EnvironmentFile=-"$config_dir/operator.env"
Environment="HOME=$HOME"
Environment="PATH=$node_root/bin:$HOME/.local/bin:$HOME/.grok/bin:/usr/local/bin:/usr/bin:/bin"
ExecStart="$launcher" web --host 127.0.0.1 --ui-host 0.0.0.0 --ui-port $ui_port
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT
  if [ "$backend" = container ] || [ "$backend" = both ]; then
    sed -i '/^Group=/a SupplementaryGroups=docker' "$service_tmp"
    sed -i '/^After=/s/$/ docker.service/; /^Wants=/s/$/ docker.service/' "$service_tmp"
  fi
  sudo install -m 0644 "$service_tmp" "$service_file"
  rm -f "$service_tmp"
  sudo systemctl daemon-reload
  sudo systemctl enable muteki-web.service
  sudo systemctl restart muteki-web.service
  if command -v ufw >/dev/null 2>&1 && sudo ufw status | rg -q '^Status: active'; then
    sudo ufw allow "$ui_port/tcp"
  fi
  for attempt in $(seq 1 60); do
    if curl --silent --fail --max-time 3 http://127.0.0.1:8000/api/health >/dev/null 2>&1 && \
       curl --silent --fail --max-time 3 "http://127.0.0.1:$ui_port/" >/dev/null 2>&1; then
      break
    fi
    if [ "$attempt" -eq 60 ]; then
      die "service did not become healthy; inspect: journalctl -u muteki-web.service -n 100"
    fi
    sleep 3
  done
  check_vm_ports || die "VM port check failed"
fi

note "installation complete; credentials: $config_file"
if [ "$no_service" -eq 0 ]; then
  vm_ip="$(vm_address)"
  note "open http://${vm_ip:-<VM-IP>}:$ui_port/ from the host (forward the VM port when using NAT)"
fi
check_command="$project_root/scripts/install-ubuntu.sh --backend $backend --ui-port $ui_port --check"
if [ "$managed" -eq 1 ]; then check_command="$check_command --managed"; fi
if [ "$no_service" -eq 1 ]; then check_command="$check_command --no-service"; fi
note "check: $check_command"
if [ "$managed" -eq 1 ]; then note 'upgrade/rollback: muteki upgrade; muteki rollback'; fi
