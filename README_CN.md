<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="./assets/logo-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="./assets/logo-light.png">
    <img alt="Muteki Logo" src="./assets/logo-light.png" width="320">
  </picture>
</p>

<h1 align="center">無敵 · Project Muteki</h1>

<p align="center">
  <strong>The open-source control plane for coding agents.</strong>
  <br />
  <strong>面向 Coding Agents 的开源控制平面 · 多模型异构 Agent 蜂群调度</strong>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-blue.svg" alt="License"></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/backend-Rust-000000.svg?logo=rust&logoColor=white" alt="Rust backend"></a>
  <a href="https://github.com/FishCodeTech/muteki/stargazers"><img src="https://img.shields.io/github/stars/FishCodeTech/muteki?style=social" alt="Stars"></a>
  <a href="https://atomgit.com/FishCodeTech/muteki"><img src="https://atomgit.com/FishCodeTech/muteki/star/badge.svg" alt="AtomGit project mirror"></a>
  <a href="https://github.com/FishCodeTech/muteki/issues"><img src="https://img.shields.io/github/issues/FishCodeTech/muteki" alt="Issues"></a>
  <a href="https://github.com/FishCodeTech/muteki/pulls"><img src="https://img.shields.io/github/issues-pr/FishCodeTech/muteki" alt="PRs"></a>
  <a href="docs/benchmarks.md"><img src="https://img.shields.io/badge/NYU_CTF_Bench_(Python)-200%2F200_solved-brightgreen" alt="Benchmark"></a>
  <a href="docs/runtime-and-configuration.md"><img src="https://img.shields.io/badge/engines-9_CLIs-orange" alt="九类 CLI 引擎；实际可用性取决于凭据与运行环境"></a>
</p>

<p align="center">
  <a href="README.md">English</a> · <strong>简体中文</strong> · <a href="https://github.com/FishCodeTech/muteki/releases/tag/v0.5.2">下载 v0.5.2</a> · <a href="CHANGELOG.md">更新日志</a> · <a href="docs/benchmarks.md">实战成绩</a> · <a href="DISCLAIMER.md">免责声明</a>
</p>

<p align="center">
  <a href="https://www.star-history.com/#FishCodeTech/muteki&amp;Date">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=FishCodeTech/muteki&amp;type=Date&amp;theme=dark">
      <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=FishCodeTech/muteki&amp;type=Date">
      <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=FishCodeTech/muteki&amp;type=Date">
    </picture>
  </a>
</p>

---

**無敵 · Project Muteki** — **The open-source control plane for coding agents.**

Muteki 是一款基于 Remix Engineering 的开源 Coding Agent 控制平面，核心是一套多 Agent 调度与上下文协调方案：自动分配任务、管理上下文并沉淀共享证据，让多个 Agent 像蜂群一样各有分工、围绕同一目标协同推进，同时统一接管运行环境、引擎凭据、并发预算与实时人工干预。项目兼容市面上大部分主流 Agent 引擎，当前可用 Worker 引擎包括 **Claude、Codex、Cursor、Pi、OMP、Kimi、Grok、OpenCode、Droid**，后续将逐步接入更多引擎。

安全攻防是 Muteki 首个经过高强度实战检验的场景，但项目并不局限于单一领域。未来我们将持续迭代，将其打造成面向研发与复杂工程协作的 All-in-One 开源 Agent 控制平面。

> ## ⚠️ 运行前必读
>
> **任何第三方利用本项目对其他项目、系统或网络实施未授权攻击，均属其自行实施的行为。相关行为及后果由实施者自行承担，与本人及本项目无关。本人及本项目不为该等第三方行为及其后果承担任何责任，也不提供授权或背书。** 完整说明见 [免责声明与未授权行为责任声明](DISCLAIMER.md)。
>
> Muteki 会驱动 CLI Agent 执行真实命令、调用工具并访问目标服务，**不承诺隔离恶意题目或不可信目标**。进行攻防/安全类任务时，推荐在专用、可丢弃的 VPS、VM 或无敏感数据的独立环境中运行，详见 [SECURITY.md](SECURITY.md)。

---

## 文档与功能导航

本 README 聚焦**版本选择、模式区别与安装运行**；各工作区功能、实战战绩与深度配置请按需跳转：

