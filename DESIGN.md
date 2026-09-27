---
version: alpha
name: "Project Muteki"
description: "面向 CTF 与渗透任务的多智能体指挥工作台，以清晰状态、低干扰层级和可核查操作为核心。"
colors:
  background: "#f7f8fa"
  rail: "#f2f4f6"
  surface: "#ffffff"
  text: "#242930"
  accent: "#56779f"
  border: "#e5e8ec"
typography:
  sans:
    fontFamily: '-apple-system, BlinkMacSystemFont, "Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif'
  mono:
    fontFamily: 'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace'
rounded:
  DEFAULT: "8px"
  sm: "6px"
  md: "10px"
  lg: "14px"
spacing:
  shell-header: "48px"
  conversation-rail: "280px"
components:
  button: {}
  navigation: {}
  sidebar: {}
  dialog: {}
---

# Project Muteki Design System

## Overview

### Creative North Star

Muteki 使用专业控制台的布局纪律：关键工作模式固定在全局顶部，当前上下文和高频动作固定在左侧，内容区保持安静。界面通过状态、边界和字重表达层级，不依赖装饰。

### Product context and register

- **Audience and primary job:** CTF 选手、安全研究人员和调度操作员需要创建任务、切换工作区并核对 Agent 执行状态。
- **Target market(s) and evidence:** 当前产品为中文界面；产品范围来自 `docs/current_iteration_todo.md` 的三类工作区验收要求。
- **Locale(s) and language policy:** 用户可见操作与状态使用简体中文；引擎名、模型名和技术标识保留原文。
- **Usage scene:** 以桌面端长时间工作为主，同时保证窄屏可以访问全部导航与操作。
- **Register:** 产品工作台。
- **Memorable signature:** 当前工作区使用一条短蓝色状态轨道连接图标、名称和活动数量。
- **Restraint:** 内容编辑区、历史列表和设置入口保持低对比，避免与任务状态争夺注意力。
- **Anti-references:** 避免整条胶囊导航、同层级边框堆叠、无含义渐变和装饰性大图标。
- **Token ownership/runtime mapping:** 本文件记录设计意图；运行时令牌以 `apps/web/ui/lib/palette-engine.ts` 生成的应用变量和 HeroUI v3 语义变量（`--background`、`--surface`、`--accent`、`--field-*` 等）为准。HeroUI v3 样式由 `apps/web/ui/app/globals.css` 直接导入；`apps/web/ui/app/providers.tsx` 只挂载 HeroUI Toast Provider，不包装业务控件。

## Colors

背景、侧栏、表面和边框形成四级灰阶；`accent` 只用于当前工作区、主要操作、焦点和运行状态。深浅模式由现有配色引擎覆盖同名运行时变量。

## Typography

导航、操作和正文使用支持中文的系统无衬线字体；快捷键、模型名、标识符和数值使用等宽字体。小字号只用于辅助状态，不承担主要操作名称。

## Layout

