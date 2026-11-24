"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Badge,
  Button,
  Callout,
  Input,
  SegmentedControl,
  Select,
  Skeleton,
  StatusDot,
  Switch,
  TextArea,
  toast,
  useCopy,
  type ListOption,
} from "@muteki/ui";
import { SettingsDisclosure, SettingsRow, SettingsSection } from "@/components/settings/primitives";
import { SettingsLink } from "@/components/settings/SettingsHost";
import { SubagentPolicySettings } from "@/components/settings/SubagentPolicySettings";
import { cn } from "@/lib/cn";
import {
  fetchControlSettings,
  fetchOrchestrationProviders,
  updateControlSettings,
  type ControlSettings,
  type SubagentPolicy,
  type OrchestrationProfile,
  type OrchestrationRole,
  type OrchestrationRuntimeItem,
  type OrchestrationRuntimeModel,
  type OrchestrationWorkbenchMode,
} from "@/lib/controlGrants";
import {
  DEFAULT_ORCHESTRATION_ROLES,
  buildOrchestrationSavePatches,
  cloneOrchestrationProfile,
  computeOrchestrationDirty,
  createSrcOverrideFromBase,
  defaultOrchestrationProfile,
  normalizeOrchestrationProfile,
  resolveModeOrchestration,
} from "@/lib/orchestrationDraft";

const CUSTOM_MODEL_SENTINEL = "__custom_model__";

const CORE_ROLE_IDS = new Set([
  "planner",
  "implementer",
  "reviewer",
  "researcher",
  "verifier",
]);

const DEFAULT_EFFORT_BY_ROLE: Record<string, string> = {
  planner: "high",
  implementer: "medium",
  reviewer: "high",
  researcher: "low",
  verifier: "medium",
};

const EFFORT_OPTIONS: ListOption<string>[] = [
  { value: "", label: "自动" },
  { value: "low", label: "低" },
  { value: "medium", label: "中" },
  { value: "high", label: "高" },
  { value: "xhigh", label: "极高" },
];

const ENGINE_DISPLAY: Record<string, string> = {
  claude: "Claude Code",
  codex: "OpenAI Codex",
  gemini: "Gemini CLI",
  pi: "Pi Agent",
  opencode: "OpenCode",
};

function CopyAction({ text, label }: { text: string; label: string }) {
  const { copied, copy } = useCopy();
  return (
    <Button size="sm" variant="secondary" icon={copied ? "check" : "copy"} onClick={() => void copy(text)}>
      {copied ? "已复制" : label}
    </Button>
  );
}

const PRESET_LABEL = {
  "cross-review": "异构审校",
  inherit: "跟随主会话",
  "cost-split": "成本分层",
} as const;

function dedupeRuntimes(items: OrchestrationRuntimeItem[]): OrchestrationRuntimeItem[] {
  const byKey = new Map<string, OrchestrationRuntimeItem>();
  for (const item of items) {
    const key = runtimeKey(item.engine, item.credential_id);
    const prev = byKey.get(key);
    if (
      !prev ||
      (!prev.available && item.available) ||
      (prev.available === item.available && (item.models?.length ?? 0) > (prev.models?.length ?? 0))
    ) {
      byKey.set(key, item);
    }
  }
  return [...byKey.values()];
}

function engineTitle(engine: string): string {
  return ENGINE_DISPLAY[engine] ?? engine;
}

function findPreferredRuntime(
  runtimes: OrchestrationRuntimeItem[],
  engine: string,
  credentialId?: string,
): OrchestrationRuntimeItem | undefined {
  const candidates = runtimes.filter(
    (r) => r.engine === engine && (!credentialId || r.credential_id === credentialId),
  );
  candidates.sort((a, b) => {
    if (a.available !== b.available) return a.available ? -1 : 1;
    return (b.models?.length ?? 0) - (a.models?.length ?? 0);
  });
  return candidates[0];
}

function runtimeKey(engine: string, credentialId: string): string {
  return `${engine}::${credentialId}`;
}

function splitRuntimeKey(key: string): { provider: string; profile: string } {
  const idx = key.indexOf("::");
  if (idx < 0) return { provider: key, profile: "" };
  return { provider: key.slice(0, idx), profile: key.slice(idx + 2) };
}

function pickAuthoritativeDefaultModel(runtime: OrchestrationRuntimeItem): string {
  const models = runtime.models ?? [];
  const defaultModel = runtime.default_model?.trim() ?? "";
  const verifiedModels = models.filter((m) => m.verified);
  if (verifiedModels.length > 0) {
    if (defaultModel && verifiedModels.some((m) => m.id === defaultModel)) {
      return defaultModel;
    }
    return verifiedModels[0].id;
  }
  if (defaultModel) {
    return defaultModel;
  }
  return "";
}

function modelUnitPrice(model: OrchestrationRuntimeModel): number | null {
  const pricing = model.pricing;
  if (!pricing || (pricing.status !== "catalog" && pricing.status !== "custom")) {
    return null;
  }
  if (
    typeof pricing.input_per_mtok !== "number" ||
    typeof pricing.output_per_mtok !== "number"
  ) {
    return null;
  }
  const total = pricing.input_per_mtok + pricing.output_per_mtok;
  return Number.isFinite(total) ? total : null;
}

