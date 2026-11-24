#!/usr/bin/env bash
set -euo pipefail
image=${1:?Usage: smoke-web-container.sh IMAGE}
name="muteki-web-smoke-${GITHUB_RUN_ID:-$$}"
mkdir -p .cache/ci-logs
certificate_dir=$(mktemp -d)
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -keyout "$certificate_dir/key.pem" -out "$certificate_dir/cert.pem" -subj /CN=localhost -addext 'subjectAltName=IP:127.0.0.1,DNS:localhost' >/dev/null 2>&1
chmod 644 "$certificate_dir/key.pem"
cleanup() {
  docker logs "$name" > .cache/ci-logs/web-container.log 2>&1 || true
  docker rm -f "$name" >/dev/null 2>&1 || true
  rm -rf "$certificate_dir"
}
trap cleanup EXIT

docker run -d --name "$name" -p 127.0.0.1::3101 \
  -v "$certificate_dir:/run/tls:ro" -e MUTEKI_WEB_TLS_CERT=/run/tls/cert.pem -e MUTEKI_WEB_TLS_KEY=/run/tls/key.pem \
  -e MUTEKI_STATE_DIR=/tmp/muteki-state \
  -e MUTEKI_WEB_PASSWORD=ci-container-smoke "$image"
port=$(docker port "$name" 3101/tcp | head -1 | awk -F: '{print $NF}')
token=""
for attempt in $(seq 1 90); do
  if [[ -z "$token" ]]; then
    if login=$(curl --cacert "$certificate_dir/cert.pem" --fail --silent --max-time 2 -H 'Content-Type: application/json' -H 'X-Muteki-CSRF: 1' \
      -d '{"password":"ci-container-smoke","client":"api"}' "https://127.0.0.1:$port/api/auth/login"); then
      token=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])' <<< "$login")
    fi
  fi
  if [[ -n "$token" ]] && curl --cacert "$certificate_dir/cert.pem" --fail --silent --max-time 2 -H "Authorization: Bearer $token" "https://127.0.0.1:$port/api/readiness" > .cache/ci-logs/container-readiness.json; then
    python3 - <<'PY'
import json
with open('.cache/ci-logs/container-readiness.json') as f:
    health=json.load(f)
assert health['app']=='muteki-rust' and health['ready'] is True, health
PY
    curl --cacert "$certificate_dir/cert.pem" --fail --silent --max-time 10 "https://127.0.0.1:$port/" > .cache/ci-logs/container-page.html
    grep -qi muteki .cache/ci-logs/container-page.html
    echo 'WEB_CONTAINER_OK: real image, API readiness and UI'
    exit 0
  fi
  if [[ $(docker inspect --format '{{.State.Running}}' "$name") != true ]]; then
    docker logs "$name"
    exit 1
  fi
  sleep 1
done
docker logs "$name"
echo 'Web container did not become ready' >&2
exit 1