全局顶部栏为 48px 单层结构：左侧品牌、中间工作区导航、右侧全局命令。桌面端允许将其收为 8px 的顶部触发轨道；鼠标在轨道停留 1 秒后，只以覆盖方式滑出居中的宽抽屉与底部把手，完整导航保持隐藏。点击抽屉把手后才固定展开顶部栏，离开抽屉则延迟收回。对话路由的桌面侧栏为 280px 全高结构，并占用全局栏的左侧品牌槽；“新对话、搜索、活动视图”直接固定在窗口左上角，与中间工作区导航共用第一行。置顶、项目、会话、归档筛选和设置共用下方的纵向滚动区域。项目文件夹和同组会话支持拖动排序，顺序作为本机侧栏偏好保存；拖动会话不改变项目归属，右键菜单中的上移、下移提供非拖动操作。项目标题控制文件夹展开与收起。每个项目文件夹默认最多展示 5 个会话，并保证当前会话包含在这 5 个条目中；存在更多会话时使用文件夹内的展开与收起按钮切换完整列表。开启“显示已归档”后，只在滚动区域末尾增加可收起的“已归档”分组；归档会话不回流到置顶、项目或最近对话。活动按钮直接切换侧栏内容，按“优先级、今天、昨天、更早”展示会话与所属项目，不打开任务回执弹窗。会话内容区顶部只保留标题、上下文和三个工作区显示按钮，不设置“对话 / 运行轨迹”二级标签栏。三个按钮依次打开任务输出右栏、切换底部终端面板和右侧工作面板。任务输出使用 HeroUI Card 放入独立的右侧布局列，打开后占用右栏位置并压缩中间对话区；点击对话内容不会关闭，只有再次点击任务输出按钮、打开右侧工作面板或进入条目详情时才关闭。卡片宽度为 320px，所在右栏宽度为 360px，内容较长时只在卡片内部滚动。1320px 及以下隐藏任务输出右栏和入口，不改为覆盖聊天内容的浮层。右侧工作面板默认关闭，并与任务输出右栏互斥；底部终端与这两者保持独立。右侧工作面板为常驻、非模态、推挤内容区的桌面工作区，使用启动器打开 Browser、Terminal、Files、Diff、Pull request 和 Agents 视图；Files、Diff 与 Terminal 直接操作当前会话绑定的工作区，Browser 只在用户输入地址后加载预览，Agents 读取当前会话真实工具事件，Pull request 在当前分支没有可用 PR 时保持禁用并说明原因。当前视图按 Thread 保存。工具或文件详情打开时替换右侧工作面板，避免双抽屉并排。840px 以下同时隐藏底部终端与右侧工作面板。

新对话的项目选择器只展示已绑定本机文件夹的项目，项目名称取文件夹名。选择“新建项目”时打开宿主系统的文件夹选择器，不显示绝对路径输入框；选择“不在项目中工作”时，对话不绑定 Project 或 Workspace。项目选择器不暴露沙箱、本机、Git 等底层工作区类型。

## Elevation & Depth

固定壳层使用细边框和轻微投影；浮层使用现有 `--shadow-pop`。常驻导航不使用大面积阴影或多重描边。

## Shapes

导航项和按钮采用 8–10px 圆角；状态徽标可以使用全圆角。相邻容器避免重复圆角边框形成嵌套胶囊。

## Components

### Canonical UI Map

| Capability | Canonical owner | Source of truth | Allowed variants | Verification |
| --- | --- | --- | --- | --- |
| Buttons, fields and overlays | HeroUI v3 | `@heroui/react`, `@heroui/styles`, `apps/web/ui/app/globals.css` | Button / Input / TextArea / Select / Checkbox / Radio / Switch / Modal / Popover / Tooltip / Toast / Skeleton | Keyboard, focus return, disabled/loading state, palette synchronization |
| Conversation task output | HeroUI Card、Accordion、Button 与 ScrollShadow 的业务组合 | `apps/web/ui/components/conversation/ConversationWorkspaceDock.tsx` | Verifiable deliverables / running processes / workspace, attachment and browser sources | Dedicated right-column placement, persistent open state, panel mutual exclusion, narrow-screen hide, long-content scroll, light/dark palette |
| Select/Listbox | HeroUI Select、ListBox、SearchField 与 Popover | `@heroui/react`；多接入点模型选择的业务编排位于 `apps/web/ui/components/conversation/ConversationModelPicker.tsx` | 固定选项；可搜索模型目录；Agent、接入点和收藏状态 | Browser wheel, keyboard focus, filtering, selection, open popup |
| Conversation access mode | ConversationModelPicker 内的单个 HeroUI Select | `AgentCapabilities.access_modes`、`ThreadRuntimeSelection.access_mode`、各 Runtime Adapter | 严格监督 / 自动接受修改 / 自动 / 完全访问；仅展示当前 Agent 声明支持的模式 | 切换八类 Agent、键盘选择、会话快照、审批卡、真实文件操作 |
| Date | Native date and time input | Browser native control | Competition scheduling and secret expiry | Locale, keyboard and invalid-value checks |
| Conversation project picker | HeroUI Popover、SearchField 与 ListBox 的业务组合 | `apps/web/ui/components/conversation/ConversationContextPicker.tsx` | Existing folder project / native folder selection / no project | Search, clear, choose, Escape focus return, no manual path |
| Conversation capability picker | PromptBar-scoped searchable listbox | `apps/web/ui/components/ai-native/prompt-bar.tsx`, `muteki/conversation/composer_capabilities.py` | `/` built-in actions + current-Agent Skills; `@` conversations/files/plugins/Muteki components; `$` current-Agent Skills only | Engine switch refresh, same-engine model retention, IME, arrows, Enter/Tab, Escape, stale-reference rejection |
| Scrollbar | Global application stylesheet | `apps/web/ui/app/globals.css` | Stable gutter and density exceptions | Computed style + browser wheel |
| Conversation workspace surfaces | Authored persistent workspace panel | `apps/web/ui/components/conversation/ConversationWorkspaceDock.tsx` | Launcher / browser / terminal / files / diff / pull request / agents | Thread-scoped selection, file navigation, command input, URL input, refresh, independent panel toggles |
| Conversation details | Existing on-demand details drawer | `apps/web/ui/components/conversation/ConversationDetailsDrawer.tsx` | Tool / diff / artifact / error | Replaces persistent right panel while open |
| Conversation turn presentation | Authored event fold, process disclosure and turn actions | `apps/web/ui/components/conversation/conversationEventViews.ts`, `ConversationTurnProcess.tsx`, `ConversationTimeline.tsx` | Running / completed / failed / interrupted / waiting / disconnected / retry | Structured stream, process disclosure above persistent result, persistent actions, composer error panel, scroll and keyboard checks |

