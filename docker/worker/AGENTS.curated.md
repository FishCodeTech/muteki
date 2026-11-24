<!-- muteki-workspace-doc:1 -->
# 环境

你在 Muteki 精选 Kali Worker 容器内。工作目录和运行模式由本次 Run 决定；共享容器中的
进程使用相同用户，不构成跨 Run 或跨账号的操作系统安全隔离。跨 Worker 交接使用 `shared/`
或黑板登记的产物。所有 Agent 及其工具直接以容器内 root 执行，可直接安装软件、使用网络和调试工具。
联网范围遵守本次任务授权。

# 工具与资料

构建清单在 `/opt/muteki/IMAGE_CAPABILITIES.json`，系统包版本在 `/opt/muteki/packages.tsv`。
使用工具前先通过 `command -v` 和 `--help` 确认实际能力，不把安装状态当成模型认证或联网成功。

本配方提供 shell、Python、pwntools、SymPy、GMPY2、Z3、PyCryptodome、curl、git、jq、ripgrep，
以及以下常用工具：

- Web/网络：nmap、masscan、ffuf、gobuster、dirb、nikto、whatweb、sqlmap、nuclei、chisel、socat、ncat、proxychains4。
- Pwn/逆向：gdb、gdb-multiarch、radare2、binutils、patchelf、strace、ltrace、QEMU user、UPX。
- 取证/隐写：tshark、tcpdump、binwalk、foremost、exiftool、steghide、Sleuth Kit、Tesseract。
- 凭据和连接：hydra、john、hashcat、OpenSSH、sshpass、OpenVPN。
- 浏览器：Chromium headless shell 和 agent-browser；使用任务提供的浏览器协调入口。
- Agent CLI：Claude Code、Codex、Cursor、Pi、OMP、Kimi Code、Grok、OpenCode、Droid。

精简字典位于 `/usr/share/wordlists/ctf-micro`。文本资料在 `/home/kali/knowledges`、
`/home/kali/pocs` 和 `/home/kali/.local/nuclei-templates`；索引在 `/home/kali/KNOWLEDGE_INDEX.txt`。
资料版本在 `/opt/muteki/knowledge-commits.tsv`。大型媒体和二进制资料不随默认镜像提供。
SageMath、angr、Volatility 等重型专科依赖不在默认配方内；需要时报告能力缺口或使用已授权的扩展环境。

# 工作与共享黑板

完整读取任务 Prompt 指定的 `muteki-blackboard/SKILL.md`。共享状态读取、事实、结果、分支
和资源登记均遵守黑板协议。不要自行改用未领取的 Step，也不要把模型输出当成真实执行证据。
大型输出保存为工作区文件，提交时说明实际命令、结果和文件位置。后台进程归本 Run 管理，
共享模式下不能用全局关闭浏览器或按命令名批量杀进程的方式清理单个任务。
