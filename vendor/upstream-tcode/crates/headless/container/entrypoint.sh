#!/bin/sh
set -eu

: "${PUID:=1000}" "${PGID:=1000}"
for value in "$PUID" "$PGID"; do
    case "$value" in
        ''|*[!0-9]*|0[0-9]*) echo 'PUID and PGID must be numeric IDs without leading zeroes' >&2; exit 1 ;;
    esac
done
[ "$PUID" != 0 ] || { echo 'PUID must not be root (0)' >&2; exit 1; }

if [ "$(id -u)" = 0 ]; then
    groupmod --non-unique --gid "$PGID" tcode
    usermod --non-unique --uid "$PUID" --gid "$PGID" --home /data/home tcode
    mkdir -p /data/home /data/tcode /data/agents /data/bin
    # UID changes also need to preserve access to existing credentials and caches.
    if [ "$(stat -c '%u:%g' /data)" != "$PUID:$PGID" ]; then
        chown -hR "$PUID:$PGID" /data
    else
        chown "$PUID:$PGID" /data /data/tcode /data/agents /data/bin
    fi
    # Root's primary group is not inherited implicitly after dropping privileges.
    groups=$PGID
    for group in $(id -G); do
        [ "$group" = 0 ] || groups="$groups,$group"
    done
    if [ "${DOCKER_GID+x}" = x ]; then
        case "$DOCKER_GID" in
            ''|*[!0-9]*) echo 'DOCKER_GID must be a numeric group ID' >&2; exit 1 ;;
        esac
        groups="$groups,$DOCKER_GID"
    fi
    exec setpriv --reuid="$PUID" --regid="$PGID" --groups="$groups" "$0" "$@"
fi

umask 022
if [ "${1:-serve}" = serve ]; then
    if [ "${TCODE_AGENTS+x}" != x ]; then
        TCODE_AGENTS=$(node -e 'console.log(Object.keys(require("/opt/tcode/agents.json")).join(","))')
        export TCODE_AGENTS
    fi
    node /opt/tcode/install-agents.mjs /data/agents/bin
fi

exec tcode-headless "$@"
