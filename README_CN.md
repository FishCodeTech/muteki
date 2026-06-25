<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="./assets/logo-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="./assets/logo-light.png">
    <img alt="Muteki Logo" src="./assets/logo-light.png" width="320">
  </picture>
</p>

<h1 align="center">無敵 · Project Muteki</h1>

<p align="center">
  <strong>多模型异构 AI Agent 蜂群 · 自主攻防安全自动化</strong>
</p>

<p align="center">
  <a href="https://github.com/FishCodeTech/muteki/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-blue.svg" alt="License"></a>
  <a href="https://www.python.org/"><img src="https://img.shields.io/badge/python-≥3.13-3776AB.svg?logo=python&logoColor=white" alt="Python"></a>
  <a href="https://github.com/FishCodeTech/muteki/stargazers"><img src="https://img.shields.io/github/stars/FishCodeTech/muteki?style=social" alt="Stars"></a>
  <a href="https://github.com/FishCodeTech/muteki/issues"><img src="https://img.shields.io/github/issues/FishCodeTech/muteki" alt="Issues"></a>
  <a href="https://github.com/FishCodeTech/muteki/pulls"><img src="https://img.shields.io/github/issues-pr/FishCodeTech/muteki" alt="PRs"></a>
  <img src="https://img.shields.io/badge/NYU_CTF_Bench-200%2F200_solved-brightgreen" alt="Benchmark">
  <img src="https://img.shields.io/badge/engines-Claude_Code_%7C_Codex_%7C_Cursor-orange" alt="Engines">
</p>

<p align="center">
  <a href="README.md">English</a> · <strong>简体中文</strong>
</p>

---

这是一款 **真正意义上的开源的多模型 CTF 求解 AI agent 蜂群。** 目标就是成为如项目名称，**無敵 · Project Muteki**

项目核心是实现了一套ai agent的调度方案，自动、智能化协调控制每个agent的上下文，像蜂群一样，各有分工，但都是为了完成最终的目标，目前只支持cursor、codex、claude code的指挥和下发。未来将持续更新迭代支持更多种类的cli agent。

Muteki就是为了解决单一ai agent在解决一个目标是极其容易陷入一个点死循环，无法自拔，无法完成最终的目标，并且单一agent效率极低，我设计了一套架构来解决这个问题，他可能不是最完美的，我将继续不断迭代升级。

ctf只是一个最基础的功能，核心架构是为了满足实现各类场景下的多agent协同目标驱动，经过实测，该可以独立自动化完成渗透测试、代码审计、ctf题解，网络安全等。

> ## ⚠️ 运行信任边界 —— 运行前必读
>
> Muteki 是**攻击性安全自动化工具**。它驱动 CLI agent 执行命令、调用安全工具、访问目标服务;
> **它不承诺隔离恶意 challenge**。
>
> 推荐**只在专用、可丢弃的环境里运行** —— 专用 VPS、throwaway VM,或无敏感数据的独立机器。不要在
> 你的主力工作机、共享主机或生产环境运行。详见 [SECURITY.md](SECURITY.md)。
>
> 当然我平时都在自己的电脑直接跑，因为配环境比较方便（

---

## 能力如何？

在RIFFHACK2026 3小时全自动无人工接管，速通ak全部题目。获得第八名。

![image-20260624162932292](./assets/image-20260624162932292.png)

春秋云镜渗透测试靶场blackmaze，三个月0解，muteki 2小时速通一血（为什么平台显示39小时因为期间涉及到各种调试测试多flag的模式支持，所以浪费时间较多，实际解题时间仅花费2小时。）。

![ee318ffa895e4b2ffd6df67da6c15f90](./assets/ee318ffa895e4b2ffd6df67da6c15f90.png)

![image-20260624163414544](./assets/image-20260624163414544.png)

春秋云镜全徽章场景ak。

hackthebox全种类 insane、hard难度ak。

nyuctf benchmark全题目测评成绩，可看文章结尾

更多你们知道和不知道的各种比赛的一血、高分，均有muteki的身影出现，在此不一一赘述。

总之经过为期一个月的工程化优化，架构能力调教。bug修复，本项目正式开源，没有欺骗star，没有吹逼文案，没有打击你们的自信，没有子群，没有社区，没有骗钱，没有付费，没有营销，直接开源共享。

