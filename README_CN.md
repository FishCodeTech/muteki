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
  <a href="https://muteki.codes/"><img src="https://img.shields.io/badge/website-muteki.codes-111111.svg" alt="Website"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-blue.svg" alt="License"></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/backend-Rust-000000.svg?logo=rust&logoColor=white" alt="Rust backend"></a>
  <a href="https://github.com/FishCodeTech/muteki/stargazers"><img src="https://img.shields.io/github/stars/FishCodeTech/muteki?style=social" alt="Stars"></a>
  <a href="https://atomgit.com/FishCodeTech/muteki"><img src="https://atomgit.com/FishCodeTech/muteki/star/badge.svg" alt="AtomGit project mirror"></a>
  <a href="https://github.com/FishCodeTech/muteki/issues"><img src="https://img.shields.io/github/issues/FishCodeTech/muteki" alt="Issues"></a>
  <a href="https://muteki.codes/docs/benchmarks/"><img src="https://img.shields.io/badge/NYU_CTF_Bench_(Python)-200%2F200_solved-brightgreen" alt="Benchmark"></a>
  <a href="https://muteki.codes/docs/runtime-and-configuration/"><img src="https://img.shields.io/badge/engines-9_CLIs-orange" alt="九类 CLI 引擎；实际可用性取决于凭据与运行环境"></a>
</p>

<p align="center">
  <a href="README.md">English</a> · <strong>简体中文</strong> · <a href="https://muteki.codes/">官网</a> · <a href="https://muteki.codes/docs/">在线文档</a> · <a href="https://github.com/FishCodeTech/muteki/releases/tag/v0.5.2">下载 v0.5.2</a> · <a href="CHANGELOG.md">更新日志</a> · <a href="DISCLAIMER.md">免责声明</a>
</p>

---

**無敵 · Project Muteki** 是一款基于 Remix Engineering 的开源 Coding Agent 控制平面。它通过统一的多 Agent 调度与上下文协调机制，自动分配任务、沉淀共享证据并接管运行环境、引擎凭据、并发预算与实时人工干预，让多个异构 Agent 像蜂群一样协同推进目标。

当前兼容 **Claude、Codex、Cursor、Pi、OMP、Kimi、Grok、OpenCode、Droid** 等主流 CLI 引擎，覆盖对话、CTF、渗透测试、SRC 赏金与比赛等工作区，并持续向通用研发与复杂工程协作演进。