### Conversation turn presentation

对话流按一轮任务拆成四类公开内容：进度说明、思考过程、工具调用和最终回复。运行期间按到达顺序流式更新；工具只展示名称、状态、耗时与参数或结果摘要，详细输入输出由用户主动打开。Codex 的 `commentary` 作为进度，`final_answer` 作为最终回复。思考区域只接收 Runtime 明确输出的 thinking/reasoning 事件，不把普通状态或最终回复重复写入思考过程。

新会话先使用“新对话”占位标题。首个包含实际任务信息的回合完成后，优先复用单题任务工作区配置的 Titler，在独立临时调用中异步生成短标题和一句话摘要；Titler 凭据、配置、请求或输出不可用时，再使用该 Thread 当前选择的 Runtime、凭据和模型生成。仅有问候的回合不触发生成。后续成功回合只更新摘要，已生成标题保持稳定；用户手动改名后，后台模型不再覆盖标题。每次生成都绑定元数据版本和依据回合，重试、后续消息或手动改名会使迟到结果失效。模型摘要用于侧栏搜索和会话导航，不进入任务输出证据。任务输出只展示可由 Artifact 事件、工具进程或工作区绑定核验的条目。

运行中的补充状态由公开事件确定：工具名称与参数只解析为“列出文件、读取文件、搜索、运行命令、检查变更、更新文件、查看网页”等有限状态；工具结束后显示正在整理回复。没有可确认事件时只显示“Agent 正在处理”，等待审批或补充输入时由对应交互卡片表达。Runtime 输出 thinking/reasoning 时显示“Agent 正在思考”，并将内容持续追加到本轮思考过程。

一轮任务结束后，工作过程收拢为最终回复上方的“耗时”折叠行，最终回复紧接其后并始终展示。用户展开时，进度说明、思考过程和工具摘要插入耗时行与最终回复之间；各工具组仍保持摘要折叠，避免长任务一次显示大量命令，完整记录继续保留并可逐层查看。完成、失败和中断的 Agent 回复下方常驻“查看 Markdown 原文、复制、重试、Fork”四个图标操作；原文按钮使用代码图标，与复制图标保持清晰区分。没有回复正文时复制处于禁用状态。审批和补充输入在等待期间保持可见，不随工作过程收起。实时连接断开时显示重连状态，已经收到的内容不清空。

“重试”从所选轮次开始替换当前分支：所选轮次及其后的内容从当前对话、统计和后续模型上下文中移除，原用户消息在新的 Runtime 会话中重新执行；旧事件和旧 Turn 记录只保留作审计。“继续”只在最新一轮失败或中断时出现，沿用现有 Runtime 会话和当前末尾继续执行。失败详情使用输入框上方可关闭的紧凑面板展示，面板与消息流分开，不在对话正文中插入大面积错误框。