| 专题页面 | 内容说明 |
| --- | --- |
| **[实战成绩与评测记录](docs/benchmarks.md)** | RIFFHACK 2026、春秋云镜、Hack The Box、NYU CTF Bench、TSecBench 等真实战绩与参考文献 |
| **[CTF 与比赛工作台](docs/ctf.md)** | 单题蜂群解题、实时干预控制（提示/下达/暂停/冻结）、复盘生成与 CTFd / rCTF / GZCTF 比赛同步 |
| **[渗透测试（Pentest）](docs/pentest.md)** | 授权范围约束、Production Guard / Cronus 护栏与审批、原始证据链与结构化渗透报告 |
| **[SRC 赏金工作台](docs/src.md)** | 长期研究主对话、资产/来源/流量分析、发现证据绑定与报告/奖金台账 |
| **[对话工作区与扩展插件](docs/chat-and-extensions.md)** | 通用 Coding Agent 对话、多模型子 Agent 编排与 Agent Plugins 1.0.0 插件管理 |
| **[运行环境、Worker 配置与远程 Runner](docs/runtime-and-configuration.md)** | 九大 CLI 引擎、本地/容器/远程节点接入、Pi + Ollama 配置与常见问题排查 |
| **[更新日志](CHANGELOG.md)** | 公开版本发布记录、修复说明、升级路径与验证范围 |

---

## 下载哪个 Release 版本？