- **官网与完整文档**：[muteki.codes](https://muteki.codes/) · [文档中心](https://muteki.codes/docs/)（[快速开始](https://muteki.codes/docs/quick-start/) · [配置手册](https://muteki.codes/docs/configuration/) · [工作区指南](https://muteki.codes/docs/ctf/) · [实战成绩](https://muteki.codes/docs/benchmarks/)）
- **仓库与反馈**：[FishCodeTech/muteki](https://github.com/FishCodeTech/muteki)（镜像 [AtomGit](https://atomgit.com/FishCodeTech/muteki)） · [问题反馈](https://github.com/FishCodeTech/muteki/issues) · [GNU AGPL-3.0](LICENSE)

> ## ⚠️ 免责声明与运行前必读
>
> **任何第三方利用本项目对其他项目、系统或网络实施未授权攻击，均属其自行实施的行为。相关行为及后果由实施者自行承担，与本人及本项目无关。本人及本项目不为该等第三方行为及其后果承担任何责任，也不提供授权或背书。** 完整条款见 [DISCLAIMER.md](DISCLAIMER.md)。
>
> Muteki 会驱动 CLI Agent 执行真实命令、调用工具并访问目标服务，**不承诺隔离恶意题目或不可信目标**。进行攻防或安全类任务时，请在专用、可丢弃的 VPS、VM 或无敏感数据的独立环境中运行；安全漏洞请按 [SECURITY.md](SECURITY.md) 私密报告。

---

## 快速启动

当前公开正式版本为 **[v0.5.2](https://github.com/FishCodeTech/muteki/releases/tag/v0.5.2)**（Rust 服务 + Next.js 前端 + macOS 原生壳）。完整安装说明、Desktop/Web 模式对比、远程访问与 Worker/Planner 配置请查阅 **[官网安装与运行指南](https://muteki.codes/docs/installation/)**。

### macOS 桌面端（Apple Silicon）

从 [v0.5.2 Release](https://github.com/FishCodeTech/muteki/releases/tag/v0.5.2) 下载 `Muteki-0.5.2-arm64.zip`，解压后将 `Muteki.app` 拖入 `/Applications` 打开。首次打开若提示未公证，前往 **系统设置 → 隐私与安全性** 点击 **仍要打开**。

### 独立 Web 包（Linux / macOS）

从 Release 下载对应平台压缩包（如 `Muteki-Web-0.5.2-linux-amd64.tar.gz`，自带 Node 与 Rust 服务），解压启动后访问 `http://127.0.0.1:3101`：

```bash
tar -xzf Muteki-Web-0.5.2-linux-amd64.tar.gz
cd muteki-web-0.5.2
MUTEKI_STATE_DIR="$HOME/.muteki-state" ./bin/node ./launcher.mjs web
```

### Docker Compose（推荐用于服务器 / Windows WSL2）

```bash
MUTEKI_HOST_DATA_ROOT=/opt/muteki/data \
MUTEKI_WEB_IMAGE=ghcr.io/fishcodetech/muteki-web:v0.5.2 \
MUTEKI_WORKER_IMAGE=ghcr.io/fishcodetech/muteki-worker:v0.5.2 \
MUTEKI_WEB_PASSWORD='replace-with-a-strong-password' \
  docker compose -f docker-compose.release.yml up -d --wait
```

### 从源码运行

需要 Node.js 22、支持 edition 2024 的 stable Rust 与基础 C/C++ 构建链（详见 [官网开发文档](https://muteki.codes/docs/development/)）：

```bash
git clone https://github.com/FishCodeTech/muteki.git
cd muteki
npm --prefix apps/frontend ci
node scripts/dev.mjs --web
```

> 启动后在设置页绑定至少一个可用的 CLI Agent 凭据与 Planner 推理模型即可开始任务，详见 [快速开始](https://muteki.codes/docs/quick-start/) 与 [运行环境与 Worker 配置](https://muteki.codes/docs/runtime-and-configuration/)。

---

## 贡献者

<a href="https://github.com/FishCodeTech/muteki/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=FishCodeTech/muteki" alt="公开仓库贡献者">
</a>

### 开发阶段补充贡献者

> 本项目日常开发在内部私有仓库进行，同步发布至公开仓库时无法完整保留部分内部开发阶段的提交者记录，特在此补充列出。

<table>
  <tr>
    <td align="center" valign="top" width="140">
      <a href="https://github.com/FzWjScJ">
        <img src="https://avatars.githubusercontent.com/u/43847366?v=4" width="72" alt="FzWjScJ"><br>
        <sub><b>FzWjScJ</b></sub>
      </a>
    </td>
  </tr>
</table>

## 鸣谢

- 感谢 [c3](https://github.com/Real-C3ngH) 提供的云镜靶场账号，浪费了很多沙砾，疯狂爆米。
- 感谢 [l4n](https://github.com/lancer0rz) 师傅提供的灵感，新增的 Reviewer 让整体的解题效率有了质的提升。
- 感谢 [陈橘墨](https://github.com/Randark-JMT) 师傅提供的靶场资源和 Writeup，用于大量测试和精调。
- ~~感谢山姆奥特曼 不封我号~~ 现在已经被封了，我将永远记住他的名字。
- ~~感谢 Dario Amodei 不封我号~~ 现在已经被封了，我将永远记住他的名字。

## Star History

<a href="https://www.star-history.com/#FishCodeTech/muteki&amp;Date">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=FishCodeTech/muteki&amp;type=Date&amp;theme=dark">
    <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=FishCodeTech/muteki&amp;type=Date">
    <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=FishCodeTech/muteki&amp;type=Date">
  </picture>
</a>
