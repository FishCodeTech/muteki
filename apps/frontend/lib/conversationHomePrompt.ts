import type { IconName } from "@muteki/ui";
import type { WorkspaceBindMode } from "@/lib/useConversation";

export type HomeTimePeriod =
  | "dawn"
  | "morning"
  | "midday"
  | "afternoon"
  | "evening"
  | "late_night"
  | "after_midnight";

export interface HomeClockSnapshot {
  period: HomeTimePeriod;
  dayKey: string;
  isWeekend: boolean;
  hour: number;
}

export interface ConversationHomeSuggestion {
  icon: IconName;
  label: string;
  prompt: string;
  workspace?: boolean;
}

export interface HomePromptContext {
  clock: HomeClockSnapshot | null;
  seed?: number;
  projectName?: string;
  hasProjects?: boolean;
  workspaceMode?: WorkspaceBindMode;
  worktreeBranch?: string;
  interactionMode?: "default" | "plan";
  attachmentCount?: number;
}

export interface ResolvedHomePrompt {
  title: string;
  subtitle: string;
  placeholder: string;
  suggestions: ConversationHomeSuggestion[];
}

export const NEUTRAL_HOME_TITLE = "随时待命，现在想推进点什么？";

const TITLE_POOLS: Record<HomeTimePeriod, { base: readonly string[]; weekend?: readonly string[] }> = {
  dawn: {
    base: [
      "起得真早，今天想先推进哪件事？",
      "清晨很安静，从哪一块开始？",
      "天刚亮，先把头绪交给 Agent 理一理？",
      "早安，趁思路清爽，先开工点什么？",
    ],
    weekend: ["周末起得真早，今天想折腾点什么？"],
  },
  morning: {
    base: [
      "早上好，今天准备攻克什么？",
      "上午思路正清，先从哪一块下手？",
      "新的一轮开始了，今天想推进点什么？",
      "早安，把今天的第一个任务交给我吧",
    ],
    weekend: ["周末早上好，今天想折腾点什么？"],
  },
  midday: {
    base: [
      "中午好，手头有什么想顺手解决的？",
      "午间好，趁热把想法落地吧",
      "正午时分，接下来想推进哪一步？",
      "中午好，聊个思路，还是直接开工？",
    ],
    weekend: ["周末中午好，有什么灵感先记下来聊聊？"],
  },
  afternoon: {
    base: [
      "下午好，现在想专注解决什么？",
      "午后适合攻坚，交给我一起拆解？",
      "下午好，接下来想把哪一块收掉？",
      "状态在线，手头哪件事需要搭把手？",
    ],
    weekend: ["周末午后，正好慢慢打磨一个想法"],
  },
  evening: {
    base: [
      "晚上好，今天还想收尾哪件事？",
      "夜幕降临，接下来想一起推进什么？",
      "晚上好，继续收个尾，还是聊聊新想法？",
      "傍晚好，还有什么方案想顺手跑通？",
    ],
    weekend: ["周末晚上好，想探索点什么新点子？"],
  },
  late_night: {
    base: [
      "夜色渐深，还有哪一步想今晚收掉？",
      "这么晚还在忙，交给我来分担一点？",
      "夜晚安静，正好专心把这一块理顺",
      "今晚的最后一项，想从哪开始？",
    ],
  },
  after_midnight: {
    base: [
      "凌晨了，我在，想快点解决哪件事？",
      "夜深人静，把繁琐的活交给我来跑",
      "还在守夜？挑最关键的一步先推进吧",
      "凌晨时段，我们直奔主题，尽快收工",
    ],
  },
};

const WORKSPACE_DEFAULT_SUGGESTIONS: readonly ConversationHomeSuggestion[] = [
  {
    icon: "gitCompare",
    label: "审查当前改动",
    prompt: "审查当前工作区的未提交改动，指出潜在问题并给出修改建议。",
    workspace: true,
  },
  {
    icon: "book",
    label: "解释这个项目",
    prompt: "阅读这个项目，概述它的架构、关键模块和运行方式。",
    workspace: true,
  },
  {
    icon: "wrench",
    label: "修复失败的测试",
    prompt: "运行测试，定位失败原因并修复，完成后再次验证。",
    workspace: true,
  },
  {
    icon: "listTodo",
    label: "起草实现方案",
    prompt: "为下面的需求起草一个分步骤的实现方案，并说明取舍：\n\n",
  },
];