欢迎使用并一同建设升级，遇到的任何问题请随时提issue，欢迎加入交流群。我们共同建设世界最强的ctf agent。

![mmqrcode1782307542963](./assets/mmqrcode1782307542963..png)

---

## 架构

無敵让一群异构的编码 Agent（Claude Code / Codex / cursor-agent）扑同一道题，在一张**共享黑板**上协作：谁发现的事实大家都能用，谁走过的死路大家都不再试，而 flag 只有**逐字出现在真实执行输出里**才被接受。核心不是「换个更强的脑子」，而是 **异构 + 共享证据 + 溯源闸门**。

而 worker 是怎么把数据交到平台、又怎么看到队友进展的？**全靠每个 worker 内置的 `muteki-blackboard` skill**——这是 worker 与黑板之间唯一的数据通道。

详细架构说明，请参考：[docs/工作原理.md](docs/工作原理.md)

项目秉承着 less is more的原则，不注入任何安全工具、安全知识，开放网络，让worker自由发挥，自由编写和自由安装依赖脚本。

![image-20260624164618066](./assets/image-20260624164618066.png)

> *web 指挥台:左侧 run 列表、中间协调器对话流、右侧带 per-worker 状态的实时 run 控制面板。*

### 一张图看懂：解题阶段 × agent 循环

外层 `①②③④` 是一次 run 的四个阶段，内层 `(1)~(5)` 是阶段 ③ 每一拍的协作循环。难题的功夫全在 ③ 这个圈里，而圈里 worker ↔ 黑板的每一次读写都走 `muteki-blackboard` skill。

![1782305107059](./assets/1782305107059.png)

`**(1)→(5)` 一圈就是无敵的核心**：协调器读黑板 → Reason 规划下一步 → intent 上黑板 → worker 各认一个跑真实命令 → **经 skill 把结果写回黑板（flag 还要过闸门）**，然后再读……每 2 秒转一圈，难题就是这样一圈圈把证据攒厚的。外层 `①②③④` 则是一次 run 的完整时间线。


| 阶段            | 什么时候进                 | 干什么                                   | 产出                    |
| ------------- | --------------------- | ------------------------------------- | --------------------- |
| **① 准备**      | run 一开始               | 建黑板、暂存附件、探活引擎、装好 skill、（容器模式）起容器+反向连接 | 空黑板 + 可用引擎 + 接好通道     |
| **② 侦察 Race** | 仅冷启动（复盘已解的题跳过）        | 多引擎并行单发扑整题，做广度侦察                      | flag（→快路径）或一批 fact    |
| **③ 协调主循环**   | 侦察没直接解出时              | `(1)~(5)` 不断转圈，随证据扩张 swarm            | 黑板持续长大，直到攒够 flag      |
| **④ 收尾**      | 攒够 flag / 操作员停 / 预算耗尽 | 落 winner、释放认领、发终态事件、清扫                | RUN_FINISHED + 可复盘的黑板 |


为了防止muteki在做单一任务时进入死循环，我们设定了一个review机制，当muteki在执行任务时，会定期进行review，review机制会检查已经记录的事实并验证，然后随时及时纠正。

---

## 快速开始

```bash
# 1. 引导:装依赖 + 跑快速测试套件
./init.sh

# 2a. web 指挥台 —— FastAPI 后端(:8000)+ Next UI(:3001)
./run.sh web
#     只起后端:  ./run.sh web --backend-only
```

仓库根目录的 `.env` 会被自动加载(从 `.env.example` 复制);shell 导出的变量始终优先。配置通过`MUTEKI_*` 环境变量。

推荐设置项：

```
MUTEKI_DEEPSEEK_API_KEY=sk-xxxx
```

主要是核心是用于设置Reason 规划器 来规划整套agent的凭据，你也可以换成其他的任意端点，和在前端设置中配置模型内容。默认是deepseek，因为相比较来说性价比较高。

不设置主要影响在reason规划器不会自主规划题目和总结进展。

---

## 环境要求

