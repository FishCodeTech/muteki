<!-- muteki-workspace-doc:1 -->
# 环境

你在 Muteki CTF Worker 容器中（Ubuntu slim）。
当前目录是本 Worker 独立使用的工作目录（私有 cwd），其他 Worker 不会自动以此为当前目录。
同一 Run 的 Worker 共享容器用户与挂载，这不是 Worker 之间的文件权限隔离。
需要交接的文件放到 `shared/` 或按黑板协议登记，避免依赖其他 Worker 的临时路径。
联网及权限以本次运行配置为准。默认禁止提权；只有显式选择扩展权限时才可能使用 sudo。
需要额外系统软件而权限不足时，报告所缺依赖。

# 已安装工具

本镜像是精简接线镜像，**不含** Kali full 工具链、字典、PoC、知识库或 Nuclei 模板。
宿主投影的 `./toolbox/` 只包含本镜像内实际存在的路径；不要假设存在 chisel、
proxychains4、tmux、SecLists 等 full 专用链接。

提供 shell、Python 3、pwntools、curl、wget、git、jq、ripgrep、openssh-client，以及
Claude Code、Codex、Cursor、Pi、OMP、Kimi Code、Grok Build、OpenCode 八个 Worker CLI。
当前任务由其中一个 CLI 执行。

需要某项能力时，先执行 `command -v <command>` 和 `<command> --help`；确认确实缺失后，
再通过 `apt`、`apt-get` 或 `pip3 install --break-system-packages` 安装。

# 共享黑板流程

开始工作前完整读取任务 Prompt 指定的 `muteki-blackboard/SKILL.md`。共享状态的读取、事实与
结果写入、分支声明、资源协调和结果提交全部以该 Skill 为准。当前 Step 已由 Coordinator
分配，不要自行改用未领取的分支。

# 工作方式

- 需要跨 Worker 交接的文件写入 `shared/`，不要依赖私有 cwd 被其他 Worker 看见。
- 大型输出写入工作空间文件，在回复中给出文件路径和结论。
- 修改脚本后先运行与当前操作路径直接相关的命令，确认功能可以执行。
- 功能路径完成后等待后续指令，再补充额外防护、回归测试或兼容性处理。
