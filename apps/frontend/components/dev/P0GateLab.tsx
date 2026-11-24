"use client";

import { useMemo, useRef, useState } from "react";
import {
  Accordion,
  AccordionContent,
  AccordionItem,
  AccordionTrigger,
  Alert,
  Badge,
  Button,
  ButtonGroup,
  Callout,
  Card,
  CardBody,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
  Checkbox,
  Chip,
  ConfirmDialog,
  CopyButton,
  Dialog,
  DiffStat,
  EmptyState,
  HoverCard,
  Icon,
  IconButton,
  Kbd,
  Menu,
  MenuItem,
  MenuLabel,
  MenuSeparator,
  NativeSelect,
  Popover,
  ProgressBar,
  RadioGroup,
  SearchInput,
  SegmentedControl,
  Select,
  Sheet,
  ShimmerText,
  Shortcut,
  Skeleton,
  Slider,
  Spinner,
  StatusDot,
  Switch,
  Tabs,
  TabsContent,
  TabsList,
  TabsTrigger,
  TextArea,
  TextField,
  Tooltip,
  toast,
  type IconName,
  type ListOption,
} from "@muteki/ui";
import {
  AgentMarkdown,
  AgentPromptBar,
  ApprovalCard,
  CodeBlock,
  ContextMeter,
  DiffTable,
  EffortSlider,
  FileDiff,
  LoadingState,
  Message,
  MessageBubble,
  MessageBubbleContent,
  ModelPicker,
  QuestionCard,
  Thinking,
  TodoList,
  ToolResult,
  ToolResultOutput,
  parseUnifiedDiff,
  type QuestionCardAnswers,
  type QuestionCardQuestion,
  type ReasoningEffort,
  type TodoItem,
} from "@muteki/ui/agent";
import { MathFormula } from "@muteki/ui/engines/math-view";
import {
  SCHEMES,
  applySelection,
  readSavedSelection,
  readSavedTheme,
  type ThemeMode,
} from "@/lib/palette-engine";
import { setThemePreference } from "@/lib/themePreference";

const OPTIONS: ListOption<string>[] = [
  { value: "deepseek", label: "DeepSeek Reasoner", description: "Default coordinator engine", section: "Reasoning" },
  { value: "claude", label: "Claude Sonnet", description: "Code & analysis worker", section: "Workers" },
  { value: "codex", label: "Codex CLI", description: "Repository editing agent", section: "Workers" },
  { value: "pi", label: "Pi Local Runtime", description: "Isolated container worker", section: "Workers" },
];

const SHOWCASE_ICONS: IconName[] = [
  "sparkles", "terminal", "cpu", "shield", "shieldAlert", "bot", "brain",
  "code", "codeBlock", "fileCode", "fileDiff", "gitBranch", "gitCompare",
  "gitMerge", "gitPullRequest", "layers", "lock", "search", "sliders", "gear",
  "send", "stop", "play", "pause", "check", "checkCircle", "x", "xCircle",
  "alert", "circleAlert", "info", "home", "chartCombined", "power",
  "messages", "newChat", "panelLeft", "panelRightOpen", "chevronDown",
  "chevronRight", "copy", "download", "upload", "trash", "edit", "refresh",
  "target", "trophy", "globe", "database", "workflow", "activity", "gauge",
  "zap", "bookmark", "pin", "flag", "eye", "sun", "moon", "droplet",
];

const SAMPLE_UNIFIED_DIFF = `@@ -1,4 +1,5 @@
 import { Button, ConfirmDialog } from "@muteki/ui";
-import { HeroButton } from "@heroui/react";

 export function ActionBar() {
-  return <HeroButton>Run</HeroButton>;
+  return <Button variant="primary" agentConfirm="hitl">Run</Button>;
 }`;

const SAMPLE_QUESTIONS: QuestionCardQuestion[] = [
  {
    id: "target_env",
    title: "确认隔离验证环境",
    description: "选择要运行 E2E 验证的目标入口",
    options: [
      { value: "web", label: "Web Workbench（kimi-webbridge）" },
      { value: "desktop", label: "Desktop WKWebView（cua-driver）" },
      { value: "both", label: "双端完整验证（推荐）" },
    ],
    allowCustom: true,
    customPlaceholder: "或补充自定义端口与状态目录…",
    required: true,
  },
];