- `**[uv](https://docs.astral.sh/uv/)**` —— Python 工具链与运行器
- **Python ≥ 3.13**(在 `pyproject.toml` 声明;`uv` 负责管理)
- **Node.js** —— 仅 web UI 需要(`apps/web/ui`,Next.js)
- **Go ≥ 1.26** —— 仅构建 worker 镜像里的容器内 supervisor 时需要
- **Docker** —— 仅 `container` worker 后端 / 构建 worker 镜像时需要
- 你打算用的**引擎 CLI**,需在 `PATH` 上(见下)
- 当前项目仅在macos上进行过测试，未在windows上进行测试，请酌情处理。

### 专有引擎 CLI

Muteki **套壳调用**下面三个闭源 agent CLI;装好并认证你想用的那些。它们各有自己的 license, 且会向各自的厂商回传数据:


| 引擎       | CLI                                  | 厂商        | 凭据                                  |
| -------- | ------------------------------------ | --------- | ----------------------------------- |
| `claude` | `@anthropic-ai/claude-code`          | Anthropic | OAuth token(`claude setup-token`)   |
| `codex`  | `@openai/codex`                      | OpenAI    | `~/.codex/auth.json`(`codex login`) |
| `cursor` | `cursor-agent`(`cursor.com/install`) | Cursor    | API key                             |


