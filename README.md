<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="./assets/logo-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="./assets/logo-light.png">
    <img alt="Muteki Logo" src="./assets/logo-light.png" width="320">
  </picture>
</p>

<h1 align="center">無敵 · Project Muteki</h1>

<p align="center">
  <strong>Heterogeneous Multi-Model AI Agent Swarm · Autonomous Security Automation</strong>
</p>

<p align="center">
  <a href="https://github.com/FishCodeTech/muteki/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-blue.svg" alt="License"></a>
  <a href="https://www.python.org/"><img src="https://img.shields.io/badge/python-≥3.13-3776AB.svg?logo=python&logoColor=white" alt="Python"></a>
  <a href="https://github.com/FishCodeTech/muteki/stargazers"><img src="https://img.shields.io/github/stars/FishCodeTech/muteki?style=social" alt="Stars"></a>
  <a href="https://github.com/FishCodeTech/muteki/issues"><img src="https://img.shields.io/github/issues/FishCodeTech/muteki" alt="Issues"></a>
  <a href="https://github.com/FishCodeTech/muteki/pulls"><img src="https://img.shields.io/github/issues-pr/FishCodeTech/muteki" alt="PRs"></a>
  <img src="https://img.shields.io/badge/NYU_CTF_Bench-200%2F200_solved-brightgreen" alt="Benchmark">
  <img src="https://img.shields.io/badge/engines-8_active_CLIs-orange" alt="Eight available CLI engines; DeepSeek Harness is registered but temporarily unavailable">
</p>

<p align="center">
  <strong>English</strong> · <a href="README_CN.md">简体中文</a> · <a href="CHANGELOG.md">Changelog</a>
</p>

<p align="center">
  <a href="https://www.star-history.com/#fishcodetech/muteki&amp;Date">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=fishcodetech/muteki&amp;type=Date&amp;theme=dark">
      <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=fishcodetech/muteki&amp;type=Date">
      <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=fishcodetech/muteki&amp;type=Date">
    </picture>
  </a>
</p>

---

**無敵 · Project Muteki** is an open-source, multi-agent security framework built with Remix Engineering. A Coordinator schedules different Agent engines toward one goal, manages their context, and lets them collaborate through shared evidence. The current Worker engines are Claude, Codex, Cursor, Pi, OMP, Kimi, Grok, and OpenCode. The project will keep expanding beyond CTF toward a broader Agent workbench.

> ## ⚠️ Read before running
>
> Use Muteki only against challenges and targets you own or are authorized to test. Muteki drives CLI Agents that can run commands, invoke security tools, and access target services. **It does not isolate malicious challenges.**
>
> A dedicated disposable VPS, VM, or machine without sensitive data is recommended. Avoid shared hosts and production systems. See [SECURITY.md](SECURITY.md). I often run it directly on my own computer because that is easier to set up, but that does not remove the risk.

---

## What can it do?

At RIFFHACK 2026, Muteki ran for three hours without human takeover, solved every challenge, and placed eighth.

![RIFFHACK 2026 result](./assets/image-20260624162932292.png)

On the iChunQiu Yunjing *blackmaze* penetration-testing range, which had seen no solves for three months, Muteki took first blood in about two hours of actual solving. The platform shows 39 hours because debugging and multi-Flag development were interleaved with that run.

![Blackmaze range record](./assets/ee318ffa895e4b2ffd6df67da6c15f90.png)

![Blackmaze solve record](./assets/image-20260624163414544.png)

It also cleared the Yunjing badge scenarios and Hack The Box challenges across Insane and Hard difficulty levels. The NYU CTF Bench evaluation reached 200/200; those figures reflect the tested setup and model versions, not a promise about every new challenge.

