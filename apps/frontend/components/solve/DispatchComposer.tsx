// ai-design-check-ignore-file use-shared-components -- toolbar pills share the chat composer's pill shape, which Button does not offer
"use client";

import { useCallback, useEffect, useId, useRef, useState, type ReactNode } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { PromptBar, type PromptBarAttachment } from "@/components/chat/composer/MutekiPromptBar";
import { Badge, Button, Collapse, Input, SegmentedControl, Select, Shortcut, Switch, Tooltip, toast, type ListOption, type Tone } from "@muteki/ui";
import { Icon, type IconName } from "@muteki/ui";
import { NumberField } from "@/components/NumberField";
import type { DispatchOpts } from "@/components/Conversation";
import { ComposerEffortSlider } from "@/components/conversation/ComposerEffortSlider";
import { effortLabelOf } from "@/components/conversation/ConversationComposerModes";
import { engineEffortLevels } from "@/components/worker-roster/model";
import { RosterAvatar } from "@/components/worker-roster/RosterAvatar";
import { cn } from "@/lib/cn";
import { dataTransferHasFiles, filesFromDataTransfer } from "@/lib/composerAttachments";
import {
  DISPATCH_TEMPLATE_STORAGE, defaultRosterSeats, findDispatchTemplate, rosterSlotEngines, rosterSummary,
} from "@/lib/dispatchTemplates";
import { useT } from "@/lib/i18n";
import { engineDisplayName, readyCatalog, useProviderDescriptors } from "@/lib/providerDescriptors";
import { listRunnerNodes } from "@/lib/runnerNodes";
import {
  checkAuth,
  fetchLocalPiCatalog,
  getExecutionGuardrailSettings,
  getWorkerSettings,
  type ExecutionGuardrailSettings,
  type LocalPiCatalogResponse,
  type SavedFile,
  type WorkerSettings,
} from "@/lib/useRun";

type FlagFormat = "brace" | "token" | "custom";
type LimitKey = "wallClockBudget" | "maxTotalWorkers" | "costBudgetUsd";

export interface DispatchPrefill { text: string }

const STORAGE = {
  web: "muteki.webSearch",
  collect: "muteki.collect",
  flagFormat: "muteki.flagFormat",
  flagWrapper: "muteki.flagWrapper",
  operator: "muteki.allowOperatorInput",
  executionKind: "muteki.executionKind",
} as const;

function remember(key: string, value: string) {
  try { window.localStorage.setItem(key, value); } catch {  }
}

const PILL = "cx-press inline-flex h-8 min-w-0 shrink-0 items-center gap-1.5 rounded-full px-2.5 text-cx-sm font-medium outline-none transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)] disabled:cursor-not-allowed disabled:opacity-60";

const PICKER = "cx-press inline-flex h-8 items-center gap-2 rounded-xl px-2 text-cx-sm font-medium text-cx-fg-2 outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)] data-[state=open]:bg-cx-active data-[state=open]:text-cx-fg";

function Pill({ icon, label, title, ariaLabel = title, on, disabled, expanded, controls, onPress, trailing }: {
  icon: IconName; label: ReactNode; title: string; ariaLabel?: string; on?: boolean; disabled?: boolean;
  expanded?: boolean; controls?: string; onPress: () => void; trailing?: ReactNode;
}) {
  return (
    <Tooltip content={title} placement="top">
      <button
        type="button"
        aria-label={ariaLabel}
        aria-pressed={expanded === undefined ? Boolean(on) : undefined}
        aria-expanded={expanded}
        aria-controls={controls}
        disabled={disabled}
        onClick={onPress}
        className={cn("dc-pill", PILL, on || expanded ? "bg-cx-accent-soft text-cx-accent" : "text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg")}
      >
        <Icon name={icon} size={14} />
        <span className="truncate">{label}</span>
        {trailing}
      </button>
    </Tooltip>
  );
}

function ManageLink({ label, onPress }: { label: string; onPress: () => void }) {
  return (
    <button
      type="button"
      onClick={onPress}
      className="flex h-8 w-full items-center gap-2 rounded-lg px-2 text-left text-cx-sm text-cx-fg-2 outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:bg-cx-hover"
    >
      <Icon name="gear" size={14} className="shrink-0 text-cx-fg-3" />
      <span className="truncate">{label}</span>
    </button>
  );
}

function LimitField({ label, hint, children }: { label: string; hint: string; children: ReactNode }) {
  return (
    <div className="flex min-w-0 flex-col gap-2">
      <span className="text-cx-sm text-cx-fg">{label}</span>
      {children}
      <span className="text-cx-xs text-[color:var(--dc-soft)]">{hint}</span>
    </div>
  );
}

function OptionRow({ title, hint, children }: { title: string; hint: string; children: ReactNode }) {
  return (
    <div className="flex min-w-0 flex-wrap items-center justify-between gap-x-6 gap-y-2.5 py-3.5 first:pt-0 last:pb-0">
      <div className="min-w-[min(100%,11rem)] flex-1">
        <div className="text-cx-sm font-medium text-cx-fg">{title}</div>
        <div className="mt-0.5 text-cx-xs leading-4 text-[color:var(--dc-soft)]">{hint}</div>
      </div>
      <div className="flex min-w-0 shrink-0 items-center gap-2">{children}</div>
    </div>
  );
}

const ADVANCED_ID = "dispatch-advanced-controls";
const DEFAULT_TEMPLATE = "__default__";

