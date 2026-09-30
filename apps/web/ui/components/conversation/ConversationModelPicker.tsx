"use client";

import { useEffect, useId, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { useLang } from "@/lib/i18n";
import {
  favoriteToggleAnnouncement,
  isFavoriteToggleKey,
  isImeComposingKeyEvent,
  resolveActiveIndexAfterListChange,
  scrollActiveOptionIntoView,
} from "@/lib/modelPickerListKeyboard";
import { NARROW_MODEL_PICKER_MQ } from "@/lib/modelPickerNarrowLayout";
import { modelEffortLevels, rememberedModelEffort, rememberModelEffort } from "@/lib/modelReasoning";
import { useMediaQuery } from "@/lib/useMediaQuery";
import { EngineLogo } from "@/components/EngineLogo";
import { Icon, type IconName } from "@/components/Icon";
import {
  Badge,
  Button,
  Callout,
  EmptyState,
  Popover,
  SearchInput,
  SegmentedControl,
  Select,
  Skeleton,
  StatusDot,
  Tooltip,
  useControllableOpen,
  useListKeyboard,
  type ListOption,
  type Tone,
} from "@/components/chat/ui";
import {
  allCredentialModels,
  CONVERSATION_WORKER_ENGINES,
  isConversationWorkerEngine,
  type ConversationCredential,
  type ConversationCredentialModel,
  type RuntimeInstance,
} from "@/lib/useConversation";

export interface ConversationModelPickerProps {
  credentials: ConversationCredential[];
  selectedCredentialId: string;
  selectedModel: string;
  selectedEffort: string;
  selectedAccessMode?: string;
  accessModes?: string[];
  runtimes?: RuntimeInstance[];
  runtimeKey?: string;
  onRuntimeChange?: (key: string) => void;
  loading?: boolean;
  error?: string;
  onRetry?: () => void;
  variant?: "conversation" | "default-model";
  onSelect: (params: { credentialId: string; model: string; effort?: string; accessMode?: string }) => void;
  className?: string;
  /** Controlled popover state (e.g. bound to mod+shift+m by the Shell). */
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
}

const ENGINE_LABELS: Record<string, string> = {
  codex: "Codex",
  claude: "Claude Code",
  cursor: "Cursor",
  grok: "Grok",
  opencode: "OpenCode",
  pi: "Pi",
  kimi: "Kimi Code",
  omp: "OMP",
  devin: "Devin CLI",
};
const EFFORT_LABELS: Record<string, string> = {
  "": "默认", none: "关闭", minimal: "极低", low: "低", medium: "中",
  high: "高", xhigh: "超高", max: "最大", off: "关闭", on: "开启", ultra: "极高",
};

function accessModeMeta(mode: string): {
  label: string;
  detail: string;
  icon: IconName;
} {
  const modes: Record<string, { label: string; detail: string; icon: IconName }> = {
    supervised: {
      label: "严格监督",
      detail: "执行命令和修改文件前先询问。",
      icon: "lock",
    },
    "auto-accept-edits": {
      label: "自动接受修改",
      detail: "自动批准文件修改，其他操作先询问。",
      icon: "pencil",
    },
    auto: {
      label: "自动",
      detail: "Agent 支持的常规操作自动执行，其余操作先询问。",
      icon: "sparkles",
    },
    "full-access": {
      label: "完全访问",
      detail: "执行命令和修改文件时不再询问。",
      icon: "shieldAlert",
    },
  };
  if (modes[mode]) {
    return modes[mode];
  }
  return {
    label: mode,
    detail: `由当前 Agent 提供的访问模式：${mode}`,
    icon: "lock",
  };
}

function engineLabel(engine: string): string {
  return ENGINE_LABELS[engine] || engine;
}

function usableCredential(credential: ConversationCredential): boolean {
  return credential.present !== false
    && !["missing", "absent", "failed", "invalid", "error", "unavailable"].includes(
      credential.status.toLowerCase(),
    );
}

function credentialTone(credential: ConversationCredential): Tone {
  const status = credential.status.toLowerCase();
  if (credential.present === false || status === "missing" || status === "absent") return "neutral";
  if (!usableCredential(credential)) return "danger";
  if (credential.last_test && credential.last_test.ok === false) return "warning";
  return "success";
}

function endpointKind(credential: ConversationCredential): string {
  if (credential.source === "system") return "宿主登录";
  if (credential.connection === "custom_endpoint") return "自定义端点";
  if (credential.provider) return credential.provider;
  return "账号";
}

function matchesQuery(needle: string, model: ConversationCredentialModel): boolean {
  if (!needle) return true;
  return `${model.label} ${model.id}`.toLowerCase().includes(needle);
}

const FAVORITE_MODELS_KEY = "muteki.conversation.favorite-models.v1";

interface FavoriteModel {
  credentialId: string;
  modelId: string;
}

function favoriteKey(credentialId: string, modelId: string): string {
  return `${credentialId}\0${modelId}`;
}

function readFavoriteModels(): FavoriteModel[] {
  try {
    const saved = JSON.parse(localStorage.getItem(FAVORITE_MODELS_KEY) || "[]");
    if (!Array.isArray(saved)) return [];
    const rows: FavoriteModel[] = [];
    const seen = new Set<string>();
    for (const item of saved) {
      const credentialId = String(item?.credentialId ?? "").trim();
      const modelId = String(item?.modelId ?? "").trim();
      const key = favoriteKey(credentialId, modelId);
      if (!credentialId || !modelId || seen.has(key)) continue;
      seen.add(key);
      rows.push({ credentialId, modelId });
    }
    return rows;
  } catch {
    return [];
  }
}

function persistFavoriteModels(rows: FavoriteModel[]): void {
  try {
    localStorage.setItem(FAVORITE_MODELS_KEY, JSON.stringify(rows));
  } catch {
    // Conversation model favorites are optional when storage is unavailable.
  }
}

function effortLabelOf(effort: string): string {
  return EFFORT_LABELS[effort] || effort || EFFORT_LABELS[""];
}

function credentialStatusLabel(credential: ConversationCredential | undefined): string {
  if (!credential) return "";
  const status = credential.status.toLowerCase();
  if (credential.present === false || status === "missing" || status === "absent") return "未登录";
  if (status === "unavailable" || status === "failed" || status === "invalid" || status === "error") {
    return credential.status_detail || "不可用";
  }
  return "";
}

interface ModelRow {
  key: string;
  credential: ConversationCredential;
  model: ConversationCredentialModel;
}

function modelBadges(credential: ConversationCredential, model: ConversationCredentialModel): Array<{ label: string; tone: Tone; detail?: string }> {
  const badges: Array<{ label: string; tone: Tone; detail?: string }> = [];
  if (credential.default_model === model.id) badges.push({ label: "默认", tone: "accent" });
  if (model.reasoning?.supported || model.reasoning?.levels?.length) badges.push({ label: "推理", tone: "neutral" });
  if (!credential.models.some((probed) => probed.id === model.id)) badges.push({
    label: "未验证", tone: "warning",
    detail: "此凭据下还没有该模型的成功使用记录，仍可选择。成功完成一次聊天后会自动标记为已验证，也可在设置 → Agents 中执行「真实连通测试」。刷新模型列表不会验证模型。",
  });
  return badges;
}

function SettingsLink() {
  return (
    <a
      href="/settings/agents"
      className="inline-flex items-center gap-1 text-[12px] font-medium text-cx-accent hover:underline hover:underline-offset-4"
    >
      前往 Agents 设置
      <Icon name="arrowUpRight" size={12} />
    </a>
  );
}

function PickerSkeleton() {
  return (
    <div className="flex h-[300px]" data-testid="conversation-model-loading" role="status" aria-label="加载中">
      <div className="flex w-[168px] shrink-0 flex-col gap-2 border-r border-cx-border-subtle bg-cx-bg-subtle p-3">
        {[0, 1, 2, 3].map((row) => <Skeleton key={row} className="h-6 w-full" />)}
      </div>
      <div className="flex flex-1 flex-col gap-3 p-3">
        <Skeleton className="h-8 w-full rounded-lg" />
        {[0, 1, 2, 3, 4].map((row) => (
          <div key={row} className="flex flex-col gap-1.5 px-1">
            <Skeleton className="h-3 w-1/2" />
            <Skeleton className="h-2.5 w-1/3" />
          </div>
        ))}
      </div>
    </div>
  );
}

export function ConversationModelPicker({
  credentials, selectedCredentialId, selectedModel, selectedEffort,
  selectedAccessMode = "supervised", accessModes = [], loading = false, error = "", onRetry, variant = "conversation",
  runtimes, runtimeKey = "", onRuntimeChange,
  onSelect, className = "", open: openProp, onOpenChange,
}: ConversationModelPickerProps) {
  const { lang } = useLang();
  const [open, setOpen] = useControllableOpen(openProp, false, onOpenChange);
  const [query, setQuery] = useState("");
  const [favorites, setFavorites] = useState<FavoriteModel[]>([]);
  const [favoritesMode, setFavoritesMode] = useState(false);
  // #200: ≤480px stack endpoint rail above models so long ids wrap.
  const narrowStacked = useMediaQuery(NARROW_MODEL_PICKER_MQ);
  const [favoriteStatus, setFavoriteStatus] = useState("");
  const listId = useId();
  const modelListScrollRef = useRef<HTMLDivElement>(null);
  const listedCredentials = useMemo(
    () => credentials.filter((credential) => isConversationWorkerEngine(credential.engine)),
    [credentials],
  );
  const availableCredentials = useMemo(
    () => listedCredentials.filter(usableCredential),
    [listedCredentials],
  );
  // Exact selection only — never fall back for the trigger label. Falling back
  // made unbound threads (Provider import) look selected while send still
  // required an endpoint (#136). Browse fallback is for the open menu only.
  const boundCredential = useMemo(
    () => listedCredentials.find((item) => item.id === selectedCredentialId),
    [listedCredentials, selectedCredentialId],
  );
  const runtimeCandidates = useMemo(
    () => boundCredential ? (runtimes || []).filter(runtime => runtime.engine === boundCredential.engine) : [],
    [boundCredential, runtimes],
  );
  const showRuntime = variant === "conversation" && (runtimes !== undefined || onRuntimeChange !== undefined);
  const runtimeDisabled = !onRuntimeChange || !boundCredential || boundCredential.present === false
    || !runtimeCandidates.some(runtime => runtime.enabled !== false);
  const selectedRuntime = runtimeCandidates.find(runtime => runtime.key === runtimeKey);
  const browseCredential = useMemo(
    () => boundCredential || availableCredentials[0] || listedCredentials[0],
    [availableCredentials, boundCredential, listedCredentials],
  );
  const currentCredential = browseCredential;
  const [activeCredentialId, setActiveCredentialId] = useState(browseCredential?.id || "");

  useEffect(() => {
    if (loading) return;
    // A transient/failed catalog is not evidence that a saved account or model
    // was deleted. Only an explicit favorite toggle removes persisted entries.
    setFavorites(readFavoriteModels());
  }, [listedCredentials, loading]);

  useEffect(() => {
    if (open) return;
    if (browseCredential?.id) setActiveCredentialId(browseCredential.id);
  }, [browseCredential?.id, open]);

  useEffect(() => {
    if (!open) return;
    setQuery("");
    setFavoritesMode(false);
  }, [open]);

  const boundModel = useMemo(() => {
    if (!boundCredential) return null;
    return allCredentialModels(boundCredential).find((item) => item.id === selectedModel) || null;
  }, [boundCredential, selectedModel]);
  const currentModel = useMemo(() => {
    if (!currentCredential) return null;
    const models = allCredentialModels(currentCredential);
    return models.find((item) => item.id === selectedModel) || (!selectedModel ? models[0] : null) || null;
  }, [currentCredential, selectedModel]);
  const engineGroups = useMemo(
    () => CONVERSATION_WORKER_ENGINES
      .map((engine) => ({ engine, rows: listedCredentials.filter((item) => item.engine === engine) }))
      .filter((group) => group.rows.length > 0),
    [listedCredentials],
  );
  const unconfiguredEngines = useMemo(
    () => CONVERSATION_WORKER_ENGINES.filter((engine) => !listedCredentials.some((item) => item.engine === engine)),
    [listedCredentials],
  );
  const activeCredential = useMemo(
    () => listedCredentials.find((item) => item.id === activeCredentialId) || browseCredential,
    [activeCredentialId, browseCredential, listedCredentials],
  );
  const favoriteKeys = useMemo(
    () => new Set(favorites.map((item) => favoriteKey(item.credentialId, item.modelId))),
    [favorites],
  );
  const favoriteRows = useMemo<ModelRow[]>(() => favorites.flatMap((item) => {
    const credential = availableCredentials.find((row) => row.id === item.credentialId);
    if (!credential) return [];
    const model = allCredentialModels(credential).find((row) => row.id === item.modelId);
    if (!model) return [];
    return [{ key: favoriteKey(credential.id, model.id), credential, model }];
  }), [availableCredentials, favorites]);
  const visibleRows = useMemo<ModelRow[]>(() => {
    const needle = query.trim().toLowerCase();
    if (favoritesMode) return favoriteRows.filter((row) => matchesQuery(needle, row.model));
    if (!activeCredential) return [];
    return allCredentialModels(activeCredential)
      .filter((model) => matchesQuery(needle, model))
      .map((model) => ({ key: favoriteKey(activeCredential.id, model.id), credential: activeCredential, model }));
  }, [activeCredential, favoriteRows, favoritesMode, query]);
  const effortLevels = useMemo(() => modelEffortLevels(currentModel), [currentModel]);
  const effortOptions = useMemo(() => [""].concat(effortLevels), [effortLevels]);
  const activeEffort = selectedEffort === "default" ? "" : selectedEffort;
  const activeAccess = accessModes.includes(selectedAccessMode)
    ? selectedAccessMode
    : accessModes[0] || selectedAccessMode;
  const showEffort = variant === "conversation" && Boolean(boundCredential);
  const showAccess = variant === "conversation" && accessModes.length > 0;

  const choose = (patch: {
    credentialId?: string;
    model?: string;
    effort?: string;
    accessMode?: string;
  }) => {
    const selection = {
      credentialId: patch.credentialId ?? currentCredential?.id ?? "",
      model: patch.model ?? currentModel?.id ?? selectedModel,
      effort: patch.effort ?? selectedEffort,
      accessMode: patch.accessMode ?? selectedAccessMode,
    };
    const credential = listedCredentials.find((item) => item.id === selection.credentialId);
    const model = credential && allCredentialModels(credential).find((item) => item.id === selection.model);
    if (variant === "conversation" && credential && model) {
      rememberModelEffort(`${credential.id}:${credential.runtime_instance || ""}`, model, selection.effort);
    }
    onSelect(selection);
  };

  const toggleFavorite = (credentialId: string, modelId: string, modelLabel?: string) => {
    const key = favoriteKey(credentialId, modelId);
    const nextFavorited = !favoriteKeys.has(key);
    setFavorites((current) => {
      const next = current.some((item) => favoriteKey(item.credentialId, item.modelId) === key)
        ? current.filter((item) => favoriteKey(item.credentialId, item.modelId) !== key)
        : [...current, { credentialId, modelId }];
      persistFavoriteModels(next);
      return next;
    });
    if (modelLabel !== undefined) {
      setFavoriteStatus(favoriteToggleAnnouncement(modelLabel, nextFavorited));
    }
  };

  const selectModel = (credentialId: string, modelId: string) => {
    const credential = availableCredentials.find((item) => item.id === credentialId);
    const model = credential
      ? allCredentialModels(credential).find((item) => item.id === modelId)
      : undefined;
    if (credential) setActiveCredentialId(credential.id);
    choose({
      credentialId,
      model: modelId,
      effort: rememberedModelEffort(`${credentialId}:${credential?.runtime_instance || ""}`, model),
    });
    setOpen(false);
  };

  const keyboardOptions = useMemo<ListOption[]>(
    () => visibleRows.map((row) => ({ value: row.key, label: row.model.label, disabled: !usableCredential(row.credential) })),
    [visibleRows],
  );
  const keyboard = useListKeyboard(keyboardOptions, (key) => {
    const row = visibleRows.find((item) => item.key === key);
    if (row) selectModel(row.credential.id, row.model.id);
  });

  const toggleActiveFavorite = () => {
    if (keyboard.activeIndex < 0) return false;
    const row = visibleRows[keyboard.activeIndex];
    if (!row) return false;
    toggleFavorite(row.credential.id, row.model.id, row.model.label);
    return true;
  };

  useEffect(() => {
    if (!open) return;
    const preferred = visibleRows.findIndex(
      (row) => row.credential.id === selectedCredentialId && row.model.id === selectedModel,
    );
    // Keep activeIndex valid when search / endpoint / favorites mode changes (#196).
    keyboard.setActiveIndex(resolveActiveIndexAfterListChange(visibleRows.length, preferred));
    // Reset the highlight only when the list identity changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, favoritesMode, activeCredential?.id, query]);

  // Scroll the keyboard-active row into the list scrollport (OptionList pattern / #196).
  useEffect(() => {
    if (!open) return;
    scrollActiveOptionIntoView(modelListScrollRef.current, keyboard.activeIndex);
  }, [open, keyboard.activeIndex, visibleRows]);

  const triggerAria = boundCredential && boundModel
    ? `${engineLabel(boundCredential.engine)} · ${boundCredential.label} · ${boundModel.label}`
    : "选择接入点";
  const triggerText = boundModel?.label
    || (boundCredential && selectedModel)
    || (loading ? "加载中…" : error ? "加载失败" : "选择接入点");
  const triggerEngine = boundCredential?.engine || browseCredential?.engine || "omp";
  const fullAccess = showAccess && activeAccess === "full-access";

  const trigger = variant === "default-model" ? (
    <button
      type="button"
      aria-label={triggerAria}
      className={cn(
        "cx-press inline-flex h-9 w-full min-w-0 items-center gap-2 rounded-lg border border-cx-border bg-cx-elevated px-3 text-left text-[13px] text-cx-fg shadow-cx-xs",
        "hover:border-cx-border-strong data-[state=open]:border-cx-border-strong data-[state=open]:bg-cx-hover",
        "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
      )}
    >
      <EngineLogo engine={triggerEngine} size={15} />
      <span className="flex min-w-0 flex-1 items-center gap-1.5">
        {boundCredential ? (
          <span className="shrink-0 text-cx-fg-3">{engineLabel(boundCredential.engine)} · {boundCredential.label}</span>
        ) : null}
        {boundCredential ? <span className="text-cx-fg-4">/</span> : null}
        <span className={cn("min-w-0 truncate font-medium", !boundModel && "text-cx-fg-3")}>{triggerText}</span>
      </span>
      <Icon name="chevronsUpDown" size={13} className="shrink-0 text-cx-fg-4" />
    </button>
  ) : (
    <button
      type="button"
      aria-label={triggerAria}
      className={cn(
        "cx-press inline-flex h-8 min-w-0 max-w-[260px] items-center gap-1.5 rounded-full px-2.5 text-[12.5px] font-medium text-cx-fg-2",
        "hover:bg-cx-hover hover:text-cx-fg data-[state=open]:bg-cx-active data-[state=open]:text-cx-fg",
        "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
      )}
    >
      <EngineLogo engine={triggerEngine} size={14} className="shrink-0" />
      <span className="min-w-0 truncate">{triggerText}</span>
      {showEffort ? <span className="shrink-0 font-normal text-cx-fg-4">{effortLabelOf(activeEffort)}</span> : null}
      {fullAccess ? <Icon name="shieldAlert" size={13} className="shrink-0 text-cx-warning" /> : null}
      <Icon name="chevronDown" size={12} className="shrink-0 text-cx-fg-4" />
    </button>
  );

  const renderRow = (row: ModelRow, index: number) => {
    const selected = row.credential.id === selectedCredentialId && row.model.id === selectedModel;
    const favorited = favoriteKeys.has(row.key);
    const active = index === keyboard.activeIndex;
    const disabled = !usableCredential(row.credential);
    const badges = modelBadges(row.credential, row.model);
    const rowAriaLabel = row.model.label === row.model.id
      ? row.model.id
      : `${row.model.label}（${row.model.id}）`;
    return (
      <div
        key={row.key}
        id={`${listId}-opt-${index}`}
        role="option"
        aria-label={rowAriaLabel}
        aria-selected={selected}
        aria-disabled={disabled || undefined}
        data-index={index}
        data-active={active || undefined}
        data-selected={selected || undefined}
        onPointerMove={() => { if (!active) keyboard.setActiveIndex(index); }}
        onPointerDown={(event) => event.preventDefault()}
        onClick={() => { if (!disabled) selectModel(row.credential.id, row.model.id); }}
        className={cn(
          "group flex min-h-11 cursor-default select-none gap-2.5 rounded-lg px-2.5 py-1.5",
          narrowStacked ? "items-start" : "items-center",
          "data-[active=true]:bg-cx-hover",
          disabled && "opacity-45",
        )}
      >
        {favoritesMode ? <EngineLogo engine={row.credential.engine} size={16} className={cn("shrink-0", narrowStacked && "mt-1")} /> : null}
        <span className="flex min-w-0 flex-1 flex-col">
          <span className={cn("flex min-w-0 gap-1.5", narrowStacked ? "flex-wrap items-start" : "items-center")}>
            <span
              className={cn(
                "text-[13px] leading-5",
                selected ? "font-semibold text-cx-fg" : "font-medium text-cx-fg",
                narrowStacked ? "break-words [overflow-wrap:anywhere]" : "truncate",
              )}
            >
              {row.model.label}
            </span>
            {badges.map((badge) => (
              <Tooltip key={badge.label} content={badge.detail} disabled={!badge.detail}>
                <span><Badge tone={badge.tone} className="h-[18px] px-1.5 text-[10.5px]">{badge.label}</Badge></span>
              </Tooltip>
            ))}
          </span>
          <span
            className={cn(
              "font-cx-mono text-[11px] leading-4 text-cx-fg-4",
              narrowStacked ? "break-words [overflow-wrap:anywhere]" : "truncate",
            )}
          >
            {favoritesMode ? `${engineLabel(row.credential.engine)} · ${row.credential.label}` : row.model.id}
          </span>
          <span className="block truncate text-[10.5px] text-cx-fg-4" title={row.credential.runtime_instance || (lang === "en" ? "Model capability scope was not reported" : "未上报模型能力范围")}>{lang === "en" ? "Model capability Runtime: " : "模型能力来源 Runtime："}{row.credential.runtime_instance || (lang === "en" ? "Not reported" : "未上报")}</span>
        </span>
        <button
          type="button"
          tabIndex={-1}
          aria-pressed={favorited}
          aria-keyshortcuts="Alt+S"
          aria-label={favorited ? `取消收藏 ${row.model.label}（Alt+S）` : `收藏 ${row.model.label}（Alt+S）`}
          onPointerDown={(event) => event.stopPropagation()}
          onClick={(event) => {
            event.stopPropagation();
            toggleFavorite(row.credential.id, row.model.id, row.model.label);
          }}
          className={cn(
            "grid size-6 shrink-0 place-items-center rounded-md transition-opacity hover:bg-cx-active",
            narrowStacked && "mt-1",
            favorited
              ? "text-cx-warning opacity-100"
              : "text-cx-fg-4 opacity-0 group-hover:opacity-100 group-data-[active=true]:opacity-100",
          )}
        >
          <Icon name="star" size={13} filled={favorited} />
        </button>
        <Icon name="check" size={14} className={cn("shrink-0 text-cx-fg", narrowStacked && "mt-1.5", selected ? "opacity-100" : "opacity-0")} />
      </div>
    );
  };

  const railButton = "cx-press flex h-8 w-full min-w-0 items-center gap-2 rounded-lg px-2 text-left text-[12.5px] text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg data-[active=true]:bg-cx-active data-[active=true]:font-medium data-[active=true]:text-cx-fg";

  const body = loading ? (
    <PickerSkeleton />
  ) : error ? (
    <div className="p-3" data-testid="conversation-model-error">
      <Callout
        tone="danger"
        title="无法加载 Agent 列表"
        action={onRetry ? <Button size="xs" variant="secondary" icon="retry" onClick={onRetry}>重试</Button> : undefined}
      >
        <span className="block break-words">{error}</span>
        <span className="mt-1.5 block"><SettingsLink /></span>
      </Callout>
    </div>
  ) : listedCredentials.length === 0 ? (
    <EmptyState compact icon="bot" title="未发现可用 Agent" description="先在设置中登录或添加一个 Agent 接入点。" action={<SettingsLink />} />
  ) : (
    <div
      className={cn(
        "flex h-[min(380px,calc(100vh-180px))] min-h-[260px] min-w-0 overflow-x-hidden",
        narrowStacked && "flex-col",
      )}
      data-narrow-stacked={narrowStacked || undefined}
    >
      <nav
        aria-label="Agent 与接入点"
        className={cn(
          "cx-scroll flex shrink-0 flex-col gap-0.5 overflow-y-auto bg-cx-bg-subtle p-1.5",
          narrowStacked
            ? "max-h-[min(38%,148px)] w-full border-b border-cx-border-subtle"
            : "w-[172px] border-r border-cx-border-subtle",
        )}
      >
        <button
          type="button"
          data-active={favoritesMode || undefined}
          aria-pressed={favoritesMode}
          aria-label={`查看收藏模型，共 ${favoriteRows.length} 个`}
          onClick={() => { setFavoritesMode((value) => !value); setQuery(""); }}
          className={railButton}
        >
          <Icon name="star" size={14} filled={favoriteRows.length > 0} className={favoriteRows.length ? "text-cx-warning" : "text-cx-fg-4"} />
          <span className="flex-1 truncate">收藏</span>
          {favoriteRows.length ? <span className="cx-tabular text-[11px] text-cx-fg-4">{favoriteRows.length}</span> : null}
        </button>
        {engineGroups.map(({ engine, rows }) => (
          <div key={engine} className="flex flex-col gap-0.5 pt-1.5">
            <div className="flex items-center gap-1.5 px-2 pb-0.5 text-[11px] font-medium text-cx-fg-4">
              <EngineLogo engine={engine} size={12} />
              <span className="truncate">{engineLabel(engine)}</span>
            </div>
            {rows.map((credential) => {
              const statusText = credentialStatusLabel(credential);
              const active = !favoritesMode && credential.id === activeCredential?.id;
              return (
                <Tooltip key={credential.id} content={[endpointKind(credential), statusText].filter(Boolean).join(" · ")} placement="right">
                  <button
                    type="button"
                    data-active={active || undefined}
                    aria-current={active || undefined}
                    aria-label={credential.label}
                    onClick={() => { setFavoritesMode(false); setActiveCredentialId(credential.id); setQuery(""); }}
                    className={railButton}
                  >
                    <StatusDot tone={credentialTone(credential)} />
                    <span className="min-w-0 flex-1 truncate">{credential.label}</span>
                    <span className="cx-tabular shrink-0 text-[11px] text-cx-fg-4">{allCredentialModels(credential).length}</span>
                  </button>
                </Tooltip>
              );
            })}
          </div>
        ))}
        {unconfiguredEngines.length ? (
          <a
            href="/settings/agents"
            className="mt-auto flex items-center gap-1.5 rounded-lg px-2 pb-1 pt-3 text-[11.5px] text-cx-fg-4 hover:text-cx-fg-2"
          >
            <Icon name="plus" size={12} />
            添加其他 Agent
          </a>
        ) : null}
      </nav>

      <section className="flex min-w-0 flex-1 flex-col">
        <div className="border-b border-cx-border-subtle p-2">
          <SearchInput
            value={query}
            onValueChange={(next) => { setQuery(next); keyboard.setActiveIndex(0); }}
            onKeyDown={(event) => {
              // IME candidate confirm must not favorite / navigate (#197).
              if (isImeComposingKeyEvent(event)) return;
              // Alt+S favorites the aria-activedescendant row without leaving search (#197).
              if (isFavoriteToggleKey(event)) {
                if (!visibleRows.length || keyboard.activeIndex < 0) return;
                event.preventDefault();
                toggleActiveFavorite();
                return;
              }
              keyboard.onKeyDown(event);
            }}
            placeholder={favoritesMode ? "搜索收藏…" : "搜索模型…"}
            aria-label="搜索模型，Alt+S 收藏当前项"
            autoComplete="off"
            spellCheck={false}
            role="combobox"
            aria-expanded
            aria-controls={listId}
            aria-activedescendant={keyboard.activeIndex >= 0 && visibleRows.length ? `${listId}-opt-${keyboard.activeIndex}` : undefined}
            data-autofocus
            className="[&_input]:border-transparent [&_input]:bg-cx-hover [&_input]:shadow-none [&_input:focus]:bg-cx-elevated"
          />
        </div>
        {!favoritesMode && activeCredential ? (
          <div className="flex items-center gap-2 px-3.5 pb-1 pt-2.5 text-[11px] text-cx-fg-4">
            <span className="truncate font-medium text-cx-fg-3">{activeCredential.label}</span>
            <span className="shrink-0">{endpointKind(activeCredential)}</span>
            {credentialStatusLabel(activeCredential) ? (
              <span className="ml-auto shrink-0 text-cx-danger">{credentialStatusLabel(activeCredential)}</span>
            ) : null}
          </div>
        ) : null}
        <div className="sr-only" role="status" aria-live="polite" aria-atomic="true">
          {favoriteStatus}
        </div>
        <div
          ref={modelListScrollRef}
          id={listId}
          role="listbox"
          aria-label={favoritesMode ? "收藏模型" : "可用模型"}
          aria-keyshortcuts="Alt+S"
          className="cx-scroll min-h-0 flex-1 overflow-y-auto overscroll-contain p-1.5"
        >
          {visibleRows.length ? visibleRows.map(renderRow) : (
            <EmptyState
              compact
              icon={favoritesMode ? "star" : "search"}
              title={favoritesMode
                ? (query.trim() ? "没有匹配的收藏" : "还没有收藏")
                : query.trim()
                  ? "没有匹配的模型"
                  : credentialStatusLabel(activeCredential) || "该接入点暂无可用模型"}
              description={favoritesMode && !query.trim() ? "在模型行上点星标即可收藏常用组合。" : undefined}
              action={favoritesMode ? undefined : <SettingsLink />}
            />
          )}
        </div>
      </section>
    </div>
  );

  const footer = showRuntime || (!loading && !error && (showEffort || showAccess)) ? (
    <div className="flex flex-col gap-2 border-t border-cx-border-subtle bg-cx-bg-subtle px-3 py-2.5">
      {showRuntime ? (
        <div className="flex flex-col gap-1">
          <div className="flex items-center gap-3">
            <span className="w-14 shrink-0 text-[11.5px] font-medium text-cx-fg-3">Runtime</span>
            <Select
              size="sm"
              ariaLabel={lang === "en" ? "Conversation Runtime" : "会话 Runtime"}
              value={selectedRuntime?.key || null}
              disabled={runtimeDisabled}
              placeholder={!boundCredential || boundCredential.present === false
                ? (lang === "en" ? "Select a credential first" : "请先选择凭据")
                : !runtimeCandidates.length ? (lang === "en" ? "No Runtime for this Agent" : "此 Agent 没有 Runtime")
                  : runtimeKey && !selectedRuntime ? (lang === "en" ? "Current Runtime unavailable; select again" : "当前 Runtime 不可用，请重新选择")
                    : (lang === "en" ? "Select Runtime" : "选择 Runtime")}
              onChange={key => {
                if (runtimeDisabled || key === runtimeKey) return;
                const runtime = runtimeCandidates.find(row => row.key === key && row.enabled !== false);
                if (runtime) onRuntimeChange?.(runtime.key);
              }}
              placement="top-start"
              className="min-w-0 flex-1"
              popoverClassName="w-[min(420px,calc(100vw-24px))]"
              options={runtimeCandidates.map(runtime => ({
                value: runtime.key,
                label: `${runtime.adapter_id} · ${runtime.instance_id}`,
                description: [
                  runtime.enabled === false ? (lang === "en" ? "Disabled" : "已停用") : "",
                  runtime.health?.healthy === true ? (lang === "en" ? "Reported healthy" : "上报状态正常")
                    : runtime.health?.healthy === false ? (lang === "en" ? "Reported unhealthy" : "上报状态异常")
                      : (lang === "en" ? "Not probed" : "未探测"),
                  runtime.health?.detail || "",
                ].filter(Boolean).join(" · "),
                disabled: runtime.enabled === false,
              }))}
            />
          </div>
          <p className="pl-[68px] text-[10.5px] text-cx-fg-4">{boundCredential
            ? `${lang === "en" ? "Current credential" : "当前凭据"}: ${boundCredential.label} · ${engineLabel(boundCredential.engine)}`
            : (lang === "en" ? "No credential selected" : "尚未选择凭据")}</p>
          {runtimeCandidates.length && !runtimeCandidates.some(runtime => runtime.enabled !== false)
            ? <p className="pl-[68px] text-[10.5px] text-cx-fg-4">{lang === "en" ? "All Runtime instances for this Agent are disabled" : "此 Agent 的 Runtime 均已停用"}</p> : null}
        </div>
      ) : null}
      {!loading && !error && showEffort ? (
        <div className="flex items-center gap-3">
          <span className="flex w-14 shrink-0 items-center gap-1 text-[11.5px] font-medium text-cx-fg-3">
            <Icon name="brain" size={12} />
            思考
          </span>
          <SegmentedControl
            size="xs"
            ariaLabel="思考程度"
            value={effortOptions.includes(activeEffort) ? activeEffort : ""}
            onChange={(effort) => { if (effort !== activeEffort) choose({ effort }); }}
            options={effortOptions.map((level) => ({ value: level, label: currentModel?.reasoning?.kind === "variant" && level ? level : effortLabelOf(level) }))}
            className="min-w-0 flex-1 justify-between overflow-x-auto"
          />
        </div>
      ) : null}
      {!loading && !error && showAccess ? (
        <div className="flex items-center gap-3">
          <span className="flex w-14 shrink-0 items-center gap-1 text-[11.5px] font-medium text-cx-fg-3">
            <Icon name="shield" size={12} />
            权限
          </span>
          <Select
            size="sm"
            ariaLabel="应如何批准 Agent 操作"
            value={activeAccess}
            onChange={(next) => {
              if (!next || !accessModes.includes(next) || next === activeAccess) return;
              choose({ accessMode: next });
            }}
            placement="top-start"
            className={cn("flex-1", fullAccess && "text-cx-warning")}
            popoverClassName="w-[300px]"
            options={accessModes.map((mode) => {
              const meta = accessModeMeta(mode);
              return {
                value: mode,
                label: meta.label,
                description: meta.detail,
                leading: <Icon name={meta.icon} size={15} className={mode === "full-access" ? "text-cx-warning" : "text-cx-fg-3"} />,
                icon: meta.icon,
              };
            })}
          />
        </div>
      ) : null}
    </div>
  ) : null;

  return (
    <div className={cn("inline-flex min-w-0 items-center", variant === "default-model" && "w-full", className)}>
      <Popover
        open={open}
        onOpenChange={setOpen}
        trigger={trigger}
        placement={variant === "conversation" ? "top-start" : "bottom-start"}
        offset={8}
        ariaLabel="选择 Agent 和模型"
        className="w-[min(480px,calc(100vw-16px))] p-0"
      >
        {body}
        {footer}
      </Popover>
    </div>
  );
}
