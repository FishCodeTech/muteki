#!/usr/bin/env bash
set -euo pipefail

ctf_tools_root="$(cd "$(dirname "$0")" && pwd)"
install_packages=1
if [ "${1:-}" = "--link-only" ]; then
  install_packages=0
elif [ "$#" -gt 0 ]; then
  echo "用法: $0 [--link-only]" >&2
  exit 2
fi

if [ "$(uname -s)" != "Darwin" ]; then
  echo "ERROR: ctf-tools 原生环境只用于 macOS。" >&2
  exit 1
fi

mkdir -p "$ctf_tools_root/bin" "$ctf_tools_root/python" \
  "$ctf_tools_root/ruby"

if [ "$install_packages" -eq 1 ]; then
  command -v brew >/dev/null 2>&1 || {
    echo "ERROR: 需要 Homebrew。" >&2
    exit 1
  }
  command -v uv >/dev/null 2>&1 || {
    echo "ERROR: 需要 uv；先运行仓库根目录的 ./run.sh web 安装。" >&2
    exit 1
  }

  echo ">> 安装 Dockerfile 对应的 macOS 工具"
  brew bundle --file "$ctf_tools_root/Brewfile"

  echo ">> 创建目录内 Python 环境"
  uv venv --clear --python 3.13 "$ctf_tools_root/python"
  temporary_pypi_index="${MUTEKI_PYPI_INDEX:-https://pypi.tuna.tsinghua.edu.cn/simple}"
  temporary_no_proxy="pypi.tuna.tsinghua.edu.cn,${NO_PROXY:-}"
  UV_DEFAULT_INDEX="$temporary_pypi_index" \
    NO_PROXY="$temporary_no_proxy" no_proxy="$temporary_no_proxy" \
    uv pip install --python "$ctf_tools_root/python/bin/python" \
      -r "$ctf_tools_root/python-requirements.txt"
  UV_DEFAULT_INDEX="$temporary_pypi_index" \
    NO_PROXY="$temporary_no_proxy" no_proxy="$temporary_no_proxy" \
    uv pip install --python "$ctf_tools_root/python/bin/python" \
      'setuptools<81' wheel
  UV_DEFAULT_INDEX="$temporary_pypi_index" \
    NO_PROXY="$temporary_no_proxy" no_proxy="$temporary_no_proxy" \
    uv pip install --python "$ctf_tools_root/python/bin/python" \
      --no-build-isolation angr || \
      echo "optional Python package unavailable on macOS: angr"

  for gem_name in \
    one_gadget seccomp-tools zsteg getoptlong resolv-replace \
    addressable ipaddr json; do
    "$(brew --prefix ruby)/bin/gem" install \
      --install-dir "$ctf_tools_root/ruby" --no-document "$gem_name" || \
      echo "optional Ruby gem unavailable on macOS: $gem_name"
  done
fi

# One stable bin directory for the prepared security tools.
export PATH="$ctf_tools_root/python/bin:$ctf_tools_root/ruby/bin:$PATH"
while IFS= read -r tool_name; do
  [ -n "$tool_name" ] || continue
  tool_path="$(command -v "$tool_name" 2>/dev/null || true)"
  if [ -n "$tool_path" ] && [ "$tool_path" != "$ctf_tools_root/bin/$tool_name" ]; then
    ln -sfn "$tool_path" "$ctf_tools_root/bin/$tool_name"
  fi
done < "$ctf_tools_root/tool-list.txt"

# Prefer the supported native mitmproxy build over an old Python package that
# may still exist in a previously prepared directory.
if command -v brew >/dev/null 2>&1; then
  brew_bin="$(brew --prefix)/bin"
  for mitm_name in mitmproxy mitmdump mitmweb; do
    if [ -x "$brew_bin/$mitm_name" ]; then
      ln -sfn "$brew_bin/$mitm_name" "$ctf_tools_root/bin/$mitm_name"
    fi
  done
fi

if command -v d2j-dex2jar >/dev/null 2>&1; then
  ln -sfn "$(command -v d2j-dex2jar)" "$ctf_tools_root/bin/dex2jar"
fi
if [ -x "/opt/homebrew/opt/openjdk/bin/java" ]; then
  ln -sfn "/opt/homebrew/opt/openjdk/bin/java" "$ctf_tools_root/bin/java"
elif [ -x "/usr/local/opt/openjdk/bin/java" ]; then
  ln -sfn "/usr/local/opt/openjdk/bin/java" "$ctf_tools_root/bin/java"
fi

# Homebrew's package named "chisel" is an LLDB plugin. The Kali image uses
# jpillora/chisel, so build that exact tunnel tool into this directory.
if command -v go >/dev/null 2>&1; then
  if [ -L "$ctf_tools_root/bin/chisel" ]; then
    unlink "$ctf_tools_root/bin/chisel"
  fi
  GOBIN="$ctf_tools_root/bin" \
    GOPROXY="${MUTEKI_GO_PROXY:-https://goproxy.cn,direct}" \
    go install github.com/jpillora/chisel@v1.12.0
fi

if [ -f "$ctf_tools_root/tools/jwt_tool/jwt_tool.py" ]; then
  ln -sfn "$ctf_tools_root/tools/jwt_tool/jwt_tool.py" "$ctf_tools_root/bin/jwt_tool"
  chmod +x "$ctf_tools_root/tools/jwt_tool/jwt_tool.py"
fi
if [ -f "$ctf_tools_root/tools/php_filter_chain_generator.py" ]; then
  ln -sfn "$ctf_tools_root/tools/php_filter_chain_generator.py" \
    "$ctf_tools_root/bin/php_filter_chain_generator"
  chmod +x "$ctf_tools_root/tools/php_filter_chain_generator.py"
fi
if [ -f "$ctf_tools_root/tools/whatweb/whatweb" ]; then
  if [ -L "$ctf_tools_root/bin/whatweb" ]; then
    unlink "$ctf_tools_root/bin/whatweb"
  fi
  printf '#!/usr/bin/env bash\nRUBYLIB=%q exec ruby %q "$@"\n' \
    "$ctf_tools_root/tools/whatweb/lib" \
    "$ctf_tools_root/tools/whatweb/whatweb" \
    > "$ctf_tools_root/bin/whatweb"
  chmod +x "$ctf_tools_root/bin/whatweb"
  chmod +x "$ctf_tools_root/tools/whatweb/whatweb"
fi

for jar_name in ysoserial marshalsec stegsolve JNDI-Injection-Exploit; do
  jar_path="$ctf_tools_root/tools/jars/$jar_name.jar"
  if [ -f "$jar_path" ]; then
    launcher_name="$(printf '%s' "$jar_name" | tr '[:upper:]' '[:lower:]')"
    printf '#!/usr/bin/env bash\nexec java -jar %q "$@"\n' "$jar_path" \
      > "$ctf_tools_root/bin/$launcher_name"
    chmod +x "$ctf_tools_root/bin/$launcher_name"
  fi
done

touch "$ctf_tools_root/.ready"
echo ">> ctf-tools 已准备完成。使用 ./run.sh web，运行环境选择“本地运行”。"