The TSecBench hosted-mode run with deepseek-flash ranked 13th: [result page](https://tsecbench.zc.tencent.com/agent/22091).

![TSecBench hosted-mode ranking](./assets/image-20260928022218702.png)

After a month of engineering work, tuning, and fixes, Muteki is open source without a paid tier or private subgroup. Please report problems in Issues and help improve it. If the discussion group is full, the QR code below is another way to get in touch.

![Discussion group contact QR code](./assets/mmqrcode1790533396147.png)

---

## Release notes

The current version is **0.4.0**. See [CHANGELOG.md](CHANGELOG.md) for new features, behavior changes, removals, and upgrade notes.

---

## Quick start

> **Start with the single-challenge CTF workspace.** The home page shows this workspace by default. Conversation, competitions, and custom extensions are still being tested and can be enabled as described below. The Web pentest mode is being reworked and is currently unavailable.

### 1. Prepare the environment and start the Web app

You need [uv](https://docs.astral.sh/uv/), Python 3.13 or newer, and Node.js/npm. For local solving, install and sign in to at least one supported Agent CLI. `./init.sh` syncs Python dependencies; the first Web start installs and builds the frontend. The shortest manual path is:

```bash
git clone https://github.com/FishCodeTech/muteki.git
cd muteki
./init.sh
./run.sh web
```

Open **http://127.0.0.1:3001**. The backend defaults to port `8000`; both bind to loopback by default. The first frontend build and service initialization may take a while. Use the addresses and readiness output shown in the terminal. Stop the service with `Ctrl+C`.

![Current Web home page with the single-challenge workspace shown by default](./assets/readme-cn-home.png)

### 2. Complete the minimum configuration

1. Click **Single-task Workers** on the home page, or open **Worker settings** from the lower-left corner of the single-task page.
2. Under **Agent credentials**, check an existing CLI login or add a token, API key, or custom Base URL. Use the real connection test.
3. Under **Worker lineup**, enable at least one Worker and bind it to a usable account and model. Run the full self-check or check that Worker individually.
4. Under **Runtime environment**, choose **Local** for host CLIs or **Container** for Docker Workers. Container mode needs a Worker image and injectable credentials.
5. Under **Reasoning models**, select and test a Planner endpoint and model. Configure the Titler if needed.
6. Save the settings and return to **Single task**.

Local mode can reuse an Agent CLI login already available on the host. A container cannot inherit that login automatically; bind an injectable account in the credentials page. To get started quickly, use one CLI that already works in your terminal and select Local mode.

![Single-task settings for Workers, credentials, runtime, and reasoning models](./assets/readme-cn-worker-settings.png)

### 3. Submit the first challenge

On **Single task**, enter the challenge name, original description, target URL, known facts, and Flag format. Add attachments with the picker, paste, or drag and drop. Check single versus multiple Flags, Web tools, runtime, and advanced options, then click **Dispatch swarm** (`⌘↵` on Mac). If you leave the category empty, Muteki infers it from the challenge. The first run also prepares a workspace and Workers, so watch the status rather than clicking dispatch repeatedly. Defaults are generally a good starting point.

![Single-task composer with CTF mode, attachments, Flag format, Web tools, and advanced options](./assets/readme-cn-task-composer.png)

Only run challenges and targets you own or are authorized to test. Workers can execute commands and access target services.

## Recommended runtime and tool installation

The project has been tested on **macOS 26** and **Ubuntu 24.04**. On Windows, we recommend running Muteki inside an Ubuntu 24.04 VMware virtual machine.

| Runtime | Where Muteki and Workers run | Tool setup |
| --- | --- | --- |
| **macOS local** | Web and Workers on the Mac | Run `./ctf-tools/setup.sh` for native CTF tools |
| **macOS + Docker** | Web on the Mac; Workers in Docker | Pull the full Worker image, which includes the CTF toolchain |
| **Ubuntu 24.04 local** | Web and Workers on Ubuntu | Use the installer with `--with-ctf-tools`, or run `./ctf-tools/setup-ubuntu.sh` |
| **Windows + VMware** | Windows hosts the VM and browser; Muteki and Workers run inside Ubuntu 24.04 | Run the Ubuntu installer in the VM |

### 1. macOS local

Install Homebrew, Node.js/npm, and the Agent CLI you intend to use. Sign in using that vendor's CLI, then run from the repository root:

```bash
./init.sh
export PATH="$HOME/.local/bin:$PATH"  # If newly installed uv is not yet on PATH
./ctf-tools/setup.sh
./run.sh web
```

The setup script uses [`ctf-tools/Brewfile`](ctf-tools/Brewfile), prepares Python/Ruby tool environments, and creates command entry points. It **does not sign in to Agent CLIs**. Select Local under **Single-task settings → Runtime environment**. When `ctf-tools/.ready` exists, `run.sh` loads its tool paths automatically. See [`ctf-tools/README.md`](ctf-tools/README.md) for optional large dictionaries and knowledge copied from the Worker image. To refresh only existing tool links, run `./ctf-tools/setup.sh --link-only`. macOS native tools and those inside the Kali image are not identical.

### 2. macOS with Docker Workers

Install and start Docker Desktop, then run:

```bash
./init.sh
docker pull ghcr.io/fishcodetech/muteki-worker:latest
./run.sh web
```

Select Container under **Single-task settings → Runtime environment**. Check that Docker and the Worker image are available, then bind **injectable credentials** to the selected Workers. Host CLI logins are not automatically copied into containers. The full image already contains the CTF toolchain, so `ctf-tools/setup.sh` is not needed on the Mac for this mode. The Web app stays on the Mac; Docker starts Workers per run.

### 3. Ubuntu 24.04 local

```bash
git clone https://github.com/FishCodeTech/muteki.git
cd muteki
./scripts/install-ubuntu.sh --backend local --preflight
./scripts/install-ubuntu.sh --backend local --with-ctf-tools
./scripts/install-ubuntu.sh --backend local --check
```

The installer prepares application dependencies, Node.js, the Web service, and local Worker CLIs; it calls `ctf-tools/setup-ubuntu.sh` for the available CTF tools. Agent CLIs still need their own login or configured credentials. By default it creates `muteki-web.service`, stores the Web password in `~/.config/muteki/ubuntu.env`, and serves the UI on port `3001`. Optional apt packages may be unavailable on some mirrors or architectures; the script reports those individually.

If Muteki is already installed and running, you can install just the local CTF tools with `./ctf-tools/setup-ubuntu.sh` after ensuring `uv` is available. That script has only been tested on Ubuntu 24.04; other Linux distributions may need changes. Once `ctf-tools/.ready` exists, `./run.sh web` loads the tool paths.

### 4. Windows with an Ubuntu VMware guest

Create an **Ubuntu 24.04** VM, clone the repository inside it, and run the same installer commands there:

```bash
git clone https://github.com/FishCodeTech/muteki.git
cd muteki
./scripts/install-ubuntu.sh --backend local --with-ctf-tools
./scripts/install-ubuntu.sh --backend local --check
```

Muteki, Agent CLIs, CTF tools, and run data stay inside Ubuntu. Open `http://<VM-IP>:3001` from Windows and use the Web password saved in the VM's `~/.config/muteki/ubuntu.env`. If VMware NAT prevents access to the VM IP, configure port forwarding or use a suitable virtual network. The backend API still listens only on the VM loopback address by default.

## Configuration guide

| Location | What it controls | First action |
| --- | --- | --- |
| **Single-task settings → Agent credentials** | Engine detection, host login, tokens/keys, custom endpoints, connection tests | Confirm at least one usable account; never paste keys into a challenge or commit them |
| **Single-task settings → Worker lineup** | Worker engines, models, reasoning effort, enablement, and per-seat concurrency | Enable one Worker that passes preflight, then add others |
| **Single-task settings → Runtime environment** | Local/container mode, container scope, network, image, optional VPN | Usually keep the default |
| **Single-task settings → Scheduling and budgets** | Automatic versus fixed dispatch, concurrency, time, Worker count, cost cap | Keep defaults initially; set limits when needed |
| **Single-task settings → Reasoning models** | Planner and Titler endpoints, models, generation parameters | Test a real Planner request |
| **Settings → Appearance → Workspace mode** | Visibility of conversation and competition workspaces | Enable them to try the additional workspaces |

**Workers and the Planner are configured separately.** Worker CLIs execute the task. The Planner makes coordination decisions and proposes Steps. Local Workers may reuse host logins, while the Planner needs a working model endpoint. Add a compatible custom endpoint in credentials/model settings, then bind it to the appropriate Worker or Planner. Saving settings affects **future** runs; it does not rebuild a run already in progress.

### Supported Worker engines

The UI can configure Claude Code, Codex, Cursor, Pi, OMP, Kimi, Grok, and OpenCode. Whether each one works depends on installation, vendor login, and the chosen runtime. DeepSeek Harness is registered but cannot currently be selected as a Worker because its CLI does not provide the required structured provider capabilities. Follow each vendor's installation and login instructions.

If the page says **Needs self-check** or **No usable credential**, test the account under Agent credentials, then check the Worker's account, model, and runtime binding. Self-check sends a real model request and may incur usage.

## Solving a challenge

### Before dispatch

- **Description:** Provide the original prompt, category, URL or host and port, Flag format, and facts you have verified. Do not present guesses as confirmed facts.
- **Attachments:** Upload challenge files. The run workspace keeps inputs and Worker artifacts for later review.
- **One or several Flags:** A single-Flag run can stop at a qualified candidate. A multi-Flag run keeps collecting; enter the expected count if known, or watch the timeout, budget, and manual stop controls.
- **Web tools:** Control the Agent's WebSearch and WebFetch capabilities so unrelated external searches do not pull it away from the target.
- **Advanced:** Set the Flag format, allow operator-input requests, and bound time, Worker count, and cost for this run. Match the competition's Flag rules.

### During a run: composer and controls

The run page shows Coordinator messages, Worker state, activity, evidence, and candidate Flags. Target all Workers or one Worker from the selector above the composer. Controls change with run state:

![Activity timeline with tool calls from several Workers during a CSAW Finals 2021 challenge](./assets/readme-cn-solving-nyu-sfc.png)

| Action | Effect | When to use it |
| --- | --- | --- |
| **Type and press Enter / Send** | Sends a hint to the selected target; **does not create a Step** | Add a clue, correct the challenge text, or share a known result |
| **Dispatch** | Creates a new Step from the input; a URL in the text may update the target | Ask for a specific new direction or action |
| **Ask progress** | Summarizes verified findings, active directions, and blockers in the conversation | Check status without changing the task |
| **Pause / Resume** | Pauses and resumes swarm scheduling | Inspect the challenge or wait for operator input |
| **Freeze / Unfreeze** | Immediately freezes active Workers and releases them later | Stronger intervention in running work |
| **Stop** | Stops this run and its Workers while keeping the record | The target changed, the run should end, or a limit was reached |

A hint differs from Dispatch. Typing “The `/admin` directory is confirmed” and pressing Enter adds information. Clicking Dispatch creates a new Step and may launch a Worker. Avoid repeatedly clicking controls while the run is updating.

![Conversation composer and run controls during solving](./assets/readme-cn-solving-controls.png)

If a Worker asks for input, answer in the pending card, provide the needed resource, or reject/correct the request so the run can continue.

### After a Flag or a finished run

- **Copy the Flag and verify it on the challenge platform.**
- **Mark false positive:** If the platform rejects a candidate, mark that specific Flag as a false positive (`×` on its row does the same). For multi-Flag runs, select the candidate first. This reopens solving.
- **Continue solving:** Restart the swarm on the finished run while preserving its existing evidence.
- **Ask / Generate writeup:** Ask the solving Worker a follow-up or generate a report. On success, the report appears in the conversation and in `sessions/<run-id>/workspace/writeup.md` (or under your custom `MUTEKI_SESSIONS_ROOT`). Generation summarizes the run's full context and can take time.

## Additional workspaces under test

The home page shows only single-task solving by default. In **Settings → Appearance → Workspace mode**, enable **Show conversation and competition modes** to reveal Conversation and Competition on the home page, navigation, and search. These workspaces are still being tested; please report issues.

![Workspace-mode setting for conversation and competition](./assets/readme-cn-workspace-mode.png)

![Home page with Conversation, Single task, and Competition visible](./assets/readme-cn-workspaces.png)

### Conversation workspace (testing)

Open **Conversation** to work with an external Agent across multiple turns and review tool calls, approvals, artifacts, and history. Configure its Agent runtime, credentials, and permissions under **Settings → Agents** first. Live guidance depends on the connected transport. Its controls are separate from those for a single CTF run.

### Competition workspace (testing)

Create and test a platform connection, then register the remote competition ID. The workspace can sync challenges, schedule single-task runs, inspect candidates, and track remote submission verdicts. CTFd, rCTF, and GZCTF connection options are available; actual capability depends on the probe result. Test account and submission behavior in a practice competition first.

### Custom Agent Plugins (testing)

Open **Settings → Extensions** (or Extensions on the home page when all workspaces are enabled). Sources may be a local directory, archive, Git, HTTP, or Catalog. An extension root needs an **Agent Plugins 1.0.0** `plugin.json`. Generate an install preview, review its source, digest, and permissions, then install. Installed plugins can be enabled, configured, upgraded, rolled back, or removed. Install only extensions you trust.

![Extension settings with install preview and plugin list](./assets/readme-cn-extensions.png)

## Deployment and updates

### Local Web service

```bash
./run.sh web                         # API :8000 and UI :3001
./run.sh web --backend-only          # API only
./run.sh web --ui-port 3002          # Different UI port
./run.sh web --rebuild-ui            # Rebuild after changing UI source
```

Copy [`.env.example`](.env.example) to `.env` at the repository root for `MUTEKI_*` settings; exported shell variables take precedence. Keep secrets only in ignored locations such as `.env` and `state/_secrets/`. If binding Web to a non-loopback address, set `MUTEKI_WEB_PASSWORD` first or the backend will refuse to start.

### Docker Compose

Compose starts the FastAPI and Next.js control plane. The Docker daemon starts Workers on demand. Prepare Docker and the Worker image, then set an absolute host data path and Web password:

```bash
docker pull ghcr.io/fishcodetech/muteki-worker:latest
MUTEKI_HOST_DATA_ROOT=/opt/muteki/data \
MUTEKI_WEB_PASSWORD='replace-with-a-strong-password' \
  docker compose up --build
```

The UI remains at `http://localhost:3001` by default. `MUTEKI_HOST_DATA_ROOT` must be an absolute host path shared by the control plane and Workers. The macOS Docker Desktop path has been exercised; Windows Compose has not been verified end to end on real hardware. See [`.env.example`](.env.example) and [SECURITY.md](SECURITY.md) for more settings and runtime boundaries.

### Application updates and rollback

```bash
./run.sh upgrade --check   # Check the latest stable release
./run.sh install            # Set up a managed installation
muteki upgrade v0.4.0       # Install this release
muteki rollback             # Return to the previous installed release
muteki version              # Show version and installation kind
```

**Single-task settings → System update** provides the same operations. Preserve `.env`, `sessions/`, and `state/` during upgrades. Do not commit run records or credentials.

## Troubleshooting

| Symptom | Check first |
| --- | --- |
| Page does not open | The address and ports printed by `./run.sh web`; Node/npm, first UI build, and `state/_logs/backend.log` / `state/_logs/ui.log` |
| No usable Worker | CLI installation and login, Agent credential test, enabled Worker lineup, account and model binding |
| Container run fails to start | Docker service, Worker image, container credentials, network/VPN, and Runtime environment settings |
| Planner test fails | Endpoint, model, key/Base URL, and network; a working Worker CLI does not configure the Planner |
| Conversation, Competition, or Extensions missing | Enable their visibility in **Settings → Appearance → Workspace mode** |
| Platform rejects a Flag | Mark that candidate as a false positive, continue solving, and check the challenge text and Flag format |
| Multi-Flag run keeps going | Enter the expected count; otherwise stop manually or use a time/worker/cost limit |

## Project and participation

- **Code map:** `muteki/` holds the backend core; `apps/web/` is FastAPI; `apps/web/ui/` is Next.js; `docker/` contains image configuration.
- **Issues:** Report bugs and documentation problems in [GitHub Issues](https://github.com/FishCodeTech/muteki/issues), with reproduction steps, version, and redacted logs. Report security vulnerabilities privately as described in [SECURITY.md](SECURITY.md).
- **License:** [GNU AGPL-3.0](LICENSE). External Agent CLIs and model services have their own licenses, terms, and charges.

## Future work

- [ ] Rework pentest mode
- [ ] Add a source-code vulnerability discovery mode
- [ ] Expand the integrated Agent chat experience
- [ ] Make the product fully plugin-based

## Acknowledgements

Thanks to [c3](https://github.com/Real-C3ngH) for the Yunjing range account. I used a lot of grit and made plenty of popcorn along the way.

Thanks to [l4n](https://github.com/lancer0rz) for the inspiration; the reviewer brought a major improvement to solving efficiency.

Thanks to [陈橘墨](https://github.com/Randark-JMT) for range resources and writeups used in testing and tuning.

~~Thanks to Sam Altman for not banning my account.~~ It got banned; I will remember his name.

~~Thanks to Dario Amodei for not banning my account.~~ It got banned too; I will remember his name.

## References

This project's design and evaluation drew on the following work:

1. **NYU CTF Bench: A Scalable Open-Source Benchmark Dataset for Evaluating LLMs in Offensive Security** — Minghao Shao, Sofija Jancheska, Meet Udeshi, Brendan Dolan-Gavitt, et al. *NeurIPS 2024 Datasets & Benchmarks Track*. [arXiv:2406.05590](https://arxiv.org/abs/2406.05590)
2. **Teams of LLM Agents can Exploit Zero-Day Vulnerabilities** — Richard Fang, Rohan Bindu, Akul Gupta, Daniel Kang. *EACL 2026*. [Paper](https://aclanthology.org/2026.eacl-long.2.pdf)
3. **D-CIPHER: Dynamic Collaborative Intelligent Multi-Agent System with Planner and Heterogeneous Executors for Offensive Security** — Chenhui Zhang, et al. 2025. [arXiv:2502.10931](https://arxiv.org/abs/2502.10931)
4. **HackSynth: LLM Agent and Evaluation Framework for Autonomous Penetration Testing** — Lajos Muzsai, David Imolai, András Lukács. 2024. [arXiv:2412.01778](https://arxiv.org/abs/2412.01778)
5. **CTFAgent: An LLM-powered Agent for CTF Challenge Solving** — Jiaze Sun, et al. *Computers & Security*, 2025. [ScienceDirect](https://doi.org/10.1016/j.cose.2025.104488)
6. **Co-RedTeam: Orchestrated Security Discovery and Exploitation with LLM Agents** — Jiahao Zhu, et al. 2025. [arXiv:2602.02164](https://arxiv.org/abs/2602.02164)
7. [Related project article](https://mp.weixin.qq.com/s/ZzKF_0MOb0cak9izhHqCUQ)
