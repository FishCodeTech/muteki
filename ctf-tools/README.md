# ctf-tools

Muteki 的 macOS 原生 Worker 工具目录。Worker 选择“本地运行”后直接作为 Mac
子进程执行，因此使用 Mac 当前的路由、VPN 和监听端口。

目录内容：

- `bin/`：统一命令入口；
- `python/`、`ruby/`：目录内安全工具环境；
- `wordlists/SecLists/`、`wordlists/kali/rockyou.txt.gz`：字典；
- `knowledges/`：PayloadsAllTheThings、InternalAllTheThings、HackTricks；
- `pocs/`：vulhub、Awesome-POC；
- `nuclei-templates/`：Nuclei 模板；
- `tools/`：JWT、PHP filter chain、GEF、Java 工具和可上传到 Linux 靶机的二进制。

首次准备：

```bash
./ctf-tools/sync-data-from-image.sh muteki-worker:local-arm64-20260908
./ctf-tools/setup.sh
./run.sh web
```

Python 安装只在当前命令中使用清华 PyPI 镜像，不修改全局配置。需要换成其他
临时镜像时使用 `MUTEKI_PYPI_INDEX=<镜像地址> ./ctf-tools/setup.sh`。Agent CLI
直接使用 Mac 上已经登录的现有命令，不在本目录重复安装。

如果 Mac 上已经装好工具，只需刷新统一入口：

```bash
./ctf-tools/setup.sh --link-only
```

运行环境中选择“本地运行”。`./run.sh` 会在 `ctf-tools/.ready` 存在时自动
加载 `ctf-tools/env.sh`。

Linux 专用行为仍有差异，例如 Linux 内核调试、ELF 动态装载和部分原始报文
工具。需要这些能力时仍可切回原有容器模式。