const GENERAL_DEFAULT_SUGGESTIONS: readonly ConversationHomeSuggestion[] = [
  {
    icon: "listTodo",
    label: "起草实现方案",
    prompt: "为下面的需求起草一个分步骤的实现方案，并说明取舍：\n\n",
  },
  {
    icon: "wrench",
    label: "排查报错线索",
    prompt: "帮我分析下面这段报错或异常现象，给出排查步骤与可能根因：\n\n",
  },
  {
    icon: "code",
    label: "审查这段代码",
    prompt: "帮我审查下面这段代码，指出边界情况、潜在缺陷与简化思路：\n\n",
  },
  {
    icon: "lightbulb",
    label: "拆解复杂问题",
    prompt: "我想理清下面这个问题，帮我拆成可执行的步骤并标注关键决策点：\n\n",
  },
];

const WORKSPACE_PLAN_SUGGESTIONS: readonly ConversationHomeSuggestion[] = [
  {
    icon: "listTodo",
    label: "拆解功能方案",
    prompt: "结合当前项目结构，为下面的需求拆解一份分步骤实现方案与风险点：\n\n",
    workspace: true,
  },
  {
    icon: "compass",
    label: "评估重构边界",
    prompt: "检查当前项目相关模块，评估我准备进行的重构影响面与更稳妥的改造顺序：\n\n",
    workspace: true,
  },
  {
    icon: "gitCompare",
    label: "评审当前改动",
    prompt: "先只读审查当前工作区的未提交改动，评估架构取舍、遗漏边界与后续计划。",
    workspace: true,
  },
  {
    icon: "lightbulb",
    label: "对比技术选型",
    prompt: "针对下面的实现目标，给出两到三种可行方案并对比取舍：\n\n",
  },
];

const GENERAL_PLAN_SUGGESTIONS: readonly ConversationHomeSuggestion[] = [
  {
    icon: "listTodo",
    label: "起草实现方案",
    prompt: "为下面的需求起草一个分步骤的实现方案，并说明取舍：\n\n",
  },
  {
    icon: "compass",
    label: "梳理架构边界",
    prompt: "帮我梳理下面这个模块或系统的职责边界、核心接口与演进风险：\n\n",
  },
  {
    icon: "lightbulb",
    label: "对比技术选型",
    prompt: "针对下面的实现目标，给出两到三种可行方案并对比取舍：\n\n",
  },
  {
    icon: "listChecks",
    label: "制定验证清单",
    prompt: "围绕下面的功能或改动，帮我列一份上线前可执行的验证清单与边界用例：\n\n",
  },
];

function pad2(value: number): string {
  return String(value).padStart(2, "0");
}

export function resolveHomeTimePeriod(hour: number): HomeTimePeriod {
  const h = ((Math.floor(hour) % 24) + 24) % 24;
  if (h >= 5 && h < 8) return "dawn";
  if (h >= 8 && h < 11) return "morning";
  if (h >= 11 && h < 14) return "midday";
  if (h >= 14 && h < 18) return "afternoon";
  if (h >= 18 && h < 22) return "evening";
  if (h >= 22 || h < 1) return "late_night";
  return "after_midnight";
}

export function readHomeClock(now: Date = new Date()): HomeClockSnapshot {
  const hour = now.getHours();
  const day = now.getDay();
  const dayKey = `${now.getFullYear()}-${pad2(now.getMonth() + 1)}-${pad2(now.getDate())}`;
  return {
    period: resolveHomeTimePeriod(hour),
    dayKey,
    isWeekend: day === 0 || day === 6,
    hour,
  };
}

export function sameHomeClockSlot(a: HomeClockSnapshot | null, b: HomeClockSnapshot | null): boolean {
  return Boolean(a && b && a.period === b.period && a.dayKey === b.dayKey && a.isWeekend === b.isWeekend);
}

export function msUntilNextTimeSlot(now: Date = new Date()): number {
  const boundaries = [1, 5, 8, 11, 14, 18, 22, 24];
  const hour = now.getHours();
  const boundary = boundaries.find((b) => b > hour) ?? 24;
  const next = new Date(now.getFullYear(), now.getMonth(), now.getDate(), boundary, 0, 0, 250);
  return Math.max(1000, next.getTime() - now.getTime());
}

export function initialHomePromptSeed(now: Date = new Date()): number {
  return now.getDate() * 31 + now.getHours() * 7 + Math.floor(now.getMinutes() / 10);
}

