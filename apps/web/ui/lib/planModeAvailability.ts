/**
 * Whether the composer may offer interaction_mode "plan".
 *
 * `planMode` is the resolved capability (descriptor overlaid with the probe).
 * `undefined` means the catalog is ready but the field is absent.
 */

export type InteractionMode = "default" | "plan";

export interface PlanModeSupport {
  /** False while descriptors are still loading; that is not a hard rejection. */
  known: boolean;
  available: boolean;
  /** Chinese explanation shown when the toggle is disabled. */
  reason: string;
}

export function normalizeInteractionMode(value: unknown): InteractionMode {
  return value === "plan" ? "plan" : "default";
}

export function planModeSupportForRuntime(input: {
  catalogReady: boolean;
  runtimeKey: string;
  planMode: boolean | undefined;
}): PlanModeSupport {
  if (!input.runtimeKey.trim()) {
    return {
      known: true,
      available: false,
      reason: "请先选择 Agent 接入后再使用规划模式",
    };
  }
  if (!input.catalogReady) {
    return {
      known: false,
      available: false,
      reason: "正在确认当前 Runtime 是否支持规划模式",
    };
  }
  if (input.planMode === true) {
    return { known: true, available: true, reason: "" };
  }
  if (input.planMode === false) {
    return {
      known: true,
      available: false,
      reason: "当前 Runtime 不支持规划模式（能力描述 plan_mode=false，conversation.interaction_mode_unsupported）",
    };
  }
  return {
    known: true,
    available: false,
    reason: "当前 Runtime 不支持规划模式（conversation.interaction_mode_unsupported）",
  };
}
