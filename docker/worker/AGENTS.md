<!-- muteki-workspace-doc:1 -->
# 环境

你在 Muteki CTF Worker 容器中。当前镜像可能是 Kali full，也可能是 Ubuntu slim。
当前目录是本 Worker 独立使用的工作目录（私有 cwd），其他 Worker 不会自动以此为当前目录。
同一 Run 的 Worker 共享容器用户与挂载，这不是 Worker 之间的文件权限隔离。
需要交接的文件放到 `shared/` 或按黑板协议登记，避免依赖其他 Worker 的临时路径。
联网及权限以本次运行配置为准。默认禁止提权；只有显式选择扩展权限时才可能使用 sudo。
需要额外系统软件而权限不足时，报告所缺依赖。

# 已安装工具

先使用镜像内已经安装的工具。需要某项能力时，先执行 `command -v <command>` 和
`<command> --help`；确认确实缺失后，再通过 `apt`、`apt-get` 或
`pip3 install --break-system-packages` 安装。宿主投影的 `./toolbox/` 只包含本镜像内
实际存在的路径，不要假设 slim 具备 full 的全部链接。

两个镜像均提供 shell、Python 3、pwntools、curl、wget、git、jq、ripgrep，以及 Claude
Code、Codex、Cursor、Pi、OMP、Kimi Code、Grok Build、OpenCode 八个 Worker CLI。
当前任务由其中一个 CLI 执行。

Kali full 还提供完整的 `kali-linux-headless` 工具集。下列为构建时强制安装的核心入口
（缺失会使镜像验收失败）；使用前仍应先 `command -v` / `--help` 确认：

- Web 与网络：`nmap`、`masscan`、`ffuf`、`gobuster`、`dirb`、`nikto`、`whatweb`、
  `sqlmap`、`nuclei`、`curl`、`socat`、`ncat`、`proxychains4`、`sshpass`、`openvpn`、
  `chisel`。
- 凭据与口令：`hydra`、`john`、`hashcat`、`jwt_tool`。
- Pwn 与逆向：`gdb`、`gdb-multiarch`、GEF、`radare2`、`ROPgadget`、`angr`、
  `patchelf`、`strace`、`ltrace`、`qemu-*`、`upx`。
- 取证与隐写：Volatility 3（`vol`）、`tshark`、`tcpdump`、`binwalk`、`foremost`、
  `exiftool`、`steghide`、Sleuth Kit、`tesseract`、StegSolve。
- 密码与数学：SageMath（`sage`）、SymPy、GMPY2、Z3、PyCryptodome。
- 云、容器与区块链：`cloudfox`、CDK、Foundry（`forge`、`cast`、`anvil`）。
- Java 与 .NET：`ilspycmd`，以及 `/opt/tools/` 下的 `ysoserial.jar`、
  `marshalsec.jar`、`JNDI-Injection-Exploit.jar`、`stegsolve.jar`。
- 发布与运维辅助：`gh`。

下列为**可选**能力：构建时安装失败不会使镜像失败，文件存在也不等于可用。需要时用
`command -v` 和一次真实启动（`--version` / `--help`）确认，缺失则自行安装：

- 移动端：`jadx`、`apktool`、`aapt`、`apksigner`、`zipalign`、`adb`、`dex2jar`、
  Androguard、Frida、Objection。
- 额外 Python / Ruby：`fpylll`、Playwright Chromium、Semgrep、Bandit、mitmproxy、
  Slither、Web3、AWS CLI、`one_gadget`、`seccomp-tools`、`zsteg` 等。

Kali 本机 chisel 客户端是 `/usr/bin/chisel`。可上传到受控目标的 Linux AMD64、ARM64
静态程序分别位于 `/usr/share/chisel-common-binaries/chisel-linux-amd64` 和
`/usr/share/chisel-common-binaries/chisel-linux-arm64`。GoReSym 只在 AMD64 镜像提供，
ARM64 镜像不安装（可选）。Ubuntu slim 不包含上述 Kali full 工具和离线资料。

# Kali full 离线资料

- Payload 与利用方法：`/home/kali/knowledges/PayloadsAllTheThings`、
  `/home/kali/knowledges/InternalAllTheThings`
- 技术资料：`/home/kali/knowledges/hacktricks`、`hacktricks-cloud`
- CVE 与 PoC：`/home/kali/pocs/vulhub`、`/home/kali/pocs/Awesome-POC`
- Nuclei 模板：`/home/kali/.local/nuclei-templates`

这些目录只存在于 Kali full。目录存在时，可以先使用 `rg` 搜索本地资料；联网模式下也可以
查询外部资料。

# 共享黑板流程

开始工作前完整读取任务 Prompt 指定的 `muteki-blackboard/SKILL.md`。共享状态的读取、事实与
结果写入、分支声明、资源协调和结果提交全部以该 Skill 为准。当前 Step 已由 Coordinator
分配，不要自行改用未领取的分支。

# 工作方式

- 需要跨 Worker 交接的文件写入 `shared/`，不要依赖私有 cwd 被其他 Worker 看见。
- 需要持续运行的 HTTP 服务、监听器、反向 shell 或长时间扫描放入 tmux（若镜像提供），
  并在结果中写明 tmux 会话名。
- 大型扫描、抓包和反编译结果写入工作空间文件，在回复中给出文件路径和结论。
- 修改脚本后先运行与当前操作路径直接相关的命令，确认功能可以执行。
- 功能路径完成后等待后续指令，再补充额外防护、回归测试或兼容性处理。