每轮 Agent 标识同时展示该轮实际使用的模型接入点和模型，例如“Agent · claude 系统登录 / default”。接入点由该轮保存的稳定凭据 ID 经过全局凭据目录解析，模型保留运行时提交的原始选择；`default` 等别名不展开猜测。切换接入点或模型只影响后续轮次，历史轮次继续显示自身运行快照。

对话流只在用户位于底部附近时自动跟随新内容。用户向上阅读历史后停止自动滚动，并显示“回到最新内容”；点击后恢复跟随。输入框需要兼容中文输入法组合输入，Enter 只在组合完成后发送。运行、完成、等待、失败、中断和断开状态通过可访问状态区域播报，折叠按钮提供 `aria-expanded` 和受控区域关系。

这里的界面折叠只改变展示，不删除事件，也不等同于模型上下文压缩。重试属于明确的分支替换操作，会改变当前视图和后续模型上下文，同时继续保留被替换事件作为审计依据；上下文压缩属于运行时输入管理。

### Foundational visual states

可交互控件提供默认、悬停、按下、选中、禁用和 `focus-visible` 状态。焦点使用 `--blue` 外轮廓，状态变化不改变控件尺寸。

### Buttons and actions

每个区域只保留一个高强调主操作；图标操作必须有可访问名称。危险操作沿用语义色，并与安全操作分开。

### Navigation and data display

全局导航表达三类工作区，侧栏表达当前工作区内的项目和对话。当前项同时通过颜色、背景和结构标记，完整标题可通过聚焦或操作菜单访问。
Agent 登录、模型和 API 接入点统一由设置中的 Agents 页面维护。Agents 总览固定列出九类登记引擎，其中八类 CLI 引擎可用；DeepSeek Harness 固定显示“暂不支持”，不进入统计、配置、执行或探测。每类引擎只出现一次，并在同一行分别展示运行环境、版本、启用状态、可用凭据数、模型数和运行环境数；运行环境列只显示 CLI 名称与二进制路径，版本列独立显示已安装版本与最新版本状态。版本状态由后台定时探测和手动刷新共同维护，只读取已安装版本与官方最新发布信息，页面不提供安装或更新操作。凭据作为引擎详情中的 0–N 条资源集中管理，自定义端点同样归入对应引擎，不生成新的引擎条目；模型目录按凭据和运行环境隔离。

Reason / Planner 与 Titler 只选择“模型端点”，模型端点在公开配置中使用独立的 `endpoint:<id>` 标识，不带 Agent 引擎字段。它们由 Muteki 后端直接请求 Base URL，不启动 CLI Agent，也不读取 Worker 运行环境。为了避免重复保存密钥，模型端点继续复用 Agents 页面中自定义端点的 Base URL、API Key 和模型目录；该记录在 Agents 页面所属的引擎只约束 Worker 调用方式。推理模型页的端点选择器同时提供“自定义模型端点”，允许直接填写名称、Base URL、API Key 和模型 ID，保存后转为可复用的独立端点。相关页面必须同时说明“后端 HTTP 直连”和“不会启动 Agent”，选项中不得拼接 Pi、Cursor、Codex 等引擎名称。

Worker 出战池、运行环境、调度预算和推理模型归属单题工作区，在 `/task/workers` 管理并用于下一次单题派发；旧 `/settings/workers` 只保留兼容跳转。比赛平台连接、Secret 引用和浏览器会话统一归属比赛工作区，并在对应连接详情中管理。旧的平台凭据设置路径只保留到比赛凭据分区的兼容跳转，不再提供第二套表单。

### Forms and overlays