function formatModelPriceTag(model: OrchestrationRuntimeModel): string {
  const baseTag = model.verified ? "已验证" : "目录";
  const pricing = model.pricing;
  if (
    pricing &&
    (pricing.status === "catalog" || pricing.status === "custom") &&
    typeof pricing.input_per_mtok === "number" &&
    typeof pricing.output_per_mtok === "number"
  ) {
    const fmt = (value: number) =>
      value >= 1 ? value.toFixed(value % 1 === 0 ? 0 : 2) : value.toFixed(2);
    return `${baseTag} · $${fmt(pricing.input_per_mtok)}/$${fmt(pricing.output_per_mtok)}`;
  }
  return baseTag;
}

function buildPresetRoles(
  preset: "cross-review" | "inherit" | "cost-split",
  currentRoles: OrchestrationRole[],
  runtimes: OrchestrationRuntimeItem[],
): { profileRoles: OrchestrationRole[]; note?: string } {
  const customRoles = currentRoles.filter((role) => !CORE_ROLE_IDS.has(role.id));
  const byId = new Map(currentRoles.map((role) => [role.id, role]));
  const baseRoles = DEFAULT_ORCHESTRATION_ROLES.map((def) => {
    const existing = byId.get(def.id);
    return existing ? { ...def, ...existing } : { ...def };
  });

  const makeInheritRoles = () =>
    [...baseRoles, ...customRoles].map((role) => ({
      ...role,
      enabled: true,
      mode: "inherit" as const,
      provider: "",
      profile: "",
      model: "",
      effort: DEFAULT_EFFORT_BY_ROLE[role.id] ?? (role.effort || "medium"),
    }));

  if (preset === "inherit") {
    return { profileRoles: makeInheritRoles() };
  }

  const ready = runtimes.filter(
    (r) => r.available && (r.models?.length > 0 || Boolean(r.default_model)),
  );
  if (ready.length === 0) {
    return {
      profileRoles: makeInheritRoles(),
      note: "没有就绪的 Agent，已改为全部跟随主会话",
    };
  }

  const sortedReady = [...ready].sort((a, b) => {
    const av = (a.models ?? []).filter((m) => m.verified).length;
    const bv = (b.models ?? []).filter((m) => m.verified).length;
    if ((av > 0) !== (bv > 0)) return av > 0 ? -1 : 1;
    if (av !== bv) return bv - av;
    const ad = Boolean(a.default_model);
    const bd = Boolean(b.default_model);
    if (ad !== bd) return ad ? -1 : 1;
    return a.engine.localeCompare(b.engine) || a.credential_id.localeCompare(b.credential_id);
  });

  const primary = sortedReady[0];
  const secondary =
    sortedReady.find((r) => r.engine !== primary.engine) ?? sortedReady[1] ?? primary;

  const pin = (
    role: OrchestrationRole,
    runtime: OrchestrationRuntimeItem,
    model: string,
    effort: string,
  ): OrchestrationRole => ({
    ...role,
    enabled: true,
    mode: "pinned",
    provider: runtime.engine,
    profile: runtime.credential_id,
    model,
    effort,
  });

  if (preset === "cost-split") {
    const pricedCandidates: {
      runtime: OrchestrationRuntimeItem;
      modelId: string;
      unitPrice: number;
    }[] = [];
    for (const rt of sortedReady) {
      for (const model of rt.models ?? []) {
        if (!model.verified) continue;
        const unitPrice = modelUnitPrice(model);
        if (unitPrice !== null) {
          pricedCandidates.push({ runtime: rt, modelId: model.id, unitPrice });
        }
      }
    }
    pricedCandidates.sort((a, b) => b.unitPrice - a.unitPrice);
    const highest = pricedCandidates[0];
    const lowest = pricedCandidates[pricedCandidates.length - 1];
    if (!highest || !lowest || highest.unitPrice <= lowest.unitPrice) {
      return {
        profileRoles: makeInheritRoles(),
        note: "已验证模型缺少两档以上的价格，无法分层，已改为全部跟随主会话",
      };
    }

    const middle =
      pricedCandidates.find(
        (candidate) =>
          candidate.modelId !== highest.modelId && candidate.modelId !== lowest.modelId,
      ) ?? {
        runtime: secondary,
        modelId: pickAuthoritativeDefaultModel(secondary),
        unitPrice: highest.unitPrice,
      };

    const mapped = baseRoles.map((role) => {
      switch (role.id) {
        case "planner":
        case "reviewer":
          return pin(role, highest.runtime, highest.modelId, "high");
        case "researcher":
          return pin(role, lowest.runtime, lowest.modelId, "low");
        case "implementer":
        case "verifier":
        default:
          return pin(role, middle.runtime, middle.modelId, "medium");
      }
    });
    return { profileRoles: [...mapped, ...customRoles] };
  }

  const primaryModel = pickAuthoritativeDefaultModel(primary);
  const secondaryModel = pickAuthoritativeDefaultModel(secondary);
  const mapped = baseRoles.map((role) => {
    switch (role.id) {
      case "planner":
      case "reviewer":
        return pin(role, primary, primaryModel, "high");
      case "implementer":
        return pin(role, secondary, secondaryModel, "high");
      case "researcher":
        return pin(role, secondary, secondaryModel, "low");
      case "verifier":
      default:
        return pin(role, secondary, secondaryModel, "medium");
    }
  });

  const distinctEngines = new Set(ready.map((r) => r.engine)).size;
  const note =
    distinctEngines < 2
      ? "只有 1 种就绪引擎，无法异构配对"
      : undefined;

  return { profileRoles: [...mapped, ...customRoles], note };
}