至少需要其中一个才能跑。除这三个外,还可在 worker profile 里配置**自定义 OpenAI 兼容端点**
(`base_url` + key)—— 适合自托管或第三方模型。凭据从 macOS Keychain / 环境读取并注入到 worker
环境;见 [凭据](#凭据) 与 [SECURITY.md](SECURITY.md)。

---

## 凭据

三个agent 凭据会跟随着网页设置中进行配置，走本地模式一下可以不需要配置，只需要保证你自己运行cli的时候，订阅可用即可。

剩余情况一般用于配置远程环境、容器环境，需要涉及到容器的凭据信息。

![image-20260624184241572](./assets/image-20260624184241572.png)

容器模式下，或者其他情况你如果需要使用key，那么可以参考下面这种方式进行配置


| 引擎       | 账户目录里的文件                  | 怎么拿到                                  |
| -------- | ------------------------- | ------------------------------------- |
| `claude` | `CLAUDE_CODE_OAUTH_TOKEN` | `claude setup-token`                  |
| `codex`  | `codex-home/auth.json`    | `codex login`(拷 `~/.codex/auth.json`) |
| `cursor` | `CURSOR_API_KEY`          | cursor.com → API key                  |
| 自定义端点    | `API_KEY` + `BASE_URL`    | 任意 OpenAI 兼容厂商                        |


![image-20260624184417919](./assets/image-20260624184417919.png)

在保存后你可以随时进行点击保存并测试。

**local vs container 模式:**

- `**container`** 模式下账户是**必须的** —— 宿主登录不会挂进容器，会通过命令注入和文件挂在的方式将凭据挂到容器里
- `**local`** 模式下,若没注册账户,worker 会继承宿主 CLI 已有的登录，当然你也可以手工配置。

DeepSeek 推理模型(协调器用,不是 worker 引擎)单独通过 `.env` 里的 `MUTEKI_DEEPSEEK_API_KEY`配置。

![image-20260624184600517](./assets/image-20260624184600517.png)

凭据信任模型见 [SECURITY.md](SECURITY.md)。

### Worker 镜像(容器后端)

为了满足环境隔离、容器化的需要，我这边同时提供了一套容器模式。不过这个容器模式下测试的不够充足，不保证随时可用。

`container` 后端把 worker 跑在**一个通用 Kali 镜像**里(不再分各种模板/recipe),内含全套 CTF工具链 + 离线知识库 + 引擎 CLI + supervisor。**镜像里不烤任何凭据** —— 凭据在运行时注入。

**拉取预构建镜像(推荐):**

```bash
docker pull snowywar/muteki-worker:latest        # 或钉版本: :0.2.3
```

代码默认用 `snowywar/muteki-worker:latest`(已发布的镜像);

用 `MUTEKI_WORKER_IMAGE` 环境变量覆盖成别的名字/tag(例如 `MUTEKI_WORKER_IMAGE=snowywar/muteki-worker:0.2.3`)。

**或从源码构建:**

```bash
./docker/worker/build.sh                          # → muteki-worker:0.2.3 + muteki-worker:latest
./docker/worker/build.sh snowywar/muteki-worker 0.2.0   # 自定义 repo + 版本(用于 push)
```

镜像很大(~19.7 GB:Kali headless + ghidra + 经 conda 装的 SageMath + 离线知识库)。

---

## 部署

由于目前暂未实现鉴权逻辑，所以暂不推荐部署在公网vps服务器上。在做了在做了

最佳使用实践比较推荐本地直接启动，登陆和安装好相关的worker，随时启动。

```bash
./run.sh web
# 访问 localhost:3001
```

你也可以去启动容器模式进行使用，但是这块未经过最佳验证，可能会有暗藏的坑，欢迎广大player一起来测试。

---

## 最佳实践

1. 打开项目后会进入这样的页面
  ![image-20260624192301784](./assets/image-20260624192301784.png)
2. 优先点开左下角设置页面，勾选你的出战引擎，以及配置你的worker模型
  模型选择这块，如果你已经获得了cyber、cvp的认证，我推荐你使用opus4.8和gpt-5.5，如果没有，个人推荐使用gpt5.4,opus4.6。cursor个人推荐compose2.5，在简单题上有奇效。
   当然，你也可以通过自定义的baseurl来配置自定义的国产模型。（deepseek、kimi、glm）。
   ![image-20260624192335651](./assets/image-20260624192335651.png)
3. 运行环境推荐选择本地，如有特殊需求可以选择容器，容器会提醒你配置相关的凭据，这块请自行配置，你可以通过点击测模型来测试是否正确工作，测试方式会调用agent并让模型重复 ok。
  ![image-20260624192439759](./assets/image-20260624192439759.png)
4. 接下来可以详细配置你的 worker情况，推荐按照图中的方式进行配置。
  起始worker数量表示竞速阶段的数量，数量跟随着你的引擎数，会三个agent引擎同时进行，直至flag解出或者题目超时。用于解决简单题的快速抢血和快速解答。
   最大worker数推荐保留5-6个左右，因为对于web题目来讲，过多的worker可能会造成ddos的情况。
   ![image-20260624192517250](./assets/image-20260624192517250.png)
5. 推荐配置和测联通这块推理模型，更好的规划和把控题目节奏。
  ![image-20260624192921371](./assets/image-20260624192921371.png)
6. 全部配置完成后可以点击运行自检，没什么问题就可以保存并关闭设置页面了。
7. 题目解题的推荐prompt方式如下：
  1. 说明题目描述，题目类型，题目名称，网站地址，flag格式
  2. 同时前端页面支持复制粘贴和上传文件，可直接进行附件题目进行上传。
  3. 图中联网代表是否开始agent自身网络搜索功能，默认开启，关闭是用于数据测评。
  4. 本地容器按钮不用管，这是跟设置功能一支，后续可以删除。高级中可以手工指定flag格式，和一些简单配置，可以忽略。
    ![image-20260624193322483](./assets/image-20260624193322483.png)
    ![image-20260624193441654](./assets/image-20260624193441654.png)
8. 运行后，会初始化半分钟左右，因为初始化涉及到文件初始化，配置文件初始化，这块会慢一些。就会进入到正式的页面了
  ![image-20260624193525341](./assets/image-20260624193525341.png)
9. ![image-20260624194842261](./assets/image-20260624194842261.png)
10. 题目出后，你可以通过右上角的x来发送指定flag的误报，这样会拉起worker继续重新解题，你可以点击生成复盘来直接生成flag。
11. 其他页面用于可查看或者自行探索，请尽情尝试或使用。

---

## 测评

Muteki 在 **NYU CTF Bench** `test` 集(CSAW 2017–2023,共 200 题)上做了全量评测。结果如下：

### 能力评测(宿主工具链)

本次测评中，未预装任何安全工具、逆向工具，仅准备了一台x86的ubuntu24 vps作为评测环境。

覆盖全部六大类、横跨 CSAW 全难度段的200道题目,单题预算 30 分钟:


| 指标               | 值                                |
| ---------------- | -------------------------------- |
| 解出               | **200 / 200 = 100%**             |
| 高难/Expert 段(难度榜) | **36 / 36 全部解出**                 |
| 累计 token         | ~370 M                           |
| 累计成本             | ~$214                            |
| 解题用时             | 中位 ~2–4 分钟(最快 22 s)              |
| 各引擎 winner 数     | cursor 80 · claude 75 · codex 45 |


三引擎盲区不重叠 —— 合起来六大类全胜,含 CSAW 榜首级 V8 引擎 pwn、Windows 远程提权、16GB 磁盘镜像取证等高难题型。完整报告:
`[eval_nyu/_reports/FINAL_eval_report.md](eval_nyu/_reports/FINAL_eval_report.md)`,
逐题明细见 `[eval_nyu/_reports/RESULTS.md](eval_nyu/_reports/RESULTS.md)`。

> 引擎/模型版本会随 CLI 更新变动(worker 套壳跑各 CLI 自己的默认模型:Claude Opus 4.7 / GPT-5.5 / Cursor)。
> 请把这些数字当作能力快照,而非排行榜结论。

---

## 仓库结构


| 路径                   | 内容                                                                                |
| -------------------- | --------------------------------------------------------------------------------- |
| `muteki/`            | 核心:`swarm/`(协调器)、`solver/`(CLI driver、gate、控制平面)、`models/`、`platform/`、`sandbox/` |
| `apps/web/`          | FastAPI 后端(`server.py`)+ Next.js 操作者 UI(`ui/`)                                    |
| `apps/tui/`          | Textual TUI 指挥台 （未完工）                                                             |
| `cmd/runtime-agent/` | 容器内的 Go supervisor(反向连接控制器)                                                       |
| `docker/worker/`     | worker 镜像(Dockerfile、构建脚本、工具感知地图)                                                 |
| `muteki_kit/`        | 小型 SDK helper(如 flag 提交)                                                          |
| `scripts/`           | eval / 回测 harness                                                                 |
| `docs/`              | eval 报告 + 开源就绪审查;设计文档在 `docs/internal-design/`                                    |


### 单个 runner（题目）的工作目录

每发起一道题就是一个 **run**。它在 `sessions/` 下的工作路径和结构如下——`host` 与 `container` 两种后端的 worker 看到的是同一套布局：

```
sessions/
├── run-XXXX.jsonl              # 这道题的「事件流」：SSE 回放 / 断点续传的真相源（一行 = 一个事件）
├── run-XXXX/                   # 这道题的工作根目录
│   ├── uploads/                # 网页上传的原始题目文件（未加工；加工后进 workspace/inputs）
│   └── workspace/              # 这道题的工作区
│       ├── inputs/             # 不可变的题目输入（内容寻址 CAS）
│       │   ├── objects/        #    CAS 对象库（按 sha256 分桶存）
│       │   └── by-name/        #    按原始文件名 → 对象的符号链接
│       ├── shared/             # worker 之间共享的产出物（CAS）
│       │   ├── objects/        #    CAS 对象库
│       │   ├── links/          #    按名字 → 对象的符号链接
│       │   └── index.jsonl     #    共享产物索引（可重建的物化视图）
│       ├── graph/
│       │   └── shared_graph.db #    ★ 共享黑板：事件溯源 SQLite，唯一事实来源（facts/intents/dead-ends/...）
│       ├── arts/               # 工件库：工具输出 / 转录快照（<hex>.txt，按 artifact_id 寻址、可 peek 回看）
│       ├── workers/            # 每个 worker 各自的 cwd（scratch）
│       │   └── cli-codex-2/    #    一个 worker 的工作目录（agent 临时文件 + 指向 inputs/shared 的相对符号链接）
│       ├── homes/              # 每个 worker 的隔离 HOME（容器模式尤其需要）
│       ├── final/              # 最终产物
│       ├── tmp/                # 临时目录
│       ├── logs/               # 日志
│       ├── manifest.json       # 工作区清单：拓扑 + inputs 列表 + runtime 元数据
│       ├── winner.json         # 胜出 worker 的续接句柄（解出后追问 / 写 writeup / 复盘用）
│       ├── writeup.md          # （解出后生成的）题解，可选
│       └── .muteki_board.md    # 黑板快照：写给 worker 直接读的 Markdown 版
│
├── _secrets/accounts/<id>/     # 凭据账号库（目录 0700 / 文件 0600，从不进镜像或 prompt）
├── _worker_config.json         # 全局 worker 配置（引擎名册 / profile）
└── _rail_meta.json             # 导轨元数据（run 列表的名字 / 顺序）
```

几个要点：

- `**run-XXXX.jsonl`（事件历史）** 和 `**run-XXXX/`（干活的文件）** 用同一个 run id 关联：前者能重放给前端，后者是真正落盘的工作区。
- `**inputs/` 和 `shared/` 都是内容寻址（CAS）**：同一份文件只存一份，worker 目录里全是相对符号链接——所以 `workers/` 可随用随删而不丢数据。
- `**graph/shared_graph.db` 是核心**：黑板的全部状态都在这；worker 通过 `muteki-blackboard` skill 读写它。
- **收尾只清 `workers/` 下非 winner 的 scratch**，`shared/`、`graph/`、`arts/`、`final/`、`winner.json` 都保留，所以一道题跑完后仍可完整复盘。

---

## 测试

```bash
uv run pytest                              # Python 套件(无 key 时 live 测试自动跳过)
go test -C cmd/runtime-agent ./...         # Go supervisor(module 在 cmd/runtime-agent/ 下)
( cd apps/web/ui && npx tsc --noEmit )     # UI 类型检查
```

---

## 后续 TODO

- [ ] 添加鉴权逻辑
- [ ] 完整优化测试容器模式
- [ ] 持续迭代升级webui体验
- [ ] 更多agent worker类型支持，如pi、zai、opencode等。
- [ ] TUI模式
- [ ] 全自动爬ctf平台题目，自动解题，自动提交，自动生成报告功能。

---

## 鸣谢

感谢 [c3](https://github.com/Real-C3ngH) 提供的云镜靶场账号，浪费了很多沙砾，疯狂爆米。

感谢 [l4n](https://github.com/lancer0rz) 师傅提供的灵感，新增的reviwer让整体的解题效率有了质的提升。

感谢 [陈橘墨](https://github.com/Randark-JMT) 师傅提供的靶场资源和writeup，用于大量测试和精调。

~~感谢山姆奥特曼 不封我号~~ 现在已经被封了，我将永远记住他的名字。

感谢Dario Amodei 不封我号

---

## 许可证

[GNU AGPL-3.0](LICENSE)

---

## 参考文献

本项目的设计和评测参考了以下学术工作:

1. **NYU CTF Bench: A Scalable Open-Source Benchmark Dataset for Evaluating LLMs in Offensive Security**
  Minghao Shao, Sofija Jancheska, Meet Udeshi, Brendan Dolan-Gavitt, et al. *NeurIPS 2024 Datasets & Benchmarks Track*.
   [arXiv:2406.05590](https://arxiv.org/abs/2406.05590)
2. **Teams of LLM Agents can Exploit Zero-Day Vulnerabilities**
  Richard Fang, Rohan Bindu, Akul Gupta, Daniel Kang. *EACL 2026*.
   [Paper](https://aclanthology.org/2026.eacl-long.2.pdf)
3. **D-CIPHER: Dynamic Collaborative Intelligent Multi-Agent System with Planner and Heterogeneous Executors for Offensive Security**
  Chenhui Zhang, et al. 2025.
   [arXiv:2502.10931](https://arxiv.org/abs/2502.10931)
4. **HackSynth: LLM Agent and Evaluation Framework for Autonomous Penetration Testing**
  Lajos Muzsai, David Imolai, András Lukács. 2024.
   [arXiv:2412.01778](https://arxiv.org/abs/2412.01778)
5. **CTFAgent: An LLM-powered Agent for CTF Challenge Solving**
  Jiaze Sun, et al. *Computers & Security*, 2025.
   [ScienceDirect](https://doi.org/10.1016/j.cose.2025.104488)
6. **Co-RedTeam: Orchestrated Security Discovery and Exploitation with LLM Agents**
  Jiahao Zhu, et al. 2025.
   [arXiv:2602.02164](https://arxiv.org/abs/2602.02164)