字段、搜索、对话框、确认操作、提示和浮层直接组合 HeroUI，并使用配色引擎同步后的 HeroUI 令牌；搜索非空时必须提供清除按钮。项目不维护可由 HeroUI 替代的私有通用组件，也不建立只转发 HeroUI 属性的薄包装。HeroUI 没有对应组件时，适配样式留在具体业务页面内。
模型选择弹窗保持固定搜索栏，Agent 列表和模型列表在弹窗边界内各自滚动，任何列表内容都不能撑破浮层外壳。
对话输入框只保留一个访问模式选择器，不并列暴露权限模式和沙箱模式。前端只提交稳定的 `access_mode`；当前 Runtime Adapter 负责按该 Agent 自己的能力解析原生参数，并通过 `access_modes` 声明可选项。Worker 的无人值守启动参数不由对话访问模式覆盖。
Agents 凭据编辑页中的候选模型目录为只读的可展开清单，由目录刷新维护；默认模型输入框允许自由填写，并通过该目录提供可选提示。
对话输入框使用统一的 `/`、`@`、`$` 触发规则。`/` 展示当前 Agent 的原生内置命令、少量 Muteki 内置动作和当前 Agent 可调用的 Skill；八类可用引擎使用各自独立的命令目录，切换引擎后同步替换，原生命令选择后写回输入框并交给当前 Agent 会话处理。`@` 统一添加当前项目文件、其他对话、Muteki 插件与会话内置组件；`$` 只展示当前 Agent 与 Muteki 为该 Agent 注入的 Skill。三类入口共用贴近输入框左侧的紧凑分组列表，桌面宽度不超过 480px、高度不超过 324px，长目录在浮层内部滚动，不再扩展为与内容区等宽的页面。切换同一引擎内的模型保留已选引用，切换引擎、项目或对话时清空并重新读取。选择结果以结构化引用发送，服务端在执行前按当前 Agent、Binding 和工作目录再次解析；候选列表不授予额外权限。

### Iconography

所有产品图标通过项目 `Icon` 组件调用 Lucide，主导航图标为 15–16px。页面组件不直接维护 SVG 路径，也不使用 Unicode 字符充当图标；缺少的语义图标统一在 `Icon.tsx` 的 Lucide 映射中补充。图标不单独替代陌生操作名称。
状态型图标按钮使用轮廓表示未选中、强调色实心图标表示已选中，并同步提供变化后的操作名称与 `aria-pressed`；会话 Pin 按钮遵循这一规则。

### Motion

菜单开合、状态轨道和列表选中提供 120–200ms 反馈；文件夹展开和收起使用 200ms 的高度与透明度过渡，文件夹图标和箭头同步表达开合状态。顶部抽屉使用 300ms 的位置过渡和 1 秒悬停意图延迟，点击把手后顶部栏使用 260ms 位置过渡展开。右侧工作面板与底部终端使用 180ms 的位置和透明度过渡，任务输出浮层使用 150ms 进入反馈。`prefers-reduced-motion` 下取消非必要位移与循环状态动画，折叠只保留不超过 80ms 的透明度反馈。

Beautiful UI 只作为对话运行状态、流式反馈、思考与工具过程、审批卡、`NumberField` 以及浮层开合的表现参考；这些实现适配 HeroUI 的颜色、圆角、焦点和交互状态。现有对话壳层、信息架构、业务状态和操作能力保持为产品实现，不做整页组件替换，也不形成独立通用组件库。

### Content and data visualization

操作名称使用直接动词，状态使用统一中性词。运行数量、未读和审批数量使用文本或带可访问名称的徽标表达。
对话输入框下方使用一条低对比等宽统计栏，按“轮次与步骤、耗时、首 token、缓存、输入输出”排列；缺少真实执行样本时省略对应字段，窄于 900px 时隐藏。

## Do's and Don'ts

- **Do:** 让全局工作区、当前项目和当前对话形成明确的三级导航。
- **Do:** 通用交互直接使用 HeroUI v3 复合组件，复用配色引擎同步的应用变量与 HeroUI v3 语义变量，使所有配色方案保持一致层级。
- **Do:** 修改调色板或 HeroUI 组件后验证浅色、深色、运行时切换和持久化恢复。
- **Don't:** 新增可由 HeroUI 替代的私有通用组件或无意义薄包装。
- **Don't:** 把多个同权重控件包进一整块大胶囊。
- **Don't:** 通过缩小文字或隐藏名称解决空间不足；窄屏使用菜单和可访问标签。