function formatInstructionsPreview(
  profile: OrchestrationProfile,
  mode: OrchestrationWorkbenchMode,
  isIndependentSrc: boolean,
): string {
  const enabled = profile.roles.filter((r) => r.enabled);
  if (!profile.auto_inject_instructions || enabled.length === 0) {
    return "（未注入）";
  }
  const modeLine =
    mode === "src"
      ? isIndependentSrc
        ? "Active mode: src (independent override)."
        : "Active mode: src (inheriting conversation default)."
      : "Active mode: conversation (default).";
  const lines = [
    "## User Multi-Agent Orchestration Roster (Muteki Setup)",
    modeLine,
    "When dispatching tasks with `muteki_start_thread` (or native subagents), follow the user's configured role roster unless overridden in this conversation:",
  ];
  const defaultRole = profile.default_role?.trim() ?? "";
  if (defaultRole) {
    const matched = enabled.find((role) => role.id.toLowerCase() === defaultRole.toLowerCase());
    if (matched) {
      lines.push(`Default role when omitted (with no explicit provider/model): \`${matched.id}\`.`);
    }
  }
  for (const role of enabled) {
    let routing: string;
    if (role.mode === "pinned" && role.provider) {
      const parts = [`provider=${role.provider}`];
      if (role.profile) parts.push(`profile=${role.profile}`);
      if (role.model) parts.push(`model=${role.model}`);
      if (role.effort) parts.push(`reasoningEffort=${role.effort}`);
      routing = parts.join(", ");
    } else if (role.effort) {
      routing = `inherit current session agent/model (suggested reasoningEffort=${role.effort}; for standalone threads set provider/model from current session or muteki_list_providers)`;
    } else {
      routing = "inherit current session agent/model";
    }
    lines.push(`- role \`${role.id}\` (${role.label}): ${routing}`);
    if (role.description) lines.push(`  scope: ${role.description}`);
    if (role.guidelines) lines.push(`  guidelines: ${role.guidelines}`);
  }
  lines.push(
    "You can inspect live ready agents/models anytime via `muteki_list_providers` and this roster via `muteki_get_settings` (key `orchestration`).",
  );
  return lines.join("\n");
}