当前公开正式版本为 **[v0.5.2](https://github.com/FishCodeTech/muteki/releases/tag/v0.5.2)**（主应用为 Rust 服务 + Next.js 前端 + macOS 原生壳，已从旧版 Python/Electron `v0.4.7` 迁移）。正式下载与源码请以公开仓库 [FishCodeTech/muteki](https://github.com/FishCodeTech/muteki) 为准（镜像仓库：[AtomGit](https://atomgit.com/FishCodeTech/muteki)）。

| 你的环境 / 使用方式 | 应下载或拉取的产物 | 适用说明 |
| --- | --- | --- |
| **macOS（Apple Silicon / M 系列）桌面端** | [`Muteki-0.5.2-arm64.zip`](https://github.com/FishCodeTech/muteki/releases/tag/v0.5.2) | 原生桌面 App，开箱即用，个人 Mac 首选 |
| **macOS（Apple Silicon）独立 Web 版** | [`Muteki-Web-0.5.2-darwin-arm64.tar.gz`](https://github.com/FishCodeTech/muteki/releases/tag/v0.5.2) | 自带 Node 与 Rust 服务，命令行启动、浏览器访问 |
| **Linux x86_64（`amd64`）独立 Web 版** | [`Muteki-Web-0.5.2-linux-amd64.tar.gz`](https://github.com/FishCodeTech/muteki/releases/tag/v0.5.2) | Linux 服务器/本机直接解压运行，无需装 Node/Rust |
| **Linux ARM64（`arm64`）独立 Web 版** | [`Muteki-Web-0.5.2-linux-arm64.tar.gz`](https://github.com/FishCodeTech/muteki/releases/tag/v0.5.2) | ARM64 Linux 直接解压运行，无需装 Node/Rust |
| **Docker Compose 部署（`amd64` / `arm64`）** | `ghcr.io/fishcodetech/muteki-web:v0.5.2`<br>`ghcr.io/fishcodetech/muteki-worker:v0.5.2` | Web 与独立 Runner 分离，适合服务器与容器隔离 Worker |
| **Windows** | 在 WSL2 或 Linux 主机运行 Linux Web 包 / Docker Compose | 当前无原生 Windows 桌面包 |

> **Release 页面其他文件说明：**
>
> - `*.sha256`：对应压缩包的 SHA-256 校验文件。
> - `muteki-0.5.2-upgrade.tar.gz` 与 `release-manifest.json`：供旧版托管安装执行 `muteki upgrade` 时自动下载校验，全新安装无需手动下载。

---

## Desktop 模式与 Web 模式的区别

由于项目为个人项目维护，精力有限，暂时无其他平台的桌面端和windows的适配。windows平台推荐使用虚拟机/docker。

| 对比维度 | Desktop 模式（`Muteki.app`） | Web 模式（独立 Web 包 / Docker / 源码） |
| --- | --- | --- |
| **支持平台** | macOS Apple Silicon（`arm64`） | macOS `arm64`、Linux `amd64` / `arm64`、Docker Compose |
| **启动与访问** | 双击 `Muteki.app` 启动，内置前端、Rust 服务与本机 Runner | 命令行或 Docker 启动，浏览器访问 `http://127.0.0.1:3101`（非本机暴露需设密码；HTTPS 按需自配） |
| **界面与原生能力** | 原生 AppKit 窗口壳、系统通知、内置桌面浏览器面板与本地预览 | 浏览器访问；提供带隔离代理的本地 iframe 预览，适合无桌面服务器与远程使用 |
| **默认可见工作区** | 左侧栏默认显示全部工作区（对话、CTF、渗透测试、SRC 赏金、比赛） | 默认首页显示核心安全任务；可在 **设置 → 外观配色 → 工作区模式** 开启全部工作区 |
| **数据目录** | `~/Library/Application Support/Muteki/data`（首次启动自动备份迁移旧版稳定数据） | 默认 `<安装目录>/data/state` 或 `MUTEKI_STATE_DIR`；Docker 挂载 `MUTEKI_HOST_DATA_ROOT` |
| **Worker 执行环境** | 宿主机 CLI（Local）、本机 Docker Desktop 容器、远程 Runner | 宿主机 CLI（原生 Web 包）、独立 Runner 容器（Compose）、远程 Runner |
| **升级方式** | 应用内更新或下载新版 `.zip` 替换 | 托管 Web 执行 `muteki upgrade v0.5.2`；Docker 排空活动 Worker 后切换镜像版本 |

---

## 安装与运行

### 一、Desktop 模式运行（macOS Apple Silicon）

1. 在 [v0.5.2 Release](https://github.com/FishCodeTech/muteki/releases/tag/v0.5.2) 下载 `Muteki-0.5.2-arm64.zip`（可选校验 `shasum -a 256 -c Muteki-0.5.2-arm64.zip.sha256`）。
2. 解压后将 `Muteki.app` 拖入 `/Applications` 并双击打开。
3. 当前正式包已使用 Developer ID Application 签名但**尚未公证**：首次打开若系统拦截，前往 **系统设置 → 隐私与安全性** 点击 **仍要打开** 即可。

---

### 二、Web 模式运行

Web 模式提供三种运行方式，按你的环境任选其一即可。

#### 方式 1：独立 Web 发布包（最快，无需本机安装 Node / Rust）

以 Linux `amd64` 为例（`linux-arm64` / `darwin-arm64` 替换对应压缩包名即可）：

```bash
# 1. 解压下载好的 Web 发布包（包内自带 bin/node 与 bin/muteki-server）
tar -xzf Muteki-Web-0.5.2-linux-amd64.tar.gz
cd muteki-web-0.5.2

# 2. 启动本机 Web 服务（建议显式指定数据目录）
MUTEKI_STATE_DIR="$HOME/.muteki-state" ./bin/node ./launcher.mjs web
```

浏览器打开 **`http://127.0.0.1:3101`**（内部 Rust API 默认监听 `127.0.0.1:8100`，由前端代理转发）。

- **Linux 依赖提示**：原生 Linux Web 包需要兼容的 glibc 与 C++ 运行库；Ubuntu 24.04 若提示缺少共享库，执行 `sudo apt-get install libstdc++6`（可用 `ldd bin/muteki-server` 检查）。
- **局域网 / 服务器远程访问**：监听非本机地址（如 `0.0.0.0`）时需设置访问密码（`MUTEKI_WEB_PASSWORD` 或先在设置页配置密码；后端 API 仍仅监听 loopback）：

```bash
MUTEKI_UI_HOST=0.0.0.0 \
MUTEKI_WEB_PASSWORD='replace-with-a-strong-password' \
MUTEKI_STATE_DIR="$HOME/.muteki-state" \
  ./bin/node ./launcher.mjs web
```

启动后通过 `http://<服务器地址>:3101` 访问。不强制 HTTPS——如需 HTTPS，可自行前置 Nginx / Caddy / Traefik 等反向代理，或直接成对传入可选环境变量 `MUTEKI_WEB_TLS_CERT` 与 `MUTEKI_WEB_TLS_KEY` 启用内置 TLS。常用端口环境变量：`MUTEKI_UI_PORT`（默认 `3101`）、`MUTEKI_API_PORT`（默认 `8100`）。

#### 方式 2：Docker Compose（推荐用于服务器与容器化 Worker）

Compose 模式将 **Web** 与 **Runner** 拆分为两个容器（仅 Runner 挂载 Docker socket），并直接使用官方发布的 `amd64` / `arm64` 多架构镜像。你可以使用仓库根目录或 Web 发布包内的 [`docker-compose.release.yml`](docker-compose.release.yml)：

```bash
MUTEKI_HOST_DATA_ROOT=/opt/muteki/data \
MUTEKI_WEB_IMAGE=ghcr.io/fishcodetech/muteki-web:v0.5.2 \
MUTEKI_WORKER_IMAGE=ghcr.io/fishcodetech/muteki-worker:v0.5.2 \
MUTEKI_WEB_PASSWORD='replace-with-a-strong-password' \
  docker compose -f docker-compose.release.yml up -d --wait
```

启动后浏览器打开 **`http://127.0.0.1:3101`**。默认仅绑定宿主机 `127.0.0.1`（方便自行前置反向代理配置 HTTPS）；如需直接对外监听，附加 `MUTEKI_WEB_BIND=0.0.0.0` 即可。升级或回滚前请先排空活动 Worker 并保留数据目录与旧镜像 digest。

#### 方式 3：从源码运行 Web / 开发热预览

需预先安装 **Node.js 22 / npm**、**支持 edition 2024 的 stable Rust 工具链**（`sccache` 可选，安装后自动启用）及 C/C++ 工具链、pkg-config（依赖清单见 [构建工作流](.github/workflows/build-packages.yml)）：

```bash
git clone https://github.com/FishCodeTech/muteki.git
cd muteki
npm --prefix apps/frontend ci

# 构建并启动 Web（http://127.0.0.1:3101）
node scripts/dev.mjs --web

# 或启动前端热预览（http://127.0.0.1:3103）
node scripts/dev.mjs --hot --web
```

macOS 桌面源码开发（`node scripts/dev.mjs`）与本地打包步骤见 [English README](README.md#development) 及 [构建工作流](.github/workflows/build-packages.yml)。

---

### 三、从旧版本升级（`v0.4.7` / `v0.5.0` / `v0.5.1` → `v0.5.2`）

- **已有托管 Web 安装**：直接执行 `muteki upgrade v0.5.2`。升级器会自动校验兼容桥与当前平台原生包，保留原有数据目录以便回滚。
- **Docker Compose 部署**：先排空活动 Worker，再将 `MUTEKI_WEB_IMAGE` 与 `MUTEKI_WORKER_IMAGE` 切换至 `v0.5.2` 并重新拉起服务。

---

## 启动后如何开始？

1. **配置 Agent 凭据与 Worker**：打开对应工作区的 Worker 设置（或 **设置 → Agents**），确认至少一个 CLI 引擎连通并通过自检；本地运行可复用宿主机已有的 CLI 登录，容器运行需绑定可注入凭据。
2. **配置 Planner 推理模型**：在“推理模型”中为 Planner 选择可用模型端点并测试通过。
3. **选择工作区开始任务**：按你的场景前往 [CTF](docs/ctf.md)、[渗透测试](docs/pentest.md)、[SRC 赏金](docs/src.md) 或 [对话与插件](docs/chat-and-extensions.md) 指南；多节点/容器/Ollama 配置与排错见 [运行环境、Worker 配置与远程 Runner](docs/runtime-and-configuration.md)。

---

## 项目与参与

- **公开仓库**：[FishCodeTech/muteki](https://github.com/FishCodeTech/muteki)（源码、正式 Release、Star 与公开 Issue） · 镜像：[AtomGit](https://atomgit.com/FishCodeTech/muteki)
- **代码导航**：`crates/server/`（Rust API / 调度 / Runner）、`crates/desktop/`（macOS 原生壳）、`apps/frontend/`（Next.js 前端）、`cmd/runtime-agent/`（容器进程代理）、`docker/`（Web 与 Worker 镜像配方）
- **问题反馈**：缺陷与文档问题请在 [GitHub Issues](https://github.com/FishCodeTech/muteki/issues) 提交复现步骤、版本号与脱敏日志；安全漏洞请按 [SECURITY.md](SECURITY.md) 私密报告
- **开源许可**：[GNU AGPL-3.0](LICENSE)（外部 Agent CLI 与模型服务适用各自条款与计费规则）

## 贡献者

<a href="https://github.com/FishCodeTech/muteki/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=FishCodeTech/muteki" alt="公开仓库贡献者">
</a>

### 开发阶段补充贡献者

<table>
  <tr>
    <td align="center" valign="top" width="140">
      <a href="https://github.com/FzWjScJ">
        <img src="https://avatars.githubusercontent.com/u/43847366?v=4" width="72" alt="FzWjScJ"><br>
        <sub><b>FzWjScJ</b></sub>
      </a><br>
      <sub>Bug 反馈 · Pull Request</sub>
    </td>
  </tr>
</table>


## 鸣谢

- 感谢 [c3](https://github.com/Real-C3ngH) 提供的云镜靶场账号，浪费了很多沙砾，疯狂爆米。
- 感谢 [l4n](https://github.com/lancer0rz) 师傅提供的灵感，新增的 Reviewer 让整体的解题效率有了质的提升。
- 感谢 [陈橘墨](https://github.com/Randark-JMT) 师傅提供的靶场资源和 Writeup，用于大量测试和精调。
- ~~感谢山姆奥特曼 不封我号~~ 现在已经被封了，我将永远记住他的名字。
- ~~感谢 Dario Amodei 不封我号~~ 现在已经被封了，我将永远记住他的名字。
