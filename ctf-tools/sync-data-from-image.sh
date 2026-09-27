#!/usr/bin/env bash
set -euo pipefail

ctf_tools_root="$(cd "$(dirname "$0")" && pwd)"
worker_image="${1:-${MUTEKI_WORKER_IMAGE:-}}"

if [ -z "$worker_image" ]; then
  worker_image="$(docker images --format '{{.Repository}}:{{.Tag}}' | awk '/^muteki-worker:/{print; exit}')"
fi
if [ -z "$worker_image" ]; then
  worker_image="ghcr.io/fishcodetech/muteki-worker:latest"
fi

for required_dir in \
  "$ctf_tools_root/wordlists/SecLists" \
  "$ctf_tools_root/wordlists/kali" \
  "$ctf_tools_root/knowledges" \
  "$ctf_tools_root/pocs" \
  "$ctf_tools_root/nuclei-templates" \
  "$ctf_tools_root/tools"; do
  if [ -d "$required_dir" ] && [ -n "$(find "$required_dir" -mindepth 1 -maxdepth 1 -print -quit)" ]; then
    echo "ERROR: $required_dir 已有内容；为避免覆盖，本次同步已停止。" >&2
    exit 1
  fi
done

mkdir -p \
  "$ctf_tools_root/wordlists/SecLists" \
  "$ctf_tools_root/wordlists/kali" \
  "$ctf_tools_root/knowledges" \
  "$ctf_tools_root/pocs" \
  "$ctf_tools_root/nuclei-templates" \
  "$ctf_tools_root/tools/jars" \
  "$ctf_tools_root/tools/jwt_tool" \
  "$ctf_tools_root/tools/whatweb/lib" \
  "$ctf_tools_root/tools/target-binaries"

export_container="$(docker create --entrypoint /bin/true "$worker_image")"
cleanup_export_container() {
  docker rm -f "$export_container" >/dev/null 2>&1 || true
}
trap cleanup_export_container EXIT

echo ">> 从 $worker_image 导出离线资料到 $ctf_tools_root"
docker cp "$export_container:/usr/share/seclists/." "$ctf_tools_root/wordlists/SecLists/"
docker cp "$export_container:/usr/share/wordlists/." "$ctf_tools_root/wordlists/kali/"
docker cp "$export_container:/home/kali/knowledges/." "$ctf_tools_root/knowledges/"
docker cp "$export_container:/home/kali/pocs/." "$ctf_tools_root/pocs/"
docker cp "$export_container:/home/kali/.local/nuclei-templates/." "$ctf_tools_root/nuclei-templates/"
docker cp "$export_container:/opt/tools/." "$ctf_tools_root/tools/jars/"
docker cp "$export_container:/opt/jwt_tool/." "$ctf_tools_root/tools/jwt_tool/"
docker cp "$export_container:/usr/share/whatweb/." "$ctf_tools_root/tools/whatweb/"
docker cp "$export_container:/usr/bin/whatweb" "$ctf_tools_root/tools/whatweb/whatweb"
docker cp "$export_container:/usr/lib/ruby/vendor_ruby/." "$ctf_tools_root/tools/whatweb/lib/"
docker cp "$export_container:/usr/local/bin/php_filter_chain_generator.py" "$ctf_tools_root/tools/php_filter_chain_generator.py"
docker cp "$export_container:/opt/gef/gef.py" "$ctf_tools_root/tools/gef.py"
docker cp "$export_container:/usr/local/bin/cdk" "$ctf_tools_root/tools/target-binaries/cdk-linux-arm64"
docker cp "$export_container:/opt/muteki/callback-clients/." \
  "$ctf_tools_root/tools/target-binaries/" 2>/dev/null || \
  echo ">> 当前镜像未包含可选 callback-clients，跳过"

docker image inspect --format '{{.Id}}' "$worker_image" > "$ctf_tools_root/.image-source"
echo ">> 离线资料同步完成"