export function OrchestrationSettings() {
  const [settings, setSettings] = useState<ControlSettings | null>(null);
  const [activeMode, setActiveMode] = useState<OrchestrationWorkbenchMode>("conversation");
  const [savedChat, setSavedChat] = useState<OrchestrationProfile>(defaultOrchestrationProfile);
  const [draftChat, setDraftChat] = useState<OrchestrationProfile>(defaultOrchestrationProfile);
  const [savedSrc, setSavedSrc] = useState<OrchestrationProfile | null>(null);
  const [draftSrc, setDraftSrc] = useState<OrchestrationProfile | null>(null);
  const [runtimes, setRuntimes] = useState<OrchestrationRuntimeItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [refreshingProviders, setRefreshingProviders] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [providersError, setProvidersError] = useState<string | null>(null);
  const [expandedGuidelines, setExpandedGuidelines] = useState<Record<string, boolean>>({});
  const [customModelInputs, setCustomModelInputs] = useState<Record<string, boolean>>({});
  const [addingCustom, setAddingCustom] = useState(false);
  const [newRoleId, setNewRoleId] = useState("");
  const [newRoleLabel, setNewRoleLabel] = useState("");

  const syncFromSettings = useCallback((cfg: ControlSettings) => {
    const nextChat = normalizeOrchestrationProfile(cfg.orchestration);
    const nextSrc = cfg.orchestration_overrides?.src
      ? normalizeOrchestrationProfile(cfg.orchestration_overrides.src)
      : null;
    setSavedChat(nextChat);
    setDraftChat(cloneOrchestrationProfile(nextChat));
    setSavedSrc(nextSrc);
    setDraftSrc(nextSrc ? cloneOrchestrationProfile(nextSrc) : null);
  }, []);

  const loadAll = useCallback(async (onlyProviders = false) => {
    if (onlyProviders) {
      setRefreshingProviders(true);
    } else {
      setLoading(true);
      setError(null);
    }
    setProvidersError(null);

    const [cfgResult, invResult] = await Promise.allSettled([
      onlyProviders && settings ? Promise.resolve(settings) : fetchControlSettings(),
      fetchOrchestrationProviders(),
    ]);

    if (!onlyProviders || !settings) {
      if (cfgResult.status === "fulfilled") {
        const cfg = cfgResult.value;
        setSettings(cfg);
        syncFromSettings(cfg);
      } else {
        setError(
          cfgResult.reason instanceof Error
            ? cfgResult.reason.message
            : "加载失败",
        );
      }
    }

    if (invResult.status === "fulfilled") {
      const inv = invResult.value;
      setRuntimes(dedupeRuntimes(inv.runtimes ?? inv.providers ?? []));
    } else {
      setProvidersError(
        invResult.reason instanceof Error
          ? invResult.reason.message
          : "读取 Agent 列表失败",
      );
    }

    setLoading(false);
    setRefreshingProviders(false);
  }, [settings, syncFromSettings]);

  useEffect(() => {
    void loadAll(false);
    // Initial page mount only.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const { profile: activeDraft, inherited: isInheritedView } = useMemo(
    () => resolveModeOrchestration(draftChat, draftSrc, activeMode),
    [draftChat, draftSrc, activeMode],
  );

  const { chatDirty, srcDirty, anyDirty: dirty } = useMemo(
    () => computeOrchestrationDirty(savedChat, draftChat, savedSrc, draftSrc),
    [savedChat, draftChat, savedSrc, draftSrc],
  );

  const readyRuntimes = useMemo(
    () => runtimes.filter((r) => r.available && (r.models?.length > 0 || Boolean(r.default_model))),
    [runtimes],
  );

  const unavailableRuntimes = useMemo(
    () => runtimes.filter((r) => !(r.available && (r.models?.length > 0 || Boolean(r.default_model)))),
    [runtimes],
  );

  const readyCount = readyRuntimes.length;

  const gatewayEnabled = settings?.enabled ?? true;

  const clearCustomInputsForMode = (mode: OrchestrationWorkbenchMode) => {
    const prefix = `${mode}:`;
    setCustomModelInputs((curr) => {
      const next = { ...curr };
      for (const key of Object.keys(next)) {
        if (key.startsWith(prefix)) {
          delete next[key];
        }
      }
      return next;
    });
  };

  const mutateActiveProfile = (updater: (curr: OrchestrationProfile) => OrchestrationProfile) => {
    if (isInheritedView) return;
    if (activeMode === "conversation") {
      setDraftChat((curr) => updater(curr));
    } else {
      setDraftSrc((curr) => (curr ? updater(curr) : curr));
    }
  };

  const enableSrcOverride = () => {
    setDraftSrc(createSrcOverrideFromBase(draftChat));
    clearCustomInputsForMode("src");
    toast({ title: "已为 SRC 启用独立编排方案", tone: "success" });
  };

  const revertSrcToInherit = () => {
    setDraftSrc(null);
    setAddingCustom(false);
    clearCustomInputsForMode("src");
    toast({ title: "SRC 将恢复跟随聊天配置，保存后生效", tone: "success" });
  };

  const enableCollabQuick = async () => {
    try {
      const updated = await updateControlSettings({ enabled: true });
      setSettings(updated);
      toast({ title: "已开启", tone: "success" });
    } catch (err) {
      toast({ title: err instanceof Error ? err.message : "开启失败", tone: "danger" });
    }
  };

  const saveChildPolicy = async (policy: SubagentPolicy) => {
    setSaving(true);
    try {
      const updated = await updateControlSettings({ subagent_policy: policy });
      setSettings(updated);
      toast({ title: "已保存子 Agent 权限设置", tone: "success" });
    } catch (err) {
      toast({ title: err instanceof Error ? err.message : "保存子 Agent 权限失败", tone: "danger" });
    } finally {
      setSaving(false);
    }
  };

  const saveDraft = async () => {
    const patches = buildOrchestrationSavePatches(savedChat, draftChat, savedSrc, draftSrc);
    if (patches.length === 0) return;
    setSaving(true);
    try {
      for (const patch of patches) {
        const updated = await updateControlSettings(patch);
        setSettings(updated);
        if (patch.orchestration_mode === "conversation") {
          const nextChat = normalizeOrchestrationProfile(updated.orchestration);
          setSavedChat(nextChat);
          setDraftChat(cloneOrchestrationProfile(nextChat));
        } else if (patch.orchestration_mode === "src") {
          const nextSrc = updated.orchestration_overrides?.src
            ? normalizeOrchestrationProfile(updated.orchestration_overrides.src)
            : null;
          setSavedSrc(nextSrc);
          setDraftSrc(nextSrc ? cloneOrchestrationProfile(nextSrc) : null);
        }
      }
      setCustomModelInputs({});
      toast({ title: "已保存", tone: "success" });
    } catch (err) {
      toast({ title: err instanceof Error ? err.message : "保存失败", tone: "danger" });
    } finally {
      setSaving(false);
    }
  };

  const resetDefaults = () => {
    if (isInheritedView) return;
    mutateActiveProfile(() => defaultOrchestrationProfile());
    clearCustomInputsForMode(activeMode);
    toast({ title: "已恢复默认角色", tone: "success" });
  };

  const handleApplyPreset = (preset: "cross-review" | "inherit" | "cost-split") => {
    if (isInheritedView) return;
    const { profileRoles, note } = buildPresetRoles(preset, activeDraft.roles, runtimes);
    mutateActiveProfile((curr) => ({
      ...curr,
      preset,
      roles: profileRoles,
    }));
    clearCustomInputsForMode(activeMode);
    if (note) {
      toast({ title: note, tone: "warning" });
    } else {
      toast({ title: `已套用「${PRESET_LABEL[preset]}」`, tone: "success" });
    }
  };

  const updateRole = (roleId: string, updater: (role: OrchestrationRole) => OrchestrationRole) => {
    mutateActiveProfile((curr) => ({
      ...curr,
      preset: "custom",
      roles: curr.roles.map((r) => (r.id === roleId ? updater(r) : r)),
    }));
  };

  const runtimeOptions: ListOption<string>[] = useMemo(() => {
    const byKey = new Map<string, OrchestrationRuntimeItem>();
    for (const r of runtimes) {
      const k = runtimeKey(r.engine, r.credential_id);
      const prev = byKey.get(k);
      if (!prev || (!prev.available && r.available) || r.models.length > prev.models.length) {
        byKey.set(k, r);
      }
    }
    const sorted = [...byKey.values()].sort((a, b) => {
      if (a.available !== b.available) return a.available ? -1 : 1;
      return a.engine.localeCompare(b.engine);
    });
    return sorted.map((r) => ({
      value: runtimeKey(r.engine, r.credential_id),
      label: `${engineTitle(r.engine)} · ${r.credential_id}`,
      textValue: `${r.engine} ${engineTitle(r.engine)} ${r.credential_id}`,
      description: r.available
        ? `就绪 · ${r.models.length} 个模型${r.default_model ? ` · 默认 ${r.default_model}` : ""}`
        : `不可用：${r.reason || "未配置或无模型目录"}`,
      disabled: !r.available && r.models.length === 0 && !r.default_model,
    }));
  }, [runtimes]);

  const handleAddCustomRole = () => {
    if (isInheritedView) return;
    const cleanId = newRoleId
      .trim()
      .toLowerCase()
      .replace(/[^a-z0-9_-]+/g, "_")
      .replace(/^_+|_+$/g, "");
    if (!cleanId) {
      toast({ title: "请输入角色 ID", tone: "warning" });
      return;
    }
    if (activeDraft.roles.some((r) => r.id === cleanId)) {
      toast({ title: `${cleanId} 已存在`, tone: "warning" });
      return;
    }
    const label = newRoleLabel.trim() || cleanId;
    const role: OrchestrationRole = {
      id: cleanId,
      label,
      description: "",
      enabled: true,
      mode: "inherit",
      provider: "",
      profile: "",
      model: "",
      effort: "medium",
      guidelines: "专注该角色职责并给出可验证证据。",
    };
    mutateActiveProfile((curr) => ({
      ...curr,
      preset: "custom",
      roles: [...curr.roles, role],
    }));
    setExpandedGuidelines((curr) => ({ ...curr, [cleanId]: true }));
    setAddingCustom(false);
    setNewRoleId("");
    setNewRoleLabel("");
  };

  const removeCustomRole = (roleId: string) => {
    if (isInheritedView) return;
    mutateActiveProfile((curr) => ({
      ...curr,
      preset: "custom",
      roles: curr.roles.filter((r) => r.id !== roleId),
    }));
  };

  const defaultRoleOptions: ListOption<string>[] = useMemo(() => {
    const enabledRoles = activeDraft.roles.filter((r) => r.enabled);
    const opts: ListOption<string>[] = [{ value: "", label: "不指定（跟随主会话）" }];
    for (const r of enabledRoles) {
      opts.push({ value: r.id, label: `${r.label} (${r.id})` });
    }
    const currentDefault = activeDraft.default_role?.trim() ?? "";
    if (currentDefault && !enabledRoles.some((r) => r.id === currentDefault)) {
      opts.push({
        value: currentDefault,
        label: `${currentDefault}（未启用或不存在）`,
        disabled: true,
      });
    }
    return opts;
  }, [activeDraft.roles, activeDraft.default_role]);

  const instructionsPreview = useMemo(
    () => formatInstructionsPreview(activeDraft, activeMode, !isInheritedView && activeMode === "src"),
    [activeDraft, activeMode, isInheritedView],
  );

  if (loading && !settings) {
    return (
      <div className="flex flex-col gap-6" aria-busy="true">
        <Skeleton className="h-24 w-full rounded-2xl" />
        <Skeleton className="h-40 w-full rounded-2xl" />
        <Skeleton className="h-72 w-full rounded-2xl" />
      </div>
    );
  }

  if (!settings) {
    return (
      <div className="flex flex-col gap-6">
        <Callout
          tone="danger"
          icon="alert"
          title="读取编排配置失败"
          action={
            <Button size="sm" variant="secondary" onClick={() => void loadAll(false)}>
              重试
            </Button>
          }
        >
          {error || "无法读取设置"}
        </Callout>
      </div>
    );
  }

  const enabledCount = activeDraft.roles.filter((r) => r.enabled).length;
  const dirtyLabel = chatDirty && srcDirty
    ? "未保存（聊天、SRC）"
    : srcDirty
      ? "未保存（SRC）"
      : "未保存（聊天）";

  return (
    <div className="flex flex-col gap-5">
      {!gatewayEnabled ? (
        <Callout
          tone="warning"
          icon="alert"
          title="协作网关未开启"
          action={
            <Button size="sm" variant="primary" onClick={() => void enableCollabQuick()}>
              开启
            </Button>
          }
        />
      ) : null}

      <SubagentPolicySettings
        anchor="orchestration-subagent-policy"
        policy={settings.subagent_policy}
        disabled={saving}
        onChange={(policy) => void saveChildPolicy(policy)}
      />

      <SettingsSection anchor="orchestration-modes">
        <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-3">
          <div className="flex flex-wrap items-center gap-3">
            <SegmentedControl
              ariaLabel="编排工作台模式"
              size="sm"
              value={activeMode}
              options={[
                { value: "conversation", label: chatDirty ? "聊天（默认） · 未保存" : "聊天（默认）" },
                { value: "src", label: srcDirty ? "SRC · 未保存" : "SRC" },
              ]}
              onChange={(mode) => {
                setActiveMode(mode as OrchestrationWorkbenchMode);
                setAddingCustom(false);
              }}
            />
            {activeMode === "conversation" ? (
              <Badge tone="neutral" data-testid="orchestration-mode-status">默认基准</Badge>
            ) : isInheritedView ? (
              <Badge tone="neutral" data-testid="orchestration-src-status">跟随聊天</Badge>
            ) : (
              <Badge tone="accent" data-testid="orchestration-src-status">独立方案</Badge>
            )}
          </div>
          <div className="flex items-center gap-2">
            {activeMode === "src" ? (
              isInheritedView ? (
                <Button
                  size="sm"
                  variant="primary"
                  data-testid="orchestration-src-enable-override"
                  onClick={enableSrcOverride}
                >
                  使用独立方案
                </Button>
              ) : (
                <Button
                  size="sm"
                  variant="ghost"
                  data-testid="orchestration-src-revert-inherit"
                  onClick={revertSrcToInherit}
                >
                  恢复跟随聊天
                </Button>
              )
            ) : (
              <span className="text-cx-xs text-cx-fg-3">
                {draftSrc === null ? "SRC 当前继承此默认配置" : "SRC 已启用独立覆盖"}
              </span>
            )}
          </div>
        </div>
      </SettingsSection>

      <SettingsSection
        anchor="orchestration-roles"
        title="角色"
        actions={
          <div className="flex flex-wrap items-center gap-2">
            <SegmentedControl
              ariaLabel="方案"
              size="sm"
              value={activeDraft.preset}
              options={(Object.keys(PRESET_LABEL) as (keyof typeof PRESET_LABEL)[]).map((id) => ({
                value: id,
                label: PRESET_LABEL[id],
                disabled: isInheritedView,
              }))}
              onChange={(id) => handleApplyPreset(id as keyof typeof PRESET_LABEL)}
            />
            <Button
              size="sm"
              variant="ghost"
              icon="plus"
              iconOnly
              disabled={isInheritedView}
              aria-label="添加角色"
              tooltip="添加角色"
              onClick={() => setAddingCustom((v) => !v)}
            />
            <Button
              size="sm"
              variant="ghost"
              icon="refresh"
              iconOnly
              disabled={isInheritedView}
              aria-label="恢复默认角色"
              tooltip="恢复默认角色"
              onClick={resetDefaults}
            />
          </div>
        }
      >
        {addingCustom && !isInheritedView ? (
          <div className="flex flex-wrap items-center gap-2 border-b border-cx-border-subtle bg-cx-bg-subtle/40 px-5 py-3">
            <Input
              className="w-44"
              value={newRoleId}
              onChange={(e) => setNewRoleId(e.target.value)}
              placeholder="ID（security_auditor）"
              aria-label="角色 ID"
            />
            <Input
              className="w-40"
              value={newRoleLabel}
              onChange={(e) => setNewRoleLabel(e.target.value)}
              placeholder="名称"
              aria-label="角色名称"
            />
            <div className="ml-auto flex items-center gap-2">
              <Button size="sm" variant="ghost" onClick={() => setAddingCustom(false)}>
                取消
              </Button>
              <Button size="sm" variant="primary" onClick={handleAddCustomRole}>
                添加
              </Button>
            </div>
          </div>
        ) : null}

        {activeDraft.roles.map((role) => {
          const isPinned = role.mode === "pinned" && Boolean(role.provider);
          const currentRuntimeKey = isPinned
            ? runtimeKey(role.provider, role.profile || `system:${role.provider}`)
            : "";
          const matchedRuntime = isPinned
            ? findPreferredRuntime(runtimes, role.provider, role.profile)
            : undefined;

          const routeOptions: ListOption<string>[] = [
            { value: "", label: "跟随主会话" },
            ...(isPinned && !runtimeOptions.some((opt) => opt.value === currentRuntimeKey)
              ? [
                  {
                    value: currentRuntimeKey,
                    label: `${engineTitle(role.provider)} · ${role.profile || "默认"}`,
                    description: "当前不在线",
                  },
                ]
              : []),
            ...runtimeOptions,
          ];

          const catalogModels = matchedRuntime?.models ?? [];
          const modelInCatalog = !role.model || catalogModels.some((m) => m.id === role.model);
          const customInputKey = `${activeMode}:${role.id}`;
          const customModel = Boolean(customModelInputs[customInputKey]) || (isPinned && !modelInCatalog);

          const modelOptions: ListOption<string>[] = [
            {
              value: "",
              label: matchedRuntime?.default_model ? `默认 · ${matchedRuntime.default_model}` : "默认模型",
            },
            ...catalogModels.map((m) => ({
              value: m.id,
              label: m.id,
              trailing: formatModelPriceTag(m),
            })),
            { value: CUSTOM_MODEL_SENTINEL, label: "自定义…" },
          ];

          const isCore = CORE_ROLE_IDS.has(role.id);
          const showDetails = Boolean(expandedGuidelines[role.id]);

          return (
            <div
              key={role.id}
              data-testid={`orchestration-role-${role.id}`}
              className={cn(
                "border-b border-cx-border-subtle px-5 py-3 last:border-b-0",
                isInheritedView && "opacity-80",
              )}
            >
              <div className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-2 sm:grid-cols-6 lg:grid-cols-[170px_minmax(0,1fr)_minmax(0,1fr)_88px_auto]">
                <div className="flex min-w-0 items-center gap-2.5 sm:col-span-4 lg:col-span-1">
                  <Switch
                    checked={role.enabled}
                    disabled={isInheritedView}
                    ariaLabel={`启用 ${role.label}`}
                    onCheckedChange={(enabled) => updateRole(role.id, (curr) => ({ ...curr, enabled }))}
                  />
                  <span
                    className={cn("truncate text-cx-sm font-medium", role.enabled ? "text-cx-fg" : "text-cx-fg-3")}
                    title={role.description || role.id}
                  >
                    {role.label}
                  </span>
                </div>

                <Select
                  size="sm"
                  className="col-span-2 w-full sm:col-span-2 lg:col-span-1"
                  ariaLabel={`${role.label} Agent`}
                  value={currentRuntimeKey}
                  searchable={routeOptions.length > 7}
                  disabled={isInheritedView || !role.enabled}
                  options={routeOptions}
                  onChange={(key) => {
                    setCustomModelInputs((curr) => ({ ...curr, [customInputKey]: false }));
                    if (!key) {
                      updateRole(role.id, (curr) => ({ ...curr, mode: "inherit", provider: "", profile: "", model: "" }));
                      return;
                    }
                    const { provider, profile } = splitRuntimeKey(key);
                    const rt = findPreferredRuntime(runtimes, provider, profile);
                    updateRole(role.id, (curr) => ({
                      ...curr,
                      mode: "pinned",
                      provider,
                      profile,
                      model: rt ? pickAuthoritativeDefaultModel(rt) : "",
                    }));
                  }}
                />

                {customModel ? (
                  <Input
                    className="col-span-2 h-7 text-cx-xs sm:col-span-2 lg:col-span-1"
                    value={role.model}
                    disabled={isInheritedView || !role.enabled}
                    placeholder="模型 ID"
                    aria-label={`${role.label} 模型 ID`}
                    onChange={(e) => updateRole(role.id, (curr) => ({ ...curr, model: e.target.value }))}
                  />
                ) : (
                  <Select
                    size="sm"
                    className="col-span-2 w-full sm:col-span-2 lg:col-span-1"
                    ariaLabel={`${role.label} 模型`}
                    value={isPinned ? role.model : ""}
                    placeholder="—"
                    searchable={modelOptions.length > 8}
                    disabled={isInheritedView || !role.enabled || !isPinned}
                    options={modelOptions}
                    onChange={(val) => {
                      if (val === CUSTOM_MODEL_SENTINEL) {
                        setCustomModelInputs((curr) => ({ ...curr, [customInputKey]: true }));
                        return;
                      }
                      updateRole(role.id, (curr) => ({ ...curr, model: val }));
                    }}
                  />
                )}

                <Select
                  size="sm"
                  className="col-span-2 w-full sm:col-span-2 lg:col-span-1"
                  ariaLabel={`${role.label} 推理深度`}
                  value={role.effort || ""}
                  options={EFFORT_OPTIONS}
                  disabled={isInheritedView || !role.enabled}
                  onChange={(effort) => updateRole(role.id, (curr) => ({ ...curr, effort }))}
                />

                <div className="col-start-2 row-start-1 flex items-center justify-end gap-0.5 sm:col-span-2 sm:col-start-5 sm:row-start-1 lg:col-span-1 lg:col-start-auto lg:row-start-auto">
                  {!isCore ? (
                    <Button
                      size="sm"
                      variant="ghost"
                      icon="trash"
                      iconOnly
                      disabled={isInheritedView}
                      aria-label={`删除 ${role.label}`}
                      tooltip="删除"
                      onClick={() => removeCustomRole(role.id)}
                    />
                  ) : null}
                  <Button
                    size="sm"
                    variant="ghost"
                    icon={showDetails ? "chevronDown" : "chevronRight"}
                    iconOnly
                    aria-label={`${role.label} 详情`}
                    aria-expanded={showDetails}
                    tooltip="守则"
                    onClick={() => setExpandedGuidelines((curr) => ({ ...curr, [role.id]: !curr[role.id] }))}
                  />
                </div>
              </div>

              {showDetails ? (
                <div className="mt-3 grid grid-cols-1 gap-2">
                  <div className="grid grid-cols-1 gap-2 sm:grid-cols-[180px_minmax(0,1fr)]">
                    <Input
                      value={role.label}
                      disabled={isInheritedView}
                      aria-label={`${role.id} 名称`}
                      placeholder="名称"
                      onChange={(e) => updateRole(role.id, (curr) => ({ ...curr, label: e.target.value }))}
                    />
                    <Input
                      value={role.description}
                      disabled={isInheritedView}
                      aria-label={`${role.id} 职责`}
                      placeholder="职责"
                      onChange={(e) => updateRole(role.id, (curr) => ({ ...curr, description: e.target.value }))}
                    />
                  </div>
                  <TextArea
                    autoResize
                    maxRows={5}
                    disabled={isInheritedView}
                    aria-label={`${role.id} 守则`}
                    value={role.guidelines}
                    placeholder="守则"
                    onChange={(e) => updateRole(role.id, (curr) => ({ ...curr, guidelines: e.target.value }))}
                  />
                </div>
              ) : null}
            </div>
          );
        })}
      </SettingsSection>

      <SettingsDisclosure
        anchor="orchestration-inventory"
        title="可用 Agent"
        icon="bot"
        meta={
          <>
            <StatusDot tone={readyCount > 0 ? "success" : "warning"} />
            <span>
              {readyCount} / {runtimes.length}
            </span>
          </>
        }
      >
        <div className="flex items-center justify-end gap-2 border-b border-cx-border-subtle px-5 py-2">
          <Button size="sm" variant="ghost" icon="refresh" loading={refreshingProviders} onClick={() => void loadAll(true)}>
            刷新
          </Button>
          <SettingsLink
            href="/settings/agents"
            className="inline-flex h-7 items-center rounded-lg px-2.5 text-cx-xs font-medium text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg"
          >
            管理
          </SettingsLink>
        </div>
        {providersError ? (
          <div className="px-5 py-3 text-cx-sm text-cx-danger">{providersError}</div>
        ) : null}
        <ul className="divide-y divide-cx-border-subtle">
          {[...readyRuntimes, ...unavailableRuntimes].map((item) => {
            const ready = item.available && (item.models.length > 0 || Boolean(item.default_model));
            return (
              <li
                key={runtimeKey(item.engine, item.credential_id)}
                className={cn("flex items-center gap-2.5 px-5 py-2 text-cx-sm", !ready && "text-cx-fg-3")}
              >
                <StatusDot tone={ready ? "success" : "neutral"} />
                <span className="font-medium">{engineTitle(item.engine)}</span>
                <span className="truncate font-mono text-cx-xs text-cx-fg-3">{item.credential_id}</span>
                <span className="ml-auto truncate text-cx-xs text-cx-fg-3">
                  {ready
                    ? `${item.models.length} 个模型${item.default_model ? ` · ${item.default_model}` : ""}`
                    : item.reason || "不可用"}
                </span>
              </li>
            );
          })}
        </ul>
      </SettingsDisclosure>

      <SettingsSection anchor="orchestration-instructions">
        <SettingsRow
          title="未指定角色时的默认角色"
          description="当委派未指定 role 且未显式指定 Agent/模型时，自动套用此角色；不指定则继承当前主会话。"
          control={
            <Select
              size="sm"
              className="w-56"
              ariaLabel="未指定角色时的默认角色"
              value={activeDraft.default_role?.trim() ?? ""}
              options={defaultRoleOptions}
              disabled={isInheritedView}
              onChange={(default_role) => mutateActiveProfile((curr) => ({ ...curr, default_role }))}
            />
          }
        />
        <SettingsRow
          title="注入主会话"
          control={
            <Switch
              checked={activeDraft.auto_inject_instructions}
              disabled={isInheritedView}
              ariaLabel="注入主会话"
              onCheckedChange={(auto_inject_instructions) =>
                mutateActiveProfile((curr) => ({ ...curr, auto_inject_instructions }))
              }
            />
          }
        />
      </SettingsSection>

      <SettingsDisclosure title="注入内容预览" meta={<span>{enabledCount} 个角色</span>}>
        <div className="flex flex-col gap-3 px-5 py-4">
          <div>
            <CopyAction text={instructionsPreview} label="复制" />
          </div>
          <pre className="cx-scroll max-h-72 overflow-auto rounded-xl border border-cx-border-subtle bg-cx-bg p-3.5 font-mono text-cx-xs leading-5 whitespace-pre-wrap text-cx-fg-2">
            {instructionsPreview}
          </pre>
        </div>
      </SettingsDisclosure>

      {dirty ? (
        <div
          role="region"
          aria-label="未保存的更改"
          className="sticky bottom-4 z-(--z-raised) flex flex-wrap items-center justify-between gap-3 rounded-2xl bg-cx-elevated py-2.5 pl-5 pr-2.5 shadow-cx-pop"
        >
          <span className="text-cx-sm text-cx-fg-2">{dirtyLabel}</span>
          <div className="flex items-center gap-2">
            <Button
              variant="ghost"
              disabled={saving}
              onClick={() => {
                setDraftChat(cloneOrchestrationProfile(savedChat));
                setDraftSrc(savedSrc ? cloneOrchestrationProfile(savedSrc) : null);
                setCustomModelInputs({});
                setAddingCustom(false);
              }}
            >
              放弃
            </Button>
            <Button variant="primary" loading={saving} onClick={() => void saveDraft()}>
              保存
            </Button>
          </div>
        </div>
      ) : null}
    </div>
  );
}