export function formatProjectLabel(name?: string): string {
  const trimmed = (name ?? "").trim();
  if (!trimmed) return "";
  return trimmed.length > 24 ? `${trimmed.slice(0, 23)}…` : trimmed;
}

export function resolveHomeTitle(clock: HomeClockSnapshot | null, seed = 0): string {
  if (!clock) return NEUTRAL_HOME_TITLE;
  const entry = TITLE_POOLS[clock.period];
  const pool = clock.isWeekend && entry.weekend?.length ? [...entry.base, ...entry.weekend] : entry.base;
  const normalizedSeed = Math.trunc(seed);
  const index = ((normalizedSeed % pool.length) + pool.length) % pool.length;
  return pool[index];
}

export function resolveHomeSubtitle(ctx: HomePromptContext): string {
  const projectLabel = formatProjectLabel(ctx.projectName);
  const branch = (ctx.worktreeBranch ?? "").trim();
  const attachments = Math.max(0, Math.floor(ctx.attachmentCount ?? 0));

  if (attachments > 0) {
    if (projectLabel) {
      return `已附带 ${attachments} 个附件，结合「${projectLabel}」说说你想让我怎么处理。`;
    }
    return `已附带 ${attachments} 个附件，直接说说你想让我分析、整理还是动手处理。`;
  }

  if (ctx.interactionMode === "plan") {
    if (projectLabel) {
      return `规划模式 · 先基于「${projectLabel}」一起拆解方案与取舍，确认后再动手。`;
    }
    return "规划模式 · 先一起梳理目标、边界和步骤，不急着直接改动。";
  }

  if (projectLabel) {
    if (ctx.workspaceMode === "new_worktree") {
      return branch
        ? `将在「${projectLabel}」的新工作树 ${branch} 中隔离执行，不打扰主目录。`
        : `将在「${projectLabel}」新建隔离工作树执行，原目录保持干净。`;
    }
    if (ctx.workspaceMode === "existing_worktree") {
      return `已指向「${projectLabel}」的指定工作树，随时可以读代码、查改动或跑命令验证。`;
    }
    return `已连接「${projectLabel}」，可以直接读代码、查改动或运行命令验证。`;
  }

  if (ctx.hasProjects) {
    return "直接提问或贴代码就能开聊；需要读写文件、查改动或跑命令时，在下方选个工作目录即可。";
  }
  return "直接开聊、丢报错或理思路都可以；想让 Agent 动手改项目，从下方添加一个工作目录即可。";
}

export function resolveHomePlaceholder(ctx: HomePromptContext): string {
  const projectLabel = formatProjectLabel(ctx.projectName);
  const attachments = Math.max(0, Math.floor(ctx.attachmentCount ?? 0));

  if (attachments > 0) {
    return "说说要对这些附件做什么…";
  }
  if (ctx.interactionMode === "plan") {
    return projectLabel
      ? `在「${projectLabel}」里想规划什么功能、排查或重构？`
      : "描述你的目标或需求，我们一起先拆解方案与取舍…";
  }
  if (projectLabel) {
    return `在「${projectLabel}」里想解决什么？贴报错、提需求或直接下任务…`;
  }
  return "聊聊想法、贴一段代码或报错，随时开始…";
}

export function resolveHomeSuggestions(
  ctx: Pick<HomePromptContext, "projectName" | "interactionMode"> & { hasSelectedProject?: boolean },
): ConversationHomeSuggestion[] {
  const hasProject = Boolean(ctx.hasSelectedProject ?? ctx.projectName?.trim());
  const source = ctx.interactionMode === "plan"
    ? (hasProject ? WORKSPACE_PLAN_SUGGESTIONS : GENERAL_PLAN_SUGGESTIONS)
    : (hasProject ? WORKSPACE_DEFAULT_SUGGESTIONS : GENERAL_DEFAULT_SUGGESTIONS);
  return source.map((item) => ({ ...item }));
}

export function resolveConversationHomePrompt(
  ctx: HomePromptContext & { hasSelectedProject?: boolean },
): ResolvedHomePrompt {
  return {
    title: resolveHomeTitle(ctx.clock, ctx.seed ?? 0),
    subtitle: resolveHomeSubtitle(ctx),
    placeholder: resolveHomePlaceholder(ctx),
    suggestions: resolveHomeSuggestions(ctx),
  };
}