export function DispatchComposer({
  mode, text, onTextChange, attachments, onAddFiles, onRemoveFile, onDispatch, prefill, onPrefillConsumed, below, dispatchPending, onProductionGuardChange,
}: {
  mode: "ctf" | "pentest";
  text: string;
  onTextChange: (value: string) => void;
  attachments: SavedFile[];
  onAddFiles: (files: FileList | File[]) => void | Promise<number | void>;
  onRemoveFile: (path: string) => void;
  onDispatch: (prompt: string, opts: DispatchOpts) => void | boolean | Promise<void | boolean>;
  prefill: DispatchPrefill | null;
  onPrefillConsumed: () => void;
  dispatchPending: boolean;
  below?: ReactNode;
  onProductionGuardChange?: (enabled: boolean) => void;
}) {
  const t = useT();
  const router = useRouter();
  const catalog = readyCatalog(useProviderDescriptors());
  const rootRef = useRef<HTMLDivElement>(null);
  const fileRef = useRef<HTMLInputElement>(null);
  const blockerId = useId();
  const guardHeadingId = useId();
  const pentest = mode === "pentest";

  const [webSearch, setWebSearch] = useState(true);
  const [executionKind, setExecutionKind] = useState<"swarm" | "local_pi">("swarm");
  const [workerConfig, setWorkerConfig] = useState<WorkerSettings | null>();
  const [templateId, setTemplateId] = useState("");
  const selectedTemplate = findDispatchTemplate(workerConfig ?? null, templateId);
  const [localPiCatalog, setLocalPiCatalog] = useState<LocalPiCatalogResponse | null>(null);
  const localPiEnabled = localPiCatalog?.settings?.enabled !== false;
  const resolvedExecutionKind = pentest || !localPiEnabled ? "swarm" : executionKind;
  const [localPiAccountId, setLocalPiAccountId] = useState("system_login");
  const [localPiModel, setLocalPiModel] = useState("");
  const [localPiEffort, setLocalPiEffort] = useState("medium");
  const [localPiSkills, setLocalPiSkills] = useState<string[] | null>(null);
  const [collect, setCollect] = useState(false);
  const [collectCount, setCollectCount] = useState("");
  const [reportGoalMode, setReportGoalMode] = useState<"automatic" | "count">("automatic");
  const [expectedFindings, setExpectedFindings] = useState("10");
  const [flagFormat, setFlagFormat] = useState<FlagFormat>("brace");
  const [flagWrapper, setFlagWrapper] = useState("");
  const [containerMode, setContainerMode] = useState(false);
  const [containerLocked, setContainerLocked] = useState(false);
  const [remoteNode, setRemoteNode] = useState<{ id: string; name: string } | null>(null);
  const effectiveContainer = containerMode || containerLocked || Boolean(remoteNode);
  const [allowOperatorInput, setAllowOperatorInput] = useState(false);
  const [productionGuardEnabled, setProductionGuardEnabled] = useState(false);
  const [guardrailSnapshot, setGuardrailSnapshot] = useState<{
    loading: boolean;
    settings: ExecutionGuardrailSettings | null;
    error: string;
  }>({ loading: false, settings: null, error: "" });
  const [limits, setLimits] = useState<Record<LimitKey, string>>({ wallClockBudget: "0", maxTotalWorkers: "0", costBudgetUsd: "0" });
  const [touched, setTouched] = useState<Partial<Record<LimitKey, true>>>({});
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [fileDrag, setFileDrag] = useState(false);
  const [pending, setPending] = useState<{ id: string; name: string; size: number; error?: string }[]>([]);
  const [composerUndoEpoch, setComposerUndoEpoch] = useState(0);
  const attach = async (files: FileList | File[]) => {
    if (dispatchPending) return;
    const list = Array.from(files);
    if (!list.length) return;
    const rows = list.map((file) => ({ id: `pending:${crypto.randomUUID()}`, name: file.name, size: file.size }));
    setPending((old) => [...old, ...rows]);
    let saved: number | void = 0;
    try { saved = await onAddFiles(list); } catch { saved = 0; }
    const ids = new Set(rows.map((row) => row.id));
    if (saved === 0) setPending((old) => old.map((row) => (ids.has(row.id) ? { ...row, error: t("composer.uploadFailed") } : row)));
    else setPending((old) => old.filter((row) => !ids.has(row.id)));
  };
  const attachRef = useRef(attach);
  attachRef.current = attach;

  useEffect(() => {
    let depth = 0;
    let staleTimer = 0;
    const settle = () => { depth = 0; window.clearTimeout(staleTimer); setFileDrag(false); };
    const hasFiles = (event: DragEvent) => dataTransferHasFiles(event.dataTransfer);
    const onEnter = (event: DragEvent) => { if (hasFiles(event)) { depth += 1; setFileDrag(true); } };
    const onLeave = (event: DragEvent) => {
      if (!hasFiles(event)) return;
      depth = Math.max(0, depth - 1);
      if (depth === 0) setFileDrag(false);
    };
    const onOver = (event: DragEvent) => {
      if (!hasFiles(event)) return;
      event.preventDefault();
      if (event.dataTransfer) event.dataTransfer.dropEffect = "copy";
      setFileDrag(true);
      window.clearTimeout(staleTimer);
      staleTimer = window.setTimeout(settle, 1200);
    };
    const onDropWindow = (event: DragEvent) => {
      const wasFiles = hasFiles(event);
      settle();
      if (!wasFiles) return;
      if (event.defaultPrevented) return;
      event.preventDefault();
      const files = filesFromDataTransfer(event.dataTransfer);
      if (files.length) void attachRef.current(files);
    };
    window.addEventListener("dragenter", onEnter);
    window.addEventListener("dragleave", onLeave);
    window.addEventListener("dragover", onOver);
    window.addEventListener("drop", onDropWindow);
    window.addEventListener("dragend", settle);
    return () => {
      window.clearTimeout(staleTimer);
      window.removeEventListener("dragenter", onEnter);
      window.removeEventListener("dragleave", onLeave);
      window.removeEventListener("dragover", onOver);
      window.removeEventListener("drop", onDropWindow);
      window.removeEventListener("dragend", settle);
    };
  }, []);

  const workerLoadRef = useRef({ loading: false, live: true });
  const loadWorkerSettings = useCallback(async (initial = false) => {
    const load = workerLoadRef.current;
    if (load.loading) return;
    load.loading = true;
    const config = await getWorkerSettings();
    load.loading = false;
    if (!load.live) return;
    setWorkerConfig(config);
    if (!initial || !config) return;
    setContainerMode(config.worker_backend === "container");
    const node = config.worker_backend === "container" ? config.worker_node_id : undefined;
    if (node && node !== "local") {
      setRemoteNode({ id: node, name: node });
      listRunnerNodes().then((nodes) => {
        const match = nodes.find((row) => row.id === node);
        if (load.live && match) setRemoteNode({ id: node, name: match.name });
      }).catch(() => {  });
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    const load = workerLoadRef.current;
    load.live = true;
    try {
      if (window.localStorage.getItem(STORAGE.web) === "0") setWebSearch(false);
      if (window.localStorage.getItem(STORAGE.collect) === "1") setCollect(true);
      const savedFormat = window.localStorage.getItem(STORAGE.flagFormat);
      if (savedFormat === "token" || savedFormat === "custom") setFlagFormat(savedFormat);
      const savedWrapper = window.localStorage.getItem(STORAGE.flagWrapper);
      if (savedWrapper) setFlagWrapper(savedWrapper);
      if (window.localStorage.getItem(STORAGE.operator) === "1") setAllowOperatorInput(true);
      const savedKind = window.localStorage.getItem(STORAGE.executionKind);
      if (savedKind === "local_pi" || savedKind === "swarm") setExecutionKind(savedKind);
      setTemplateId(window.localStorage.getItem(DISPATCH_TEMPLATE_STORAGE) || "");
    } catch {  }
    void loadWorkerSettings(true);
    const refreshWorkers = () => void loadWorkerSettings();
    const interval = window.setInterval(refreshWorkers, 30_000);
    window.addEventListener("focus", refreshWorkers);
    checkAuth().then((auth) => {
      if (!cancelled && auth?.inContainer) { setContainerMode(true); setContainerLocked(true); }
    }).catch(() => {  });
    return () => {
      cancelled = true;
      load.live = false;
      window.clearInterval(interval);
      window.removeEventListener("focus", refreshWorkers);
    };
  }, [loadWorkerSettings]);

  useEffect(() => {
    if (pentest || localPiCatalog) return;
    let cancelled = false;
    fetchLocalPiCatalog().then((cat) => {
      if (cancelled || !cat) return;
      setLocalPiCatalog(cat);
      const models = cat.models ?? [];
      const savedModel = cat.settings?.default_model || "";
      const savedSource = cat.settings?.default_account_id || "";
      const initial = models.find((m) => m.id === savedModel && (m.source_ref || "system_login") === (savedSource || m.source_ref || "system_login"))
        ?? models.find((m) => m.id === savedModel)
        ?? models[0];
      if (initial) {
        setLocalPiModel(initial.id);
        setLocalPiAccountId(initial.source_ref || "system_login");
      } else if (cat.settings?.default_account_id) {
        setLocalPiAccountId(cat.settings.default_account_id);
      }
      setLocalPiSkills(Array.isArray(cat.settings?.default_skills)
        ? cat.settings.default_skills
        : cat.skills.filter((skill) => skill.default_enabled !== false).map((skill) => skill.id));
    }).catch(() => {});
    return () => { cancelled = true; };
  }, [pentest, localPiCatalog]);

  const protectedMode = pentest && productionGuardEnabled;
  const defaultSeats = workerConfig ? defaultRosterSeats(workerConfig) : [];
  const shownSeats = selectedTemplate ? selectedTemplate.seats : defaultSeats;
  const hasDefaultRoster = Boolean(workerConfig && (
    (workerConfig.engines?.some(ref => ref.trim()) && (
      defaultSeats.length > 0 || (!workerConfig.seats?.length && !workerConfig.worker_profiles?.length)
    ))
    || Object.values(workerConfig.overrides ?? {}).some(override => override.engines?.some(ref => ref.trim()))
  ));
  const hasRoster = selectedTemplate ? selectedTemplate.seats.some(seat => seat.enabled !== false) : hasDefaultRoster;
  const blockers: { message: string; section?: "roster" | "models" }[] = [];
  if (resolvedExecutionKind === "swarm") {
    if (workerConfig === undefined) blockers.push({ message: t("composer.dispatchSettingsLoading") });
    else if (workerConfig === null) blockers.push({ message: t("composer.dispatchSettingsUnavailable") });
    else {
      if (!hasRoster) blockers.push({ message: t("composer.workerRosterEmpty"), section: "roster" });
      if (workerConfig.planner_configured !== true) blockers.push({ message: t("composer.plannerRequired"), section: "models" });
    }
  }
  const dispatchBlocked = blockers.length > 0;
  const activePiSeats = shownSeats.filter((seat) => seat.enabled !== false && seat.engine === "pi");
  const disabledNonPiSeats = shownSeats.filter((seat) => seat.enabled !== false && seat.engine !== "pi");
  const protectedRosterInvalid = protectedMode && (activePiSeats.length === 0 || disabledNonPiSeats.length > 0);
  useEffect(() => {
    onProductionGuardChange?.(protectedMode);
  }, [onProductionGuardChange, protectedMode]);

  useEffect(() => {
    if (!protectedMode) return;
    const controller = new AbortController();
    setGuardrailSnapshot((prev) => ({ ...prev, loading: true, error: "" }));
    getExecutionGuardrailSettings(controller.signal)
      .then((settings) => {
        if (!controller.signal.aborted) {
          setGuardrailSnapshot({ loading: false, settings, error: "" });
        }
      })
      .catch((cause) => {
        if (!controller.signal.aborted) {
          setGuardrailSnapshot({
            loading: false,
            settings: null,
            error: cause instanceof Error ? cause.message : String(cause),
          });
        }
      });
    return () => controller.abort();
  }, [protectedMode]);

  const editor = useCallback(() => rootRef.current?.querySelector<HTMLTextAreaElement>("[data-c34-composer]") ?? null, []);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const el = event.target as HTMLElement | null;
      const typing = !!el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.tagName === "SELECT" || el.isContentEditable);
      if (el?.closest?.(".react-flow, .collab-shell")) return;
      if (event.key !== "/" || event.metaKey || event.ctrlKey || event.altKey || typing) return;
      const field = editor();
      if (!field) return;
      event.preventDefault();
      field.focus();
      field.select();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [editor]);

  useEffect(() => {
    if (!prefill) return;
    onTextChange(prefill.text);
    window.requestAnimationFrame(() => {
      const field = editor();
      field?.focus();
      field?.setSelectionRange(prefill.text.length, prefill.text.length);
    });
    onPrefillConsumed();
  }, [editor, onPrefillConsumed, onTextChange, prefill]);

  const uploading = pending.some((row) => !row.error);
  const dispatch = async () => {
    const prompt = text.trim();
    if (!prompt || dispatchPending || uploading || dispatchBlocked || protectedRosterInvalid) return;
    const int = (raw: string) => { const n = parseInt(raw, 10); return Number.isNaN(n) ? undefined : n; };
    const float = (raw: string) => { const n = parseFloat(raw); return Number.isNaN(n) ? undefined : n; };
    const caps = {
      ...(touched.wallClockBudget ? { wallClockBudget: int(limits.wallClockBudget) } : {}),
      ...(touched.maxTotalWorkers ? { maxTotalWorkers: int(limits.maxTotalWorkers) } : {}),
      ...(touched.costBudgetUsd ? { costBudgetUsd: float(limits.costBudgetUsd) } : {}),
    };
    if (pentest && reportGoalMode === "count" && (!Number.isSafeInteger(Number(expectedFindings)) || Number(expectedFindings) < 1)) {
      toast({ title: "请填写大于 0 的报告目标数量", tone: "warning" });
      return;
    }
    const dispatched = await onDispatch(prompt, {
      webSearch: pentest ? true : webSearch,
      mode,
      executionKind: resolvedExecutionKind,
      localPiModel: resolvedExecutionKind === "local_pi" ? (localPiModel.trim() || undefined) : undefined,
      localPiAccountId: resolvedExecutionKind === "local_pi" ? (localPiAccountId.trim() || undefined) : undefined,
      localPiEffort: resolvedExecutionKind === "local_pi" ? (localPiEffort || undefined) : undefined,
      localPiSkills: resolvedExecutionKind === "local_pi" && localPiSkills ? localPiSkills : undefined,
      dispatchTemplateId: resolvedExecutionKind === "swarm" ? selectedTemplate?.id : undefined,
      collect, containerMode: effectiveContainer,
      allowOperatorInput: pentest ? true : allowOperatorInput,
      flagFormat,
      flagWrapper: flagFormat === "custom" ? flagWrapper.trim() : undefined,
      collectCount: collect ? (parseInt(collectCount, 10) || 0) : undefined,
      reportGoalMode: pentest ? reportGoalMode : undefined,
      expectedFindings: pentest && reportGoalMode === "count" ? Number(expectedFindings) : undefined,
      productionGuard: pentest
        ? { enabled: productionGuardEnabled, mode: productionGuardEnabled ? "protected" : "off" }
        : undefined,
      ...caps,
    });
    if (dispatched !== false) {
      setComposerUndoEpoch((n) => n + 1);
      onTextChange("");
    } else {
      void loadWorkerSettings();
    }
  };

  const setLimit = (key: LimitKey) => (value: string) => {
    setTouched((old) => ({ ...old, [key]: true }));
    setLimits((old) => ({ ...old, [key]: value }));
  };

  const promptAttachments: PromptBarAttachment[] = [
    ...attachments.map((file): PromptBarAttachment => ({
      id: file.path, name: file.name, size: file.size, artifactId: file.path, uploadStatus: "done",
    })),
    ...pending.map((row): PromptBarAttachment => ({
      id: row.id, name: row.name, size: row.size, artifactId: row.id,
      uploadStatus: row.error ? "error" : "uploading", uploadError: row.error,
    })),
  ];

  const flagOptions: ListOption<FlagFormat>[] = [
    { value: "brace", label: t("composer.flagFormatBrace") },
    { value: "custom", label: t("composer.flagFormatCustom") },
    { value: "token", label: t("composer.flagFormatToken") },
  ];

  const nameOf = (engine: string) => engineDisplayName(catalog, engine);
  const workersPath = pentest ? "/pentest/workers" : "/ctf/workers";
  const openSettings = (section: string) => {
    const returnTo = `${window.location.pathname}${window.location.search}`;
    router.push(`${workersPath}?section=${section}&return=${encodeURIComponent(returnTo)}`);
  };

  const modeSwitch = pentest || !localPiEnabled ? null : (
    <SegmentedControl<"local_pi" | "swarm">
      size="md"
      ariaLabel={t("composer.executionLabel")}
      className="dc-mode shrink-0"
      value={resolvedExecutionKind}
      options={[
        { value: "local_pi", label: t("composer.executionPi"), ariaLabel: t("composer.executionPiTitle") },
        { value: "swarm", label: t("composer.executionSwarm"), ariaLabel: t("composer.executionSwarmTitle") },
      ]}
      onChange={(next) => { setExecutionKind(next); remember(STORAGE.executionKind, next); }}
    />
  );

  const piModels = localPiCatalog?.models ?? [];
  const currentPiModel = piModels.find((m) => m.id === localPiModel && (m.source_ref || "system_login") === localPiAccountId)
    ?? piModels.find((m) => m.id === localPiModel);
  const piModelOptions: ListOption[] = piModels.map((m) => ({
    value: m.key ?? m.id,
    label: m.label || m.id,
    textValue: `${m.label || m.id} ${m.source_label || m.source_ref || m.source}`,
    description: m.source_label || m.source_ref || m.source,
  }));
  const piEfforts = engineEffortLevels(catalog, "pi");
  const piModelLabel = currentPiModel ? (currentPiModel.label || currentPiModel.id)
    : localPiModel || t(localPiCatalog ? "composer.modelNone" : "composer.modelLoading");
  const piEffortText = localPiEffort ? effortLabelOf(localPiEffort) : "";
  const soloPicker = (
    <Select
      key="solo"
      value={currentPiModel ? (currentPiModel.key ?? currentPiModel.id) : null}
      onChange={(key) => {
        const found = piModels.find((m) => (m.key ?? m.id) === key);
        setLocalPiModel(found ? found.id : key);
        if (found) setLocalPiAccountId(found.source_ref || "system_login");
      }}
      options={piModelOptions}
      placement="top-start"
      ariaLabel={t("composer.modelPicker")}
      searchable={piModelOptions.length > 8}
      popoverClassName="w-[320px]"
      emptyText={t(localPiCatalog ? "composer.modelNone" : "composer.modelLoading")}
      footer={(close) => (
        <>
          {piEfforts.length ? (
            <div
              className="-mx-1 -mt-1 mb-1 border-b border-cx-border-subtle"
              onKeyDown={(event) => { if (event.key.startsWith("Arrow") || event.key === "Home" || event.key === "End" || event.key === "Enter") event.stopPropagation(); }}
            >
              <ComposerEffortSlider
                embedded
                levels={piEfforts}
                value={localPiEffort}
                modelDefault=""
                modelLabel={currentPiModel?.label || currentPiModel?.id}
                labelOf={effortLabelOf}
                onChange={setLocalPiEffort}
                speedOptions={[]}
                speed=""
                onSpeedChange={() => {}}
              />
            </div>
          ) : null}
          <ManageLink label={t("composer.modelManage")} onPress={() => { close(); openSettings("local_pi"); }} />
        </>
      )}
      trigger={(
        <button type="button"
          aria-label={`${t("composer.modelPicker")}: ${piModelLabel}${piEffortText ? ` · ${t("composer.effortPicker")} ${piEffortText}` : ""}`}
          title={currentPiModel?.source_label || undefined}
          className={cn(PICKER, "min-w-0 flex-1")}>
          <RosterAvatar engines={["pi"]} size={20} />
          <span className="min-w-0 flex-1 truncate text-left">{piModelLabel}</span>
          {piEffortText ? <span className="min-w-0 max-w-[40%] shrink-[2] truncate text-cx-xs text-cx-fg-4">{piEffortText}</span> : null}
          <Icon name="chevronDown" size={12} className="shrink-0 text-cx-fg-4" />
        </button>
      )}
    />
  );

  const filterSeatsForMode = <T extends { engine: string; enabled?: boolean }>(seats: T[]) => (
    protectedMode ? seats.filter((seat) => seat.engine === "pi") : seats
  );
  const templateOptions: ListOption[] = [
    {
      value: DEFAULT_TEMPLATE,
      label: t("composer.templateDefault"),
      leading: <RosterAvatar engines={rosterSlotEngines(filterSeatsForMode(defaultSeats))} size={28} />,
      description: protectedMode
        ? (rosterSummary(filterSeatsForMode(defaultSeats), nameOf) || "当前默认编排暂无 Muteki-Pi 席位")
        : (rosterSummary(defaultSeats, nameOf) || t("composer.templateDefaultEmpty")),
    },
    ...(workerConfig?.dispatch_templates ?? []).map((template): ListOption => {
      const piOnly = filterSeatsForMode(template.seats);
      const hasPi = piOnly.some((seat) => seat.enabled !== false);
      const hasNonPi = template.seats.some((seat) => seat.enabled !== false && seat.engine !== "pi");
      return {
        value: template.id,
        label: template.name,
        textValue: template.name,
        disabled: protectedMode && (!hasPi || hasNonPi),
        leading: <RosterAvatar engines={rosterSlotEngines(protectedMode ? piOnly : template.seats)} size={28} />,
        description: protectedMode
          ? hasPi && !hasNonPi
            ? rosterSummary(piOnly, nameOf)
            : hasPi
              ? `${rosterSummary(piOnly, nameOf)} · 含非 Pi 席位（受保护模式需纯 Muteki-Pi 编排）`
              : "不含 Muteki-Pi 席位（生产保护模式下不可用）"
          : rosterSummary(template.seats, nameOf),
      };
    }),
  ];
  const shownSlots = rosterSlotEngines(protectedMode ? activePiSeats : shownSeats);
  const templateLabel = selectedTemplate?.name ?? t("composer.templateDefault");
  const swarmPicker = (
    <Select
      key="swarm"
      value={selectedTemplate?.id ?? DEFAULT_TEMPLATE}
      onChange={(next) => {
        const id = next === DEFAULT_TEMPLATE ? "" : next;
        setTemplateId(id);
        remember(DISPATCH_TEMPLATE_STORAGE, id);
      }}
      options={templateOptions}
      placement="top-start"
      ariaLabel={t("composer.templatePicker")}
      searchable={templateOptions.length > 8}
      popoverClassName="w-[320px]"
      footer={(close) => <ManageLink label={t("composer.templateManage")} onPress={() => { close(); openSettings("roster"); }} />}
      trigger={(
        <button type="button" aria-label={`${t("composer.templatePicker")}: ${templateLabel}`}
          title={rosterSummary(protectedMode ? activePiSeats : shownSeats, nameOf) || t("composer.templateDefaultEmpty")}
          className={cn(PICKER, "min-w-0 flex-1")}>
          <RosterAvatar engines={shownSlots.length ? shownSlots : (protectedMode ? ["pi"] : [])} size={20} />
          <span className="min-w-0 flex-1 truncate text-left">{templateLabel}</span>
          {shownSlots.length ? <span className="shrink-0 text-cx-xs tabular-nums text-cx-fg-4">{shownSlots.length}</span> : null}
          <Icon name="chevronDown" size={12} className="shrink-0 text-cx-fg-4" />
        </button>
      )}
    />
  );

  const guardHealthBadge: { tone: Tone; label: string } = (() => {
    if (guardrailSnapshot.loading) return { tone: "neutral", label: t("composer.guard.loading") };
    if (guardrailSnapshot.error || !guardrailSnapshot.settings) {
      return { tone: "danger", label: t("composer.guard.unreadable") };
    }
    const cfg = guardrailSnapshot.settings;
    if (!cfg.enabled) return { tone: "warning", label: t("composer.guard.disabled") };
    const probe = cfg.last_probe;
    if (!probe) return { tone: "warning", label: t("composer.guard.unchecked") };
    if (probe.healthy && probe.decision_head_ok && probe.scope_head_ok) {
      return { tone: "success", label: t("composer.guard.healthy", { latency: probe.latency_ms }) };
    }
    if (probe.healthy) return { tone: "warning", label: t("composer.guard.checkFailed") };
    return { tone: "danger", label: t("composer.guard.unreachable") };
  })();

  const productionGuardToggle = pentest ? (
    <Pill
      icon={productionGuardEnabled ? "shieldAlert" : "shield"}
      on={productionGuardEnabled}
      title={t("composer.guard.toggleHint")}
      ariaLabel={t("composer.guard.toggle")}
      label={t("composer.guard.toggle")}
      onPress={() => setProductionGuardEnabled((value) => !value)}
    />
  ) : null;

  const placementHint = remoteNode
    ? t("composer.placementRemoteHint", { node: remoteNode.name })
    : containerLocked ? t("composer.placementLockedHint") : t("composer.placementHint");
  const placementControl = (
    <SegmentedControl<"local" | "container">
      ariaLabel={t("composer.placementRow")}
      value={effectiveContainer ? "container" : "local"}
      options={[
        { value: "local", label: t("composer.containerOff"), disabled: containerLocked || Boolean(remoteNode) },
        { value: "container", label: remoteNode ? remoteNode.name : t("composer.containerOn") },
      ]}
      onChange={(next) => setContainerMode(next === "container")}
    />
  );

  const advancedCount = [
    !pentest && collect,
    !pentest && !webSearch,
    !pentest && allowOperatorInput,
    !pentest && flagFormat !== "brace",
    pentest && reportGoalMode === "count",
    ...(Object.keys(touched) as LimitKey[]).map((key) => Number(limits[key]) > 0),
  ].filter(Boolean).length;
  const advancedTitle = advancedCount ? `${t("composer.advancedTitle")} · ${t("composer.advancedActive", { n: advancedCount })}` : t("composer.advancedTitle");
  const advancedControl = (
    <Pill
      icon="gear" expanded={advancedOpen} controls={ADVANCED_ID} title={advancedTitle}
      label={t("composer.advanced")} onPress={() => setAdvancedOpen((value) => !value)}
      trailing={<>
        {advancedCount ? <span className="rounded-full bg-cx-accent-soft px-1.5 text-cx-2xs leading-4 tabular-nums text-cx-accent">{advancedCount}</span> : null}
        <Icon name="chevronDown" size={12} className={cn("transition-transform", advancedOpen && "rotate-180")} />
      </>}
    />
  );
  const submitLabel = t(pentest ? "composer.startPentest" : "composer.startSolve");

  return (
    <div ref={rootRef} className={cn(
      "cx-root dispatch-composer workbench-welcome-content mx-auto flex flex-col gap-4 pb-8",
      pentest && "dispatch-composer--pentest",
      dispatchPending && "hidden",
    )} data-file-drag={fileDrag ? "true" : undefined}>
      <div className="dc-drop-zone relative">
        <div className="dc-drop-halo pointer-events-none absolute -inset-2 rounded-[28px]" aria-hidden="true">
          <span className="dc-drop-tag">
            <Icon name="upload" size={13} />
            {t("composer.dropArmed")}
          </span>
        </div>
      <PromptBar
        value={text}
        onChange={onTextChange}
        onSubmit={() => void dispatch()}
        submitDisabled={!text.trim() || dispatchPending || uploading || dispatchBlocked || protectedRosterInvalid}
        submitLabel={submitLabel}
        submitDescriptionId={dispatchBlocked ? blockerId : undefined}
        sendKey="mod-enter"
        staticLayout
        placeholder={t(pentest ? "composer.pentestPlaceholder" : "composer.dispatchPlaceholder")}
        composerLabel={t(pentest ? "composer.pentestPlaceholder" : "composer.dispatchPlaceholder")}
        attachments={promptAttachments}
        onAddAttachment={() => fileRef.current?.click()}
        onAddFiles={(files) => void attach(files)}
        onRemoveAttachment={(index) => {
          const file = attachments[index];
          if (file) { onRemoveFile(file.path); return; }
          const row = pending[index - attachments.length];
          if (row) setPending((old) => old.filter((item) => item.id !== row.id));
        }}
        capabilityContext={resolvedExecutionKind === "local_pi"
          ? { adapterId: "pi.rpc", instanceId: "default" }
          : undefined}
        extraControls={(
          <div className="dc-run-row flex min-w-0 items-center gap-1.5">
            {modeSwitch}
            <div className="dc-picker-slot flex min-w-0 items-center">
              {resolvedExecutionKind === "local_pi" ? soloPicker : swarmPicker}
            </div>
            {productionGuardToggle}
          </div>
        )}
        contextMeter={advancedControl}
        undoScope={String(composerUndoEpoch)}
      />
      </div>
      <input
        ref={fileRef} type="file" multiple hidden
        onChange={(event) => {
          const picked = event.target.files ? Array.from(event.target.files) : [];
          event.target.value = "";
          if (picked.length) void attach(picked);
        }}
      />

      {protectedMode ? (
        <section
          role="region"
          aria-labelledby={guardHeadingId}
          data-testid="pentest-production-guard-card"
          className="flex flex-col gap-3 rounded-2xl border border-cx-border bg-cx-elevated px-4 py-3.5 text-cx-xs"
        >
          <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-3">
            <div className="flex min-w-0 flex-[1_1_16rem] items-start gap-2.5">
              <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-cx-accent-soft text-cx-accent" aria-hidden="true">
                <Icon name="shield" size={16} />
              </span>
              <div className="min-w-0">
                <h2 id={guardHeadingId} className="text-cx-sm font-semibold leading-5 text-cx-fg">
                  {t("composer.guard.active")}
                </h2>
                <p className="mt-0.5 leading-5 text-cx-fg-2">{t("composer.guard.summary")}</p>
              </div>
            </div>
            <div className="flex flex-wrap items-center gap-x-2.5 gap-y-2">
              <span role="status">
                <Badge dot tone={guardHealthBadge.tone}>{guardHealthBadge.label}</Badge>
              </span>
              <Button
                size="sm"
                variant="secondary"
                icon="gear"
                onClick={() => {
                  const returnTo = typeof window !== "undefined" ? `${window.location.pathname}${window.location.search}` : "/pentest";
                  router.push(`/pentest/workers?section=guardrail&return=${encodeURIComponent(returnTo)}`);
                }}
              >
                {t("composer.guard.settings")}
              </Button>
            </div>
          </div>
          {guardrailSnapshot.settings?.endpoint ? (
            <dl className="flex flex-wrap gap-x-4 gap-y-1 text-cx-fg-3">
              <div className="flex min-w-0 items-baseline gap-2">
                <dt className="shrink-0">{t("composer.guard.endpoint")}</dt>
                <dd className="min-w-0 wrap-anywhere font-cx-mono text-cx-fg-2">{guardrailSnapshot.settings.endpoint}</dd>
              </div>
              <div className="flex items-baseline gap-2">
                <dt>{t("composer.guard.failurePolicy")}</dt>
                <dd className="text-cx-fg-2">{t(guardrailSnapshot.settings.fail_policy === "fail_closed_deny" ? "composer.guard.deny" : "composer.guard.review")}</dd>
              </div>
            </dl>
          ) : null}
          {activePiSeats.length > 0 || disabledNonPiSeats.length > 0 ? (
            <div className="flex flex-wrap items-center gap-x-2.5 gap-y-1.5 border-t border-cx-border-subtle pt-2.5">
              <span className="shrink-0 text-cx-fg-3">{t("composer.guard.roster")}</span>
              {activePiSeats.map((seat) => (
                <span
                  key={`pi-${seat.id}`}
                  className="inline-flex min-w-0 max-w-full items-center gap-1.5 rounded-md bg-cx-hover px-2 py-0.5 font-medium text-cx-fg-2"
                >
                  <Icon name="check" size={11} className="shrink-0 text-cx-accent" aria-hidden="true" />
                  <span className="min-w-0 wrap-anywhere">{nameOf(seat.engine)}{seat.model ? ` · ${seat.model}` : ""}</span>
                  {(seat.capacity?.max_running ?? 1) > 1 ? <span className="shrink-0 tabular-nums">×{seat.capacity?.max_running}</span> : null}
                </span>
              ))}
              {disabledNonPiSeats.map((seat) => (
                <Tooltip key={`nonpi-${seat.id}`} content={t("composer.guard.externalCli")} placement="top">
                  <span
                    aria-disabled="true"
                    data-guard-disabled-seat={seat.engine}
                    className="inline-flex min-w-0 max-w-full cursor-not-allowed items-center gap-1.5 rounded-md bg-cx-hover px-2 py-0.5 text-cx-fg-3"
                  >
                    <Icon name="lock" size={11} className="shrink-0" aria-hidden="true" />
                    <span className="min-w-0 wrap-anywhere">
                      {t("composer.guard.disabledSeat", { name: seat.model ? `${nameOf(seat.engine)} · ${seat.model}` : nameOf(seat.engine) })}
                    </span>
                  </span>
                </Tooltip>
              ))}
            </div>
          ) : null}
          {activePiSeats.length === 0 ? (
            <p role="status" className="flex items-center gap-1.5 text-cx-warning">
              <Icon name="circleAlert" size={13} className="shrink-0" />
              <span>{t("composer.guard.noPi")}</span>
            </p>
          ) : disabledNonPiSeats.length > 0 ? (
            <p role="status" className="flex items-center gap-1.5 text-cx-warning">
              <Icon name="circleAlert" size={13} className="shrink-0" />
              <span>{t("composer.guard.mixedRoster")}</span>
            </p>
          ) : null}
        </section>
      ) : null}

      <Collapse open={advancedOpen}>
        <div id={ADVANCED_ID} role="region" aria-label={t("composer.advancedTitle")}
          className="dc-advanced flex flex-col gap-5 rounded-2xl border border-cx-border-subtle px-5 py-4">
          <div className="flex items-baseline justify-between gap-3">
            <span className="text-cx-base font-medium text-cx-fg">{t("composer.advancedRunSettings")}</span>
            <span className="text-cx-xs text-[color:var(--dc-soft)]">{t("composer.advancedRunHint")}</span>
          </div>

          <div className="flex flex-col divide-y divide-cx-border-subtle">
            {pentest ? (
              <OptionRow title={t("composer.reportGoalRow")} hint={t("composer.reportGoalRowHint")}>
                {reportGoalMode === "count" ? (
                  <div className="w-[120px]">
                    <NumberField min={1} value={expectedFindings} onChange={setExpectedFindings}
                      scrubLabel="#" ariaLabel="目标报告数量" title="提交多少份独立漏洞报告后结束" />
                  </div>
                ) : null}
                <SegmentedControl<"automatic" | "count">
                  ariaLabel={t("composer.reportGoalRow")}
                  value={reportGoalMode}
                  options={[
                    { value: "automatic", label: t("composer.reportGoalAuto") },
                    { value: "count", label: t("composer.reportGoalCount") },
                  ]}
                  onChange={setReportGoalMode}
                />
              </OptionRow>
            ) : (
              <>
                <OptionRow title={t("composer.collectRow")} hint={t("composer.collectRowHint")}>
                  {collect ? (
                    <div className="w-[164px]">
                      <NumberField min={0} allowEmpty value={collectCount} onChange={setCollectCount}
                        scrubLabel="#" placeholder={t("composer.collectCountPlaceholder")} title={t("composer.collectCountTitle")}
                        ariaLabel={t("composer.collectCountPlaceholder")} />
                    </div>
                  ) : null}
                  <Switch
                    checked={collect}
                    ariaLabel={t("composer.collectTitle")}
                    onCheckedChange={(next) => { setCollect(next); remember(STORAGE.collect, next ? "1" : "0"); }}
                  />
                </OptionRow>
                <OptionRow title={t("composer.webRow")} hint={t("composer.webRowHint")}>
                  <Switch
                    checked={webSearch}
                    ariaLabel={t(webSearch ? "composer.webOnTitle" : "composer.webOffTitle")}
                    onCheckedChange={(next) => { setWebSearch(next); remember(STORAGE.web, next ? "1" : "0"); }}
                  />
                </OptionRow>
              </>
            )}
            <OptionRow title={t("composer.placementRow")} hint={placementHint}>
              {placementControl}
            </OptionRow>
            {pentest ? null : (
              <>
              <OptionRow title={t("composer.operatorInput")} hint={t("composer.operatorInputHint")}>
                <Switch
                  checked={allowOperatorInput}
                  ariaLabel={t(allowOperatorInput ? "composer.operatorInputOnTitle" : "composer.operatorInputOffTitle")}
                  label={<span className="text-cx-xs text-[color:var(--dc-soft)]">{t(allowOperatorInput ? "composer.operatorInputOn" : "composer.operatorInputOff")}</span>}
                  onCheckedChange={(next) => { setAllowOperatorInput(next); remember(STORAGE.operator, next ? "1" : "0"); }}
                />
              </OptionRow>
              <OptionRow title={t("composer.flagFormat")} hint={t("composer.flagFormatHint")}>
                <Select<FlagFormat>
                  value={flagFormat} options={flagOptions} size="sm" ariaLabel={t("composer.flagFormatTitle")}
                  className="w-44"
                  onChange={(next) => { setFlagFormat(next); remember(STORAGE.flagFormat, next); }}
                />
              </OptionRow>
              {flagFormat === "custom" ? (
                <div className="py-3.5 pb-0">
                  <Input
                    size="sm" value={flagWrapper} placeholder={t("composer.flagWrapperPlaceholder")}
                    aria-label={t("composer.flagWrapperTitle")} title={t("composer.flagWrapperTitle")}
                    onChange={(event) => { setFlagWrapper(event.target.value); remember(STORAGE.flagWrapper, event.target.value); }}
                  />
                </div>
              ) : null}
              </>
            )}
          </div>

          <div className="flex flex-col gap-3 border-t border-cx-border-subtle pt-5">
            <div className="flex items-baseline justify-between gap-3">
              <span className="text-cx-sm font-medium text-cx-fg">{t("composer.advancedLimits")}</span>
              <span className="text-cx-xs text-[color:var(--dc-soft)]">{t("composer.advancedLimitsHint")}</span>
            </div>
            <div className="grid grid-cols-2 gap-x-3 gap-y-4 @[560px]:grid-cols-3">
              <LimitField label={t("composer.wallBudget")} hint={t("composer.zeroUnlimited")}>
                <NumberField min={0} suffix="s" value={limits.wallClockBudget} onChange={setLimit("wallClockBudget")}
                  ariaLabel={t("composer.wallBudget")} title={t("composer.wallBudgetTitle")} />
              </LimitField>
              <LimitField label={t("composer.maxTotalWorkers")} hint={t("composer.zeroUnlimited")}>
                <NumberField min={0} value={limits.maxTotalWorkers} onChange={setLimit("maxTotalWorkers")}
                  ariaLabel={t("composer.maxTotalWorkers")} title={t("composer.maxTotalWorkersTitle")} />
              </LimitField>
              <LimitField label={t("composer.costBudget")} hint={t("composer.zeroUnlimited")}>
                <NumberField min={0} step={0.01} suffix="USD" value={limits.costBudgetUsd} onChange={setLimit("costBudgetUsd")}
                  ariaLabel={t("composer.costBudget")} title={t("composer.costBudgetTitle")} />
              </LimitField>
            </div>
          </div>
        </div>
      </Collapse>

      {dispatchBlocked ? (
        <div className="flex flex-col gap-2 rounded-xl border border-cx-border-subtle px-3 py-2 text-cx-sm">
          <div id={blockerId} role="status" className="flex flex-col gap-1.5 text-cx-warning">
            {blockers.map(({ message, section }) => <p key={message} className="flex flex-wrap items-start gap-x-2 gap-y-1">
              <Icon name="circleAlert" size={15} className="mt-0.5 shrink-0" aria-hidden="true" />
              <span className="min-w-0">{message}</span>
              {section ? (
                <button type="button" onClick={() => openSettings(section)}
                  className="rounded-sm font-medium text-cx-fg underline underline-offset-4 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]">
                  {t(section === "roster" ? "composer.configureRoster" : "composer.configurePlanner")}
                </button>
              ) : null}
            </p>)}
          </div>
          {workerConfig !== undefined ? (
            <Link href="/settings" className="w-fit rounded-sm font-medium text-cx-fg underline underline-offset-4 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]">
              {t("composer.openSettingsCenter")}
            </Link>
          ) : null}
        </div>
      ) : null}

      {below}

      <div className="flex flex-col items-center gap-1.5 px-2 pt-2 text-center text-cx-xs leading-5 text-[color:var(--dc-soft)]">
        {pentest && reportGoalMode === "count" && expectedFindings ? (
          <p className="font-medium text-cx-fg-2">{t("composer.reportCountHint", { count: expectedFindings })}</p>
        ) : null}
        <ul className={cn("flex flex-wrap items-center justify-center gap-y-1", pentest ? "gap-x-4" : "gap-x-3")}>
          <li className="inline-flex items-center gap-1"><Shortcut keys="mod+enter" tone="subtle" />{submitLabel}</li>
          <li className="inline-flex items-center gap-1"><Shortcut keys={t("composer.focusKey")} tone="subtle" />{t("composer.focusHint")}</li>
          <li className="inline-flex items-center gap-1"><Shortcut keys="mod+k" tone="subtle" />{t("palette.hint")}</li>
        </ul>
      </div>
    </div>
  );
}