const SAMPLE_TODOS: TodoItem[] = [
  { id: "t1", title: "移除 @heroui/* 与 React Aria 运行时依赖", status: "completed", detail: "29 个调用文件已迁入 @muteki/ui" },
  { id: "t2", title: "抽离 @muteki/ui（Base + Agent + Engines）", status: "completed", detail: "4 条构建打包链路验证通过" },
  { id: "t3", title: "完成双端真机 /verify 验收与 Stamp", status: "in-progress", detail: "kimi-webbridge + cua-driver" },
];

export function P0GateLab() {
  const [activeTab, setActiveTab] = useState("gate");
  const [dialogOpen, setDialogOpen] = useState(false);
  const [selected, setSelected] = useState<string>("deepseek");
  const [popoverOpen, setPopoverOpen] = useState(false);
  const [imeInput, setImeInput] = useState("");
  const imeInputRef = useRef<HTMLInputElement | null>(null);
  const [theme, setTheme] = useState<ThemeMode>(() => (typeof document === "undefined" ? "dark" : readSavedTheme()));
  const [schemeId, setSchemeId] = useState<string>(() => {
    if (typeof document === "undefined") return "azure";
    const sel = readSavedSelection();
    return sel.kind === "preset" ? sel.id : "azure";
  });
  const [reducedMotionDemo, setReducedMotionDemo] = useState(false);
  const [log, setLog] = useState<string[]>([]);

  const [secretToken, setSecretToken] = useState("sk-muteki-demo-secret");
  const [noteText, setNoteText] = useState("");
  const [searchQuery, setSearchQuery] = useState("");
  const [checkedA, setCheckedA] = useState(true);
  const [switchOn, setSwitchOn] = useState(true);
  const [radioVal, setRadioVal] = useState("precision");
  const [sliderVal, setSliderVal] = useState(68);
  const [nativeChoice, setNativeChoice] = useState("rust");
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [sheetOpen, setSheetOpen] = useState(false);
  const [chipActive, setChipActive] = useState(true);

  const [approvalStatus, setApprovalStatus] = useState<"pending" | "resolving" | "expired">("pending");
  const [approvalNote, setApprovalNote] = useState("等待操作员确认");
  const [qaAnswers, setQaAnswers] = useState<QuestionCardAnswers>({ target_env: { selected: ["both"], custom: "" } });
  const [qaStatus, setQaStatus] = useState<"pending" | "answered">("pending");
  const [promptValue, setPromptValue] = useState("");
  const [promptModel, setPromptModel] = useState("deepseek");
  const [promptEffort, setPromptEffort] = useState<ReasoningEffort>("high");
  const [iconFilter, setIconFilter] = useState("");

  const pushLog = (entry: string) => setLog((prev) => [entry, ...prev].slice(0, 8));

  const toggleTheme = () => {
    const next: ThemeMode = theme === "dark" ? "light" : "dark";
    setTheme(next);
    setThemePreference(next);
    applySelection({ kind: "preset", id: schemeId }, next);
    pushLog(`theme:${next}`);
  };

  const selectScheme = (nextScheme: string) => {
    setSchemeId(nextScheme);
    applySelection({ kind: "preset", id: nextScheme }, theme);
    pushLog(`scheme:${nextScheme}`);
  };

  const toggleReducedMotion = (next: boolean) => {
    setReducedMotionDemo(next);
    if (typeof document !== "undefined") {
      if (next) document.documentElement.dataset.motion = "reduce";
      else delete document.documentElement.dataset.motion;
    }
    pushLog(`motion:${next ? "reduce" : "default"}`);
  };

  const filteredIcons = useMemo(() => {
    const q = iconFilter.trim().toLowerCase();
    return q ? SHOWCASE_ICONS.filter((name) => name.toLowerCase().includes(q)) : SHOWCASE_ICONS;
  }, [iconFilter]);

  return (
    <div className="cx-root min-h-[180vh] bg-cx-bg px-4 py-6 text-cx-fg sm:px-8">
      <div className="mx-auto flex max-w-5xl flex-col gap-6">
        <Card variant="elevated" className="p-6">
          <div className="flex flex-wrap items-start justify-between gap-4 border-b border-cx-border-subtle pb-4">
            <div>
              <div className="flex items-center gap-2">
                <Badge tone="accent" dot>@muteki/ui v0.1.0</Badge>
                <span className="text-cx-xs text-cx-fg-3">Tactile Precision Workbench</span>
              </div>
              <h1 className="mt-1.5 text-cx-lg font-semibold tracking-tight text-cx-fg">
                Muteki UI 组件体系与真机行为门禁
              </h1>
              <p className="mt-0.5 text-cx-xs text-cx-fg-3">
                @ui-sdk/react 0.1.0-beta.13 · OKLCH Tokens · Zero HeroUI · Zero React Aria · WebKit Safe
              </p>
            </div>
            <div className="flex flex-wrap items-center gap-2">
              <Button
                variant="secondary"
                size="sm"
                data-testid="p0-theme-toggle"
                onClick={toggleTheme}
              >
                <Icon name={theme === "dark" ? "moon" : "sun"} size={14} />
                主题：{theme === "dark" ? "深色" : "浅色"}
              </Button>
              <SegmentedControl
                size="xs"
                ariaLabel="OKLCH 配色方案"
                value={schemeId}
                onChange={selectScheme}
                options={SCHEMES.map((s) => ({ value: s.id, label: s.id }))}
              />
              <Button
                variant={reducedMotionDemo ? "primary" : "ghost"}
                size="sm"
                data-testid="ui-reduced-motion-toggle"
                onClick={() => toggleReducedMotion(!reducedMotionDemo)}
              >
                动效：{reducedMotionDemo ? "已减弱" : "标准"}
              </Button>
            </div>
          </div>

          <div className="mt-4">
            <Tabs value={activeTab} onChange={setActiveTab} variant="segmented">
              <TabsList aria-label="Muteki UI 演示分类">
                <TabsTrigger value="gate" icon="layers">
                  <span data-testid="ui-tab-gate">P0 Gate · 5 项门禁</span>
                </TabsTrigger>
                <TabsTrigger value="base" icon="sliders">
                  <span data-testid="ui-tab-base">Base UI 原语</span>
                </TabsTrigger>
                <TabsTrigger value="agent" icon="bot">
                  <span data-testid="ui-tab-agent">Agent 领域组件</span>
                </TabsTrigger>
                <TabsTrigger value="icons" icon="sparkles">
                  <span data-testid="ui-tab-icons">Icons 与数学引擎</span>
                </TabsTrigger>
              </TabsList>

              <TabsContent value="gate" className="mt-5 flex flex-col gap-5">
                <div className="flex flex-wrap items-center justify-between gap-3">
                  <div>
                    <h2 className="text-cx-sm font-semibold text-cx-fg">@ui-sdk/react WKWebView + Tailwind v4 Gate</h2>
                    <p className="text-cx-xs text-cx-fg-3">
                      验证 Dialog 嵌套 Select 与 Popover、中文 IME Esc 保护、滚动锁定与焦点恢复。
                    </p>
                  </div>
                  <Button
                    variant="primary"
                    size="sm"
                    data-testid="p0-open-dialog"
                    onClick={() => {
                      setDialogOpen(true);
                      pushLog("dialog:open");
                    }}
                  >
                    打开 P0 门禁弹窗
                  </Button>
                </div>

                <div className="grid gap-3 text-cx-sm text-cx-fg-2 sm:grid-cols-2">
                  <div className="rounded-xl border border-cx-border bg-cx-bg-subtle p-4">
                    <div className="text-cx-xs font-medium text-cx-fg-3">当前选中引擎</div>
                    <div data-testid="p0-selected-value" className="mt-1 font-cx-mono text-cx-sm text-cx-fg">
                      {selected}
                    </div>
                  </div>
                  <div className="rounded-xl border border-cx-border bg-cx-bg-subtle p-4">
                    <div className="text-cx-xs font-medium text-cx-fg-3">事件记录</div>
                    <div data-testid="p0-log" className="mt-1 font-cx-mono text-cx-xs text-cx-fg-3">
                      {log.length ? log.join(" · ") : "等待交互"}
                    </div>
                  </div>
                </div>

                <p className="text-cx-xs text-cx-fg-3">
                  页面高度刻意设为 180vh 以测试弹窗打开时的背景滚动锁定与关闭后的滚动恢复。
                </p>
              </TabsContent>

              <TabsContent value="base" className="mt-5 flex flex-col gap-6">
                <div className="grid gap-5 md:grid-cols-2">
                  <Card variant="default">
                    <CardHeader>
                      <CardTitle>Buttons &amp; Actions</CardTitle>
                      <CardDescription>统一高度阶梯、微高光描边与语义确认契约。</CardDescription>
                    </CardHeader>
                    <CardBody className="flex flex-col gap-3">
                      <div className="flex flex-wrap items-center gap-2">
                        <Button variant="primary" size="sm">Primary</Button>
                        <Button variant="secondary" size="sm">Secondary</Button>
                        <Button variant="ghost" size="sm">Ghost</Button>
                        <Button variant="danger" size="sm" agentConfirm="destructive" onClick={() => setConfirmOpen(true)}>
                          Danger · Confirm
                        </Button>
                        <Button variant="secondary" size="sm" loading>Loading</Button>
                      </div>
                      <div className="flex flex-wrap items-center gap-2">
                        <ButtonGroup>
                          <Button size="xs" variant="secondary">Diff</Button>
                          <Button size="xs" variant="secondary">Split</Button>
                          <Button size="xs" variant="secondary">Raw</Button>
                        </ButtonGroup>
                        <IconButton icon="terminal" label="打开终端" size="sm" />
                        <CopyButton text="npm --prefix apps/frontend run build" />
                        <Kbd>⌘K</Kbd>
                        <Shortcut keys="Mod+Shift+P" />
                      </div>
                    </CardBody>
                  </Card>

                  <Card variant="default">
                    <CardHeader>
                      <CardTitle>Forms, Secret Contract &amp; Choices</CardTitle>
                      <CardDescription>含 data-agent-secret 隐私字段与 WAI-ARIA 单选键盘漫游。</CardDescription>
                    </CardHeader>
                    <CardBody className="flex flex-col gap-3">
                      <TextField
                        label="API Key（受保护凭据）"
                        type="password"
                        value={secretToken}
                        onChange={(e) => setSecretToken(e.target.value)}
                        agentSecret
                        data-testid="showcase-secret-input"
                        labelHint="data-agent-secret"
                      />
                      <TextField
                        label="错误态校验"
                        value={noteText}
                        onChange={(e) => setNoteText(e.target.value)}
                        placeholder="输入任意名称"
                        error={noteText.trim() ? undefined : "该字段不能为空（演示 data-invalid）"}
                      />
                      <SearchInput
                        value={searchQuery}
                        onValueChange={setSearchQuery}
                        placeholder="搜索选项（按 Esc 清空）"
                      />
                      <div className="flex flex-wrap items-center gap-4 pt-1">
                        <Checkbox checked={checkedA} onCheckedChange={setCheckedA} label="启用沙箱隔离" />
                        <Switch checked={switchOn} onCheckedChange={setSwitchOn} label="自动折叠工具输出" />
                      </div>
                      <RadioGroup
                        value={radioVal}
                        onValueChange={setRadioVal}
                        ariaLabel="设计准则"
                        options={[
                          { value: "precision", label: "Tactile Precision", description: "1px 边界与顶部内高光" },
                          { value: "compact", label: "Compact Workbench", description: "高密度工作台栅格" },
                          { value: "locked", label: "Disabled Option", disabled: true },
                        ]}
                      />
                      <Slider
                        label="推理温度"
                        value={sliderVal}
                        onValueChange={setSliderVal}
                        valueLabel={`${sliderVal}%`}
                      />
                      <div className="grid gap-2 sm:grid-cols-2">
                        <Select value={selected} onChange={setSelected} options={OPTIONS} ariaLabel="引擎选择" searchable />
                        <NativeSelect value={nativeChoice} onChange={(e) => setNativeChoice(e.target.value)} aria-label="原生下拉">
                          <option value="rust">Rust Server</option>
                          <option value="next">Next.js 15</option>
                          <option value="nsh">macOS NSH</option>
                        </NativeSelect>
                      </div>
                      <TextArea
                        label="多行备注"
                        rows={2}
                        placeholder="支持自动伸缩的文本域"
                        autoResize
                      />
                    </CardBody>
                  </Card>
                </div>

                <div className="grid gap-5 md:grid-cols-2">
                  <Card variant="default">
                    <CardHeader>
                      <CardTitle>Overlays, Accordion &amp; Menus</CardTitle>
                    </CardHeader>
                    <CardBody className="flex flex-col gap-3">
                      <div className="flex flex-wrap items-center gap-2">
                        <Button variant="secondary" size="sm" onClick={() => setSheetOpen(true)}>
                          打开右侧 Sheet
                        </Button>
                        <Button variant="danger" size="sm" agentConfirm="destructive" data-testid="ui-open-confirm" onClick={() => setConfirmOpen(true)}>
                          破坏性确认弹窗
                        </Button>
                        <HoverCard
                          trigger={
                            <Button variant="ghost" size="sm">悬停查看 HoverCard</Button>
                          }
                        >
                          <div className="w-60 p-3 text-cx-xs text-cx-fg-2">
                            由 <code className="font-cx-mono text-cx-accent">@ui-sdk/react/hover-card</code> 驱动，无 HeroUI 依赖。
                          </div>
                        </HoverCard>
                        <Tooltip content="精确对齐的轻量工具提示">
                          <IconButton icon="info" label="查看 Tooltip" size="sm" />
                        </Tooltip>
                        <Menu
                          ariaLabel="工作台操作菜单"
                          trigger={<Button variant="secondary" size="sm">打开 Menu</Button>}
                        >
                          <MenuLabel>工作区操作</MenuLabel>
                          <MenuItem icon="copy" onSelect={() => toast({ title: "已复制路径", tone: "success" })}>
                            复制路径
                          </MenuItem>
                          <MenuItem icon="refresh" onSelect={() => toast({ title: "已刷新索引", tone: "info" })}>
                            重新索引
                          </MenuItem>
                          <MenuSeparator />
                          <MenuItem icon="trash" danger onSelect={() => setConfirmOpen(true)}>
                            销毁隔离状态
                          </MenuItem>
                        </Menu>
                      </div>

                      <Accordion defaultValue={["a1"]}>
                        <AccordionItem value="a1">
                          <AccordionTrigger>WebKit [popover] 高度防塌陷约定</AccordionTrigger>
                          <AccordionContent>
                            Top-layer `[popover]` 内禁用 `flex-1`，滚动正文固定使用 `flex-auto min-h-0` 与 `shrink-0` 头部/尾部。
                          </AccordionContent>
                        </AccordionItem>
                        <AccordionItem value="a2">
                          <AccordionTrigger>CSS Keyframes 渐入替代 JS rAF</AccordionTrigger>
                          <AccordionContent>
                            弹层入场动画通过 `.muteki-dialog-anim` 与 `.muteki-popover-anim` 执行，防止 WebKit 后台节流卡在 opacity 0。
                          </AccordionContent>
                        </AccordionItem>
                      </Accordion>
                    </CardBody>
                  </Card>

                  <Card variant="default">
                    <CardHeader>
                      <CardTitle>Feedback, Status &amp; Toasts</CardTitle>
                    </CardHeader>
                    <CardBody className="flex flex-col gap-3">
                      <div className="flex flex-wrap items-center gap-2">
                        <Badge tone="neutral">Neutral</Badge>
                        <Badge tone="accent" dot>Accent</Badge>
                        <Badge tone="success">Success</Badge>
                        <Badge tone="warning">Warning</Badge>
                        <Badge tone="danger">Danger</Badge>
                        <Chip selected={chipActive} onClick={() => setChipActive((v) => !v)}>
                          Filter Chip
                        </Chip>
                        <StatusDot tone="success" label="Runner 在线" />
                        <DiffStat additions={42} deletions={18} />
                        <Spinner size={15} />
                        <ShimmerText>Agent 正在推理…</ShimmerText>
                      </div>

                      <Alert tone="info" title="Alert / Callout 统一语义">
                        已整合信息提示、警告与执行中状态横幅。
                      </Alert>
                      <Callout tone="success" title="HeroUI 清除完成">
                        运行期零 `@heroui/*`、零 React Aria、零 Radix 依赖。
                      </Callout>

                      <ProgressBar value={74} label="索引进度" valueLabel="74%" />
                      <div className="flex items-center gap-2">
                        <Skeleton className="h-4 w-32" />
                        <Skeleton className="h-4 w-20" />
                      </div>
                      <EmptyState
                        icon="inbox"
                        title="空状态演示"
                        description="统一展示图标、标题与主/次操作。"
                        action={<Button size="xs" variant="secondary" onClick={() => toast({ title: "触发 Toast 成功", description: "来自 @muteki/ui Toaster", tone: "success" })}>弹出 Toast</Button>}
                      />
                    </CardBody>
                    <CardFooter>
                      <div className="flex flex-wrap gap-2">
                        <Button size="xs" variant="secondary" onClick={() => toast({ title: "Info Toast", tone: "info" })}>Info</Button>
                        <Button size="xs" variant="secondary" onClick={() => toast({ title: "Warning Toast", tone: "warning" })}>Warning</Button>
                        <Button size="xs" variant="danger" onClick={() => toast({ title: "Danger Toast", tone: "danger" })}>Danger</Button>
                      </div>
                    </CardFooter>
                  </Card>
                </div>
              </TabsContent>

              <TabsContent value="agent" className="mt-5 flex flex-col gap-6">
                <Card variant="default">
                  <CardHeader>
                    <CardTitle>Message Stream, Thinking &amp; Loading</CardTitle>
                  </CardHeader>
                  <CardBody className="flex flex-col gap-4">
                    <Message from="user">
                      <MessageBubble variant="tint" align="end">
                        <MessageBubbleContent>请统一收口 Agent 对话层组件并确认 WKWebView 兼容性。</MessageBubbleContent>
                      </MessageBubble>
                    </Message>
                    <Message from="assistant">
                      <MessageBubble variant="soft" align="start">
                        <MessageBubbleContent>
                          <div className="flex flex-col gap-3">
                            <Thinking
                              durationMs={3400}
                              summary="已完成 Base UI 与 Agent 领域组件分层解耦，并注入 @ui-sdk/react 行为底座。"
                              rows={[
                                { primary: "Packages split", secondary: "packages/muteki-ui/src/{primitives,agent,engines}", status: "completed" },
                                { primary: "Shiki highlighting", secondary: "lazy oniguruma wasm", status: "completed" },
                              ]}
                            />
                            <AgentMarkdown text={"已完成 **@muteki/ui/agent** 收口，支持 `CodeBlock`、`FileDiff`、`ApprovalCard` 与 `QuestionCard`。\n\n| 模块 | 状态 |\n| --- | --- |\n| Base UI | Ready |\n| Agent UI | Ready |"} />
                            <LoadingState label="实时流式传输验证就绪" elapsedSeconds={4} />
                          </div>
                        </MessageBubbleContent>
                      </MessageBubble>
                    </Message>
                  </CardBody>
                </Card>

                <div className="grid gap-5 lg:grid-cols-2">
                  <div className="flex flex-col gap-4">
                    <ToolResult
                      tool="shell"
                      title="node scripts/lint_rules.mjs"
                      status="success"
                      meta="180ms"
                      defaultOpen
                    >
                      <ToolResultOutput language="bash">
                        {"LINT_RULES_OK: no-playwright, no-heroui-or-legacy-ui"}
                      </ToolResultOutput>
                    </ToolResult>

                    <FileDiff
                      file="apps/frontend/components/ActionBar.tsx"
                      language="tsx"
                      status="complete"
                      defaultOpen
                      lines={[
                        { id: "1", type: "removed", oldLine: 2, content: 'import { Button } from "@heroui/react";' },
                        { id: "2", type: "added", newLine: 2, content: 'import { Button } from "@muteki/ui";' },
                      ]}
                    />

                    <DiffTable
                      title="待审查变更"
                      files={[{
                        path: "components/ActionBar.tsx",
                        additions: 2,
                        deletions: 2,
                        rows: parseUnifiedDiff(SAMPLE_UNIFIED_DIFF),
                      }]}
                    />
                  </div>

                  <div className="flex flex-col gap-4">
                    <CodeBlock
                      filename="muteki-ui/usage.tsx"
                      language="tsx"
                      showLineNumbers
                      wrapToggle
                      code={`import { Button } from "@muteki/ui";\nimport { ApprovalCard } from "@muteki/ui/agent";\n\nexport function GuardedAction() {\n  return <Button variant="danger" agentConfirm="destructive">销毁容器</Button>;\n}`}
                    />
                    <TodoList title="重构任务清单" items={SAMPLE_TODOS} defaultOpen />
                  </div>
                </div>

                <div className="grid gap-5 lg:grid-cols-2">
                  <ApprovalCard
                    title="执行生产包健康冒烟"
                    action="shell.exec"
                    command="node scripts/smoke-web-release.mjs /tmp/muteki-verify-pkg/Muteki-Web.tar.gz"
                    cwd="/workspace/muteki"
                    scope="once"
                    reason={approvalNote}
                    status={approvalStatus}
                    onAllow={() => {
                      setApprovalStatus("resolving");
                      setApprovalNote("已通过 HITL 审批");
                      toast({ title: "HITL 审批已放行", tone: "success" });
                    }}
                    onDeny={() => {
                      setApprovalStatus("expired");
                      setApprovalNote("操作员已拒绝执行");
                      toast({ title: "HITL 操作已拒绝", tone: "warning" });
                    }}
                  />

                  <QuestionCard
                    title="运行前结构化确认"
                    description="单选 / 多选 / 自由文本混合问题流"
                    questions={SAMPLE_QUESTIONS}
                    answers={qaAnswers}
                    onAnswersChange={setQaAnswers}
                    status={qaStatus}
                    submitLabel="提交回答"
                    onSubmit={() => {
                      setQaStatus("answered");
                      toast({ title: "QuestionCard 已提交", tone: "success" });
                    }}
                  />
                </div>

                <AgentPromptBar
                  value={promptValue}
                  onChange={setPromptValue}
                  onSubmit={() => {
                    toast({ title: "发送演示指令", description: promptValue, tone: "info" });
                    setPromptValue("");
                  }}
                  leadingTools={
                    <>
                      <ModelPicker
                        value={promptModel}
                        onChange={setPromptModel}
                        options={OPTIONS.map((o) => ({ value: o.value, label: String(o.label), description: typeof o.description === "string" ? o.description : undefined }))}
                      />
                      <EffortSlider value={promptEffort} onChange={setPromptEffort} />
                    </>
                  }
                  trailingTools={<ContextMeter used={48_000} limit={128_000} />}
                />
              </TabsContent>

              <TabsContent value="icons" className="mt-5 flex flex-col gap-5">
                <div className="flex flex-wrap items-center justify-between gap-3">
                  <SearchInput
                    value={iconFilter}
                    onValueChange={setIconFilter}
                    placeholder="过滤语义图标名称…"
                    className="w-64"
                  />
                  <span className="font-cx-mono text-cx-xs text-cx-fg-3">
                    共展示 {filteredIcons.length} / {SHOWCASE_ICONS.length} 个常用语义图标
                  </span>
                </div>

                <div className="grid grid-cols-3 gap-2 sm:grid-cols-6 md:grid-cols-8">
                  {filteredIcons.map((name) => (
                    <div
                      key={name}
                      className="flex flex-col items-center gap-1.5 rounded-[10px] border border-cx-border bg-cx-elevated p-2.5 text-center"
                    >
                      <Icon name={name} size={18} className="text-cx-fg" />
                      <span className="max-w-full truncate font-cx-mono text-[11px] text-cx-fg-3">
                        {name}
                      </span>
                    </div>
                  ))}
                </div>

                <Card variant="default">
                  <CardHeader>
                    <CardTitle>@muteki/ui/engines/math-view · KaTeX 封装演示</CardTitle>
                  </CardHeader>
                  <CardBody className="flex flex-col gap-3">
                    <div className="flex flex-wrap items-center gap-2 text-cx-sm">
                      <span>行内公式：</span>
                      <MathFormula value="\\mathrm{OKLCH}(L, C, h) \\to \\mathrm{sRGB}" source="$\\mathrm{OKLCH}(L, C, h) \\to \\mathrm{sRGB}$" display={false} />
                    </div>
                    <MathFormula
                      value="\\Delta_{\\text{bundle}} = \\sum_i w_i - w_0"
                      source="$$\\Delta_{\\text{bundle}} = \\sum_i w_i - w_0$$"
                      display
                    />
                  </CardBody>
                </Card>
              </TabsContent>
            </Tabs>
          </div>
        </Card>
      </div>

      <Dialog
        open={dialogOpen}
        onOpenChange={(next) => {
          setDialogOpen(next);
          pushLog(`dialog:${next ? "open" : "close"}`);
        }}
        title="P0 门禁验证：嵌套弹层与中文 IME"
        description="验证 Dialog 内嵌套 Select 与 Popover、中文输入法组合态按 Esc 不误关、焦点返回与滚动锁定。"
        icon="layers"
        tone="accent"
        testId="p0-dialog"
        footer={
          <Button
            variant="primary"
            size="sm"
            data-testid="p0-dialog-confirm"
            onClick={() => setDialogOpen(false)}
          >
            完成并关闭
          </Button>
        }
      >
        <div className="flex flex-col gap-4">
          <div className="flex flex-col gap-1.5">
            <div className="flex items-center justify-between gap-2">
              <label htmlFor="p0-ime-input" className="text-cx-xs font-medium text-cx-fg-2">
                1. 中文 IME 输入框（组合态按 Esc 不应关闭 Dialog）
              </label>
              <div className="flex items-center gap-1.5">
                <button
                  type="button"
                  data-testid="p0-ime-start"
                  onClick={() => {
                    const el = imeInputRef.current;
                    if (!el) return;
                    el.focus();
                    el.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true, data: "nihao" }));
                    pushLog("ime:start");
                  }}
                  className="cx-press h-6 rounded border border-cx-border bg-cx-elevated px-2 text-cx-2xs font-medium text-cx-fg-2 hover:border-cx-border-strong"
                >
                  开始 IME 组合态
                </button>
                <button
                  type="button"
                  data-testid="p0-ime-end"
                  onClick={() => {
                    const el = imeInputRef.current;
                    if (!el) return;
                    el.focus();
                    el.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true, data: "你好" }));
                    pushLog("ime:end");
                  }}
                  className="cx-press h-6 rounded border border-cx-border bg-cx-elevated px-2 text-cx-2xs font-medium text-cx-fg-2 hover:border-cx-border-strong"
                >
                  结束 IME 组合态
                </button>
              </div>
            </div>
            <input
              ref={imeInputRef}
              id="p0-ime-input"
              data-testid="p0-ime-input"
              data-agent-secret="true"
              value={imeInput}
              onChange={(e) => setImeInput(e.target.value)}
              placeholder="在此输入或触发 compositionstart + Esc"
              className="cx-field h-8 rounded-lg border border-cx-border bg-cx-elevated px-3 text-cx-sm text-cx-fg outline-none"
            />
          </div>

          <div className="flex flex-col gap-1.5">
            <span className="text-cx-xs font-medium text-cx-fg-2">2. Dialog 内嵌套可搜索 Select</span>
            <Select
              ariaLabel="选择引擎"
              value={selected}
              onChange={(val) => {
                setSelected(val);
                pushLog(`select:${val}`);
              }}
              options={OPTIONS}
              searchable
              className="w-full"
            />
          </div>

          <div className="flex items-center justify-between gap-3 rounded-xl border border-cx-border bg-cx-bg-subtle p-3">
            <div>
              <div className="text-cx-sm font-medium text-cx-fg">3. Dialog 内嵌套 Popover</div>
              <div className="text-cx-xs text-cx-fg-3">Esc 应先关闭 Popover，再按一次 Esc 才关闭 Dialog</div>
            </div>
            <Popover
              open={popoverOpen}
              onOpenChange={(next) => {
                setPopoverOpen(next);
                pushLog(`popover:${next ? "open" : "close"}`);
              }}
              placement="bottom-end"
              trigger={
                <Button variant="secondary" size="xs" data-testid="p0-open-popover">
                  打开 Popover
                </Button>
              }
            >
              {({ close }) => (
                <div data-testid="p0-popover-panel" className="flex w-60 flex-col gap-2 p-3">
                  <div className="text-cx-sm font-medium text-cx-fg">嵌套浮层</div>
                  <p className="text-cx-xs text-cx-fg-3">点击这里不应关闭外层 Dialog。</p>
                  <Button
                    size="xs"
                    variant="secondary"
                    data-testid="p0-popover-close"
                    onClick={close}
                    className="self-end"
                  >
                    关闭浮层
                  </Button>
                </div>
              )}
            </Popover>
          </div>
        </div>
      </Dialog>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title="确认销毁演示隔离状态？"
        description="携带 data-agent-confirm=destructive 的安全确认契约。"
        agentConfirm="destructive"
        confirmLabel="确认执行"
        testId="ui-confirm-dialog"
        onConfirm={() => {
          setConfirmOpen(false);
          toast({ title: "破坏性确认示例已触发", tone: "warning" });
        }}
      />

      <Sheet
        open={sheetOpen}
        onOpenChange={setSheetOpen}
        side="right"
        title="右侧滑出 Sheet"
        description="支持右侧 / 左侧 / 底部抽屉与焦点隔离。"
        testId="ui-demo-sheet"
      >
        <p className="text-cx-sm text-cx-fg-2">
          Sheet 内部正文使用 `flex-auto min-h-0` 防止在受限高度视口下塌陷。
        </p>
      </Sheet>
    </div>
  );
}
