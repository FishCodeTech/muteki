import type {
  ControlSettingsPatch,
  OrchestrationProfile,
  OrchestrationRole,
  OrchestrationScopeSource,
  OrchestrationWorkbenchMode,
} from "@/lib/controlGrants";

export const DEFAULT_ORCHESTRATION_ROLES: OrchestrationRole[] = [
  {
    id: "planner",
    label: "规划与架构",
    description: "需求拆解、技术选型与实现计划。",
    enabled: true,
    mode: "inherit",
    provider: "",
    profile: "",
    model: "",
    effort: "high",
    guidelines: "输出清晰步骤、文件影响面和验收条件；不确定的架构决策先指出权衡。",
  },
  {
    id: "implementer",
    label: "编码实现",
    description: "编写功能、修复缺陷并运行检查。",
    enabled: true,
    mode: "inherit",
    provider: "",
    profile: "",
    model: "",
    effort: "medium",
    guidelines: "先读现有调用链再做最小修改，保留无关代码与注释，改完必须跑对应检查。",
  },
  {
    id: "reviewer",
    label: "交叉审查与挑刺",
    description: "独立审查改动的正确性、回归风险与遗漏。",
    enabled: true,
    mode: "inherit",
    provider: "",
    profile: "",
    model: "",
    effort: "high",
    guidelines: "只基于实际读取的文件和运行结果下结论；发现问题直接给出文件名、行号与修复建议。",
  },
  {
    id: "researcher",
    label: "代码检索与调研",
    description: "检索代码、定位调用链、总结资料。",
    enabled: true,
    mode: "inherit",
    provider: "",
    profile: "",
    model: "",
    effort: "low",
    guidelines: "高信噪比汇报：只返回准确文件路径、符号名与关键源码摘录。",
  },
  {
    id: "verifier",
    label: "实机测试与验收",
    description: "启动服务、操作界面、查看日志并实机验收。",
    enabled: true,
    mode: "inherit",
    provider: "",
    profile: "",
    model: "",
    effort: "medium",
    guidelines: "必须亲自跑命令或浏览器操作看到退出码与界面结果，严禁用推测代替验收。",
  },
];

export function cloneOrchestrationProfile(profile: OrchestrationProfile): OrchestrationProfile {
  return {
    preset: profile.preset || "balanced",
    auto_inject_instructions: profile.auto_inject_instructions ?? true,
    default_role: profile.default_role?.trim() ?? "",
    roles: (profile.roles ?? []).map((role) => ({ ...role })),
  };
}

export function defaultOrchestrationProfile(): OrchestrationProfile {
  return {
    preset: "balanced",
    auto_inject_instructions: true,
    default_role: "",
    roles: DEFAULT_ORCHESTRATION_ROLES.map((role) => ({ ...role })),
  };
}

export function normalizeOrchestrationProfile(
  profile?: OrchestrationProfile | null,
): OrchestrationProfile {
  if (!profile) {
    return defaultOrchestrationProfile();
  }
  return cloneOrchestrationProfile(profile);
}

export function resolveModeOrchestration(
  base: OrchestrationProfile,
  srcOverride: OrchestrationProfile | null,
  mode: OrchestrationWorkbenchMode,
): {
  profile: OrchestrationProfile;
  inherited: boolean;
  scope: OrchestrationScopeSource;
} {
  if (mode === "src") {
    if (srcOverride !== null) {
      return {
        profile: srcOverride,
        inherited: false,
        scope: "mode_override",
      };
    }
    return {
      profile: base,
      inherited: true,
      scope: "default",
    };
  }
  return {
    profile: base,
    inherited: false,
    scope: "default",
  };
}

export function createSrcOverrideFromBase(base: OrchestrationProfile): OrchestrationProfile {
  return cloneOrchestrationProfile(base);
}

export function computeOrchestrationDirty(
  savedChat: OrchestrationProfile,
  draftChat: OrchestrationProfile,
  savedSrc: OrchestrationProfile | null,
  draftSrc: OrchestrationProfile | null,
): { chatDirty: boolean; srcDirty: boolean; anyDirty: boolean } {
  const chatDirty =
    JSON.stringify(normalizeOrchestrationProfile(draftChat))
    !== JSON.stringify(normalizeOrchestrationProfile(savedChat));
  const normSrc = (value: OrchestrationProfile | null) =>
    value ? JSON.stringify(normalizeOrchestrationProfile(value)) : null;
  const srcDirty = normSrc(draftSrc) !== normSrc(savedSrc);
  return {
    chatDirty,
    srcDirty,
    anyDirty: chatDirty || srcDirty,
  };
}

export function buildOrchestrationSavePatches(
  savedChat: OrchestrationProfile,
  draftChat: OrchestrationProfile,
  savedSrc: OrchestrationProfile | null,
  draftSrc: OrchestrationProfile | null,
): ControlSettingsPatch[] {
  const { chatDirty, srcDirty } = computeOrchestrationDirty(
    savedChat,
    draftChat,
    savedSrc,
    draftSrc,
  );
  const patches: ControlSettingsPatch[] = [];
  if (chatDirty) {
    patches.push({
      orchestration_mode: "conversation",
      orchestration: normalizeOrchestrationProfile(draftChat),
    });
  }
  if (srcDirty) {
    patches.push({
      orchestration_mode: "src",
      orchestration: draftSrc ? normalizeOrchestrationProfile(draftSrc) : null,
    });
  }
  return patches;
}
