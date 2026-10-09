"use client";

import { ConversationRouteLink } from "@/components/conversation/ConversationNavigation";
import { Fragment, useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { motion } from "motion/react";
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
import {
  effectiveServiceTier, modelEffortLevels, modelServiceTiers, rememberedModelEffort, rememberModelEffort, rememberServiceTier,
} from "@/lib/modelReasoning";
import { isModelHidden, readHiddenModels, subscribeHiddenModels } from "@/lib/modelVisibility";
import {
  engineDisplayName, isKnownEngine, loadProviderDescriptors, providerEngines, readyCatalog, useProviderDescriptors,
} from "@/lib/providerDescriptors";
import { isMacPlatform, matchesBinding, shortcutBinding } from "@/lib/shortcutBindings";
import { useMediaQuery } from "@/lib/useMediaQuery";
import { EngineLogo } from "@/components/EngineLogo";
import { Icon } from "@/components/Icon";
import {
  Badge,
  Button,
  Callout,
  EmptyState,
  Popover,
  Kbd,
  Select,
  Skeleton,
  splitShortcut,
  StatusDot,
  Tooltip,
  useControllableOpen,
  useListKeyboard,
  type ListOption,
  type Tone,
} from "@/components/chat/ui";
import { useReducedMotion } from "@/components/chat/ui/motion";
import {
  allCredentialModels,
  type ConversationCredential,
  type ConversationCredentialModel,
  type RuntimeInstance,
} from "@/lib/useConversation";
import { ComposerEffortSlider, serviceTierLabel } from "./ComposerEffortSlider";
import { effortLabelOf } from "./ConversationComposerModes";

export interface ConversationModelPickerProps {
  credentials: ConversationCredential[];
  selectedCredentialId: string;
  selectedModel: string;
  /** Thinking effort of the bound model; together with onSelect it adds the effort section. */
  selectedEffort?: string;
  selectedServiceTier?: string;
  runtimes?: RuntimeInstance[];
  runtimeKey?: string;
  onRuntimeChange?: (key: string) => void;
  loading?: boolean;
  error?: string;
  onRetry?: () => void;
  variant?: "conversation" | "default-model";
  onSelect: (params: { credentialId: string; model: string; effort?: string; serviceTier?: string }) => void;
  className?: string;
  /** Controlled popover state (e.g. bound to mod+shift+m by the Shell). */
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
}

const FAVORITES_TAB = "\0favorites";
const SEARCH_RESULT_LIMIT = 200;
const RAIL_FADE = 18;
const INDICATOR_SPRING = { type: "spring", stiffness: 520, damping: 38, mass: 0.7 } as const;

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

/** One rail dot per engine: the healthiest of its endpoints. */
function engineTone(rows: ConversationCredential[]): Tone {
  const order: Tone[] = ["success", "warning", "danger", "neutral"];
  return rows.map(credentialTone).sort((a, b) => order.indexOf(a) - order.indexOf(b))[0] || "neutral";
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

interface ModelRow {
  key: string;
  credential: ConversationCredential;
  model: ConversationCredentialModel;
  verified: boolean;
}

function isVerified(credential: ConversationCredential, modelId: string): boolean {
  return credential.models.some((probed) => probed.id === modelId);
}

function credentialRows(credential: ConversationCredential): ModelRow[] {
  return allCredentialModels(credential).map((model) => ({
    key: favoriteKey(credential.id, model.id),
    credential,
    model,
    verified: isVerified(credential, model.id),
  }));
}

/** Every whitespace-separated term must appear in the model, account or Agent name. */
function matchesTerms(terms: string[], row: ModelRow, engineLabel: string): boolean {
  if (!terms.length) return true;
  const haystack = `${row.model.label} ${row.model.id} ${row.credential.label} ${engineLabel}`.toLowerCase();
  return terms.every((term) => haystack.includes(term));
}

const UNVERIFIED_DETAIL = "此凭据下还没有该模型的成功使用记录，仍可选择。成功完成一次聊天后会自动标记为已验证，也可在设置 → Agents 中执行「真实连通测试」。";

function SettingsLink() {
  return (
    <ConversationRouteLink
      href="/settings/agents"
      className="inline-flex items-center gap-1 text-[12px] font-medium text-cx-accent hover:underline hover:underline-offset-4"
    >
      前往 Agents 设置
      <Icon name="arrowUpRight" size={12} />
    </ConversationRouteLink>
  );
}

function PickerSkeleton() {
  return (
    <div className="flex flex-col" data-testid="conversation-model-loading" role="status" aria-label="加载中">
      <div className="flex h-11 items-center gap-1.5 border-b border-cx-border-subtle px-2.5">
        {[0, 1, 2, 3, 4].map((row) => <Skeleton key={row} className="size-7 rounded-lg" />)}
      </div>
      <div className="flex flex-col gap-3 p-3.5">
        <Skeleton className="h-4 w-2/5" />
        {[0, 1, 2, 3, 4].map((row) => <Skeleton key={row} className="h-3.5 w-3/5" />)}
      </div>
    </div>
  );
}

export function ConversationModelPicker({
  credentials, selectedCredentialId, selectedModel, selectedEffort, selectedServiceTier = "",
  loading: catalogLoading = false, error: catalogError = "", onRetry: retryCatalog, variant = "conversation",
  runtimes, runtimeKey = "", onRuntimeChange,
  onSelect, className = "", open: openProp, onOpenChange,
}: ConversationModelPickerProps) {
  const { lang } = useLang();
  const en = lang === "en";
  const reduced = useReducedMotion();
  const [open, setOpen] = useControllableOpen(openProp, false, onOpenChange);
  const [query, setQuery] = useState("");
  const [favorites, setFavorites] = useState<FavoriteModel[]>([]);
  /** FAVORITES_TAB or an engine id; the endpoint inside an engine is the second level. */
  const [activeTab, setActiveTab] = useState("");
  const [endpointByEngine, setEndpointByEngine] = useState<Record<string, string>>({});
  // #200: ≤480px long model ids wrap instead of truncating, and ⌘ hints hide.
  const narrowStacked = useMediaQuery(NARROW_MODEL_PICKER_MQ);
  const [favoriteStatus, setFavoriteStatus] = useState("");
  // Models hidden in 设置 → Agents stay selectable nowhere except as the
  // already-bound selection, so the trigger label of a hidden model survives.
  const [hiddenModels, setHiddenModels] = useState<ReadonlySet<string>>(() => readHiddenModels());
  useEffect(() => subscribeHiddenModels(() => setHiddenModels(readHiddenModels())), []);
  /** Opened through the effort shortcut: focus lands on the slider instead of search. */
  const [effortFocus, setEffortFocus] = useState(false);
  const listId = useId();
  const modelListScrollRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement | null>(null);
  const railRef = useRef<HTMLDivElement>(null);
  const endpointsRef = useRef<HTMLDivElement>(null);
  const effortRef = useRef<HTMLDivElement>(null);
  const [railEdges, setRailEdges] = useState({ start: false, end: false });
  const [indicator, setIndicator] = useState<{ x: number; width: number } | null>(null);
  const measureRailEdges = useCallback(() => {
    const el = railRef.current;
    if (!el) return;
    const max = el.scrollWidth - el.clientWidth;
    const next = { start: el.scrollLeft > 1, end: el.scrollLeft < max - 1 };
    setRailEdges((prev) => (prev.start === next.start && prev.end === next.end ? prev : next));
  }, []);
  const descriptorState = useProviderDescriptors();
  const descriptors = readyCatalog(descriptorState);
  const loading = catalogLoading || descriptorState.status === "loading";
  const error = catalogError || (descriptorState.status === "error" ? descriptorState.message : "");
  const descriptorFailed = descriptorState.status === "error";
  const onRetry = retryCatalog || descriptorFailed
    ? () => {
      if (descriptorFailed) void loadProviderDescriptors({ fresh: true }).catch(() => undefined);
      retryCatalog?.();
    }
    : undefined;
  const engineLabel = (engine: string) => engineDisplayName(descriptors, engine);
  const listedCredentials = useMemo(
    () => credentials
      .filter((credential) => isKnownEngine(descriptors, credential.engine))
      .map((credential) => {
        const keep = (model: ConversationCredentialModel) =>
          (credential.id === selectedCredentialId && model.id === selectedModel)
          || !isModelHidden(hiddenModels, credential.id, model.id);
        const models = credential.models.filter(keep);
        const candidateModels = credential.candidate_models.filter(keep);
        return models.length === credential.models.length && candidateModels.length === credential.candidate_models.length
          ? credential
          : { ...credential, models, candidate_models: candidateModels };
      }),
    [credentials, descriptors, hiddenModels, selectedCredentialId, selectedModel],
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
  const boundModel = useMemo(
    () => boundCredential ? allCredentialModels(boundCredential).find((item) => item.id === selectedModel) || null : null,
    [boundCredential, selectedModel],
  );
  const browseCredential = boundCredential || availableCredentials[0] || listedCredentials[0];
  // Engine order first, usable accounts before unusable ones inside each engine.
  const railGroups = useMemo(
    () => providerEngines(descriptors)
      .map((engine) => {
        const rows = listedCredentials.filter((item) => item.engine === engine);
        return { engine, rows: [...rows.filter(usableCredential), ...rows.filter((item) => !usableCredential(item))] };
      })
      .filter((group) => group.rows.length > 0),
    [descriptors, listedCredentials],
  );
  const tabOrder = useMemo(() => [FAVORITES_TAB, ...railGroups.map((group) => group.engine)], [railGroups]);
  const hasUnconfiguredEngines = providerEngines(descriptors).some((engine) => !listedCredentials.some((item) => item.engine === engine));

  // Thinking effort and speed always describe the bound model, whatever tab is being browsed.
  const effortLevels = useMemo(() => modelEffortLevels(boundModel), [boundModel]);
  const serviceTiers = useMemo(() => modelServiceTiers(boundModel), [boundModel]);
  const activeTier = effectiveServiceTier(boundModel, selectedServiceTier);
  const activeTierMeta = serviceTiers.find((tier) => tier.id === activeTier);
  const activeEffort = !selectedEffort || selectedEffort === "default" || !effortLevels.includes(selectedEffort) ? "" : selectedEffort;
  const variantNames = boundModel?.reasoning?.kind === "variant";
  const levelLabel = (level: string) => variantNames && level ? level : effortLabelOf(level);
  const modelDefault = boundModel?.reasoning?.default && effortLevels.includes(boundModel.reasoning.default)
    ? boundModel.reasoning.default : "";
  const showEffort = variant === "conversation" && selectedEffort !== undefined
    && Boolean(boundCredential && boundModel && (effortLevels.length || serviceTiers.length));
  const effortText = effortLevels.length ? levelLabel(activeEffort || modelDefault) : "";
  const effortSummary = [
    effortLevels.length ? `思考强度：${levelLabel(activeEffort || modelDefault)}${activeEffort ? "" : "（默认）"}` : "",
    serviceTiers.length ? `速度：${serviceTierLabel(activeTierMeta)}` : "",
  ].filter(Boolean).join(" · ");
  const effortMemoryKey = boundCredential ? `${boundCredential.id}:${boundCredential.runtime_instance || ""}` : "";
  const chooseEffort = (level: string) => {
    if (!boundCredential || !boundModel || level === activeEffort) return;
    rememberModelEffort(effortMemoryKey, boundModel, level);
    onSelect({ credentialId: boundCredential.id, model: boundModel.id, effort: level });
  };
  const chooseServiceTier = (tier: string) => {
    if (!boundCredential || !boundModel || tier === activeTier) return;
    rememberServiceTier(tier);
    onSelect({ credentialId: boundCredential.id, model: boundModel.id, serviceTier: tier });
  };
  // Models with only a speed tier have no slider; their first control takes focus instead.
  const focusEffort = () => (effortRef.current?.querySelector<HTMLElement>("[role='slider']")
    ?? effortRef.current?.querySelector<HTMLElement>("[role='radio'], button"))?.focus({ preventScroll: true });

  useEffect(() => {
    if (!showEffort) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (!matchesBinding(event, shortcutBinding("effortPicker"), isMacPlatform())) return;
      event.preventDefault();
      if (!open) {
        setEffortFocus(true);
        setOpen(true);
      } else if (effortRef.current?.contains(document.activeElement)) {
        setOpen(false);
      } else {
        focusEffort();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [showEffort, open, setOpen]);

  useEffect(() => {
    if (!open) {
      setEffortFocus(false);
      return;
    }
    if (!effortFocus) return;
    const frame = requestAnimationFrame(focusEffort);
    return () => cancelAnimationFrame(frame);
  }, [open, effortFocus]);

  useEffect(() => {
    if (loading) return;
    // A transient/failed catalog is not evidence that a saved account or model
    // was deleted. Only an explicit favorite toggle removes persisted entries.
    setFavorites(readFavoriteModels());
  }, [listedCredentials, loading]);

  const favoriteKeys = useMemo(
    () => new Set(favorites.map((item) => favoriteKey(item.credentialId, item.modelId))),
    [favorites],
  );

  // Open on Favorites only when the current selection lives there; otherwise on its own Agent.
  // Re-run once a catalog that was still loading at open time arrives: the search box only mounts then.
  const catalogReady = !loading && !error && listedCredentials.length > 0;
  useEffect(() => {
    if (!open || !catalogReady) return;
    setQuery("");
    const selectedIsFavorite = boundCredential && readFavoriteModels()
      .some((item) => item.credentialId === boundCredential.id && item.modelId === selectedModel);
    setActiveTab(selectedIsFavorite || !browseCredential ? FAVORITES_TAB : browseCredential.engine);
    setEndpointByEngine(browseCredential ? { [browseCredential.engine]: browseCredential.id } : {});
    if (effortFocus) return;
    const frame = requestAnimationFrame(() => {
      const active = document.activeElement;
      if (!active || active === document.body || active.getAttribute("role") === "dialog") searchRef.current?.focus({ preventScroll: true });
    });
    return () => cancelAnimationFrame(frame);
    // Only on open / first ready catalog; later refreshes must not yank the tab away.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, catalogReady]);

  const favoritesMode = activeTab === FAVORITES_TAB;
  const activeGroup = favoritesMode
    ? undefined
    : railGroups.find((group) => group.engine === activeTab) || railGroups.find((group) => group.engine === browseCredential?.engine);
  const activeCredential = activeGroup
    ? activeGroup.rows.find((item) => item.id === endpointByEngine[activeGroup.engine])
      || activeGroup.rows.find((item) => item.id === selectedCredentialId)
      || activeGroup.rows[0]
    : undefined;
  const terms = useMemo(() => query.trim().toLowerCase().split(/\s+/).filter(Boolean), [query]);
  const searching = terms.length > 0;

  const favoriteRows = useMemo<ModelRow[]>(() => favorites.flatMap((item) => {
    const credential = availableCredentials.find((row) => row.id === item.credentialId);
    const model = credential && allCredentialModels(credential).find((row) => row.id === item.modelId);
    if (!credential || !model) return [];
    return [{ key: favoriteKey(credential.id, model.id), credential, model, verified: isVerified(credential, model.id) }];
  }), [availableCredentials, favorites]);

  const searchMatches = useMemo<ModelRow[]>(() => {
    if (!searching) return [];
    const rows = railGroups.flatMap((group) => group.rows.flatMap(credentialRows)).filter((row) => matchesTerms(terms, row, engineDisplayName(descriptors, row.credential.engine)));
    const rank = (row: ModelRow) => (favoriteKeys.has(row.key) ? 0 : 2) + (usableCredential(row.credential) ? 0 : 1);
    return rows.map((row, index) => ({ row, index }))
      .sort((a, b) => rank(a.row) - rank(b.row) || a.index - b.index)
      .map((item) => item.row);
  }, [descriptors, favoriteKeys, railGroups, searching, terms]);

  const visibleRows = useMemo<ModelRow[]>(() => {
    if (searching) return searchMatches.slice(0, SEARCH_RESULT_LIMIT);
    if (favoritesMode) return favoriteRows;
    if (!activeCredential) return [];
    const rows = credentialRows(activeCredential);
    return [...rows.filter((row) => row.verified), ...rows.filter((row) => !row.verified)];
  }, [activeCredential, favoriteRows, favoritesMode, searchMatches, searching]);
  const showProvider = searching || favoritesMode;
  const firstUnverifiedIndex = visibleRows.findIndex((row) => !row.verified);
  const groupedByVerification = !showProvider && firstUnverifiedIndex > 0;

  const toggleFavorite = (credentialId: string, modelId: string, modelLabel: string) => {
    const key = favoriteKey(credentialId, modelId);
    const nextFavorited = !favoriteKeys.has(key);
    setFavorites((current) => {
      const next = current.some((item) => favoriteKey(item.credentialId, item.modelId) === key)
        ? current.filter((item) => favoriteKey(item.credentialId, item.modelId) !== key)
        : [...current, { credentialId, modelId }];
      persistFavoriteModels(next);
      return next;
    });
    setFavoriteStatus(favoriteToggleAnnouncement(modelLabel, nextFavorited));
  };

  const selectModel = (row: ModelRow) => {
    if (!usableCredential(row.credential)) return;
    onSelect({
      credentialId: row.credential.id,
      model: row.model.id,
      effort: variant === "conversation"
        ? rememberedModelEffort(`${row.credential.id}:${row.credential.runtime_instance || ""}`, row.model)
        : undefined,
    });
    setOpen(false);
  };

  const keyboardOptions = useMemo<ListOption[]>(
    () => visibleRows.map((row) => ({ value: row.key, label: row.model.label, disabled: !usableCredential(row.credential) })),
    [visibleRows],
  );
  const keyboard = useListKeyboard(keyboardOptions, (key) => {
    const row = visibleRows.find((item) => item.key === key);
    if (row) selectModel(row);
  });

  useEffect(() => {
    if (!open) return;
    const preferred = visibleRows.findIndex(
      (row) => row.credential.id === selectedCredentialId && row.model.id === selectedModel,
    );
    // Keep activeIndex valid when search / Agent tab changes (#196).
    keyboard.setActiveIndex(resolveActiveIndexAfterListChange(visibleRows.length, preferred));
    // Reset the highlight only when the list identity changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, activeTab, activeCredential?.id, query]);

  useEffect(() => {
    if (!open) return;
    scrollActiveOptionIntoView(modelListScrollRef.current, keyboard.activeIndex);
  }, [open, keyboard.activeIndex, visibleRows]);

  // Keep the selected Agent out of the faded edges and park the underline under it.
  useEffect(() => {
    if (!open) return;
    let observer: ResizeObserver | undefined;
    const frame = requestAnimationFrame(() => {
      const el = railRef.current;
      if (!el) return;
      const tab = el.querySelector<HTMLElement>(`[data-tab="${CSS.escape(activeTab || FAVORITES_TAB)}"]`);
      if (tab) {
        const start = el.scrollLeft;
        const end = start + el.clientWidth;
        const itemStart = tab.offsetLeft;
        const itemEnd = itemStart + tab.offsetWidth;
        const delta = itemStart < start + RAIL_FADE
          ? itemStart - start - RAIL_FADE
          : itemEnd > end - RAIL_FADE ? itemEnd - end + RAIL_FADE : 0;
        if (delta) el.scrollLeft += delta;
        const width = Math.max(12, tab.offsetWidth - 12);
        setIndicator({ x: itemStart + (tab.offsetWidth - width) / 2, width });
      }
      measureRailEdges();
      observer = new ResizeObserver(measureRailEdges);
      observer.observe(el);
    });
    return () => {
      cancelAnimationFrame(frame);
      observer?.disconnect();
    };
  }, [open, activeTab, loading, railGroups.length, measureRailEdges]);

  useEffect(() => {
    if (!open) setIndicator(null);
  }, [open]);

  const switchTab = (tab: string, focus: "rail" | "search" | "none" = "none") => {
    setActiveTab(tab);
    setQuery("");
    if (focus === "search") searchRef.current?.focus();
    if (focus === "rail") {
      requestAnimationFrame(() => railRef.current?.querySelector<HTMLElement>(`[data-tab="${CSS.escape(tab)}"]`)?.focus());
    }
  };
  const stepTab = (delta: number, focus: "rail" | "search") => {
    const current = Math.max(0, tabOrder.indexOf(activeTab));
    switchTab(tabOrder[(current + delta + tabOrder.length) % tabOrder.length], focus);
  };
  // Endpoint chips appear only while browsing one engine that has a choice to make
  // (or whose only endpoint needs attention); search and favorites span engines.
  const showEndpoints = Boolean(
    activeGroup && !searching && !favoritesMode
    && (activeGroup.rows.length > 1 || activeGroup.rows.some((row) => !usableCredential(row))),
  );
  const focusEndpoint = (id: string) => requestAnimationFrame(() => {
    endpointsRef.current?.querySelector<HTMLElement>(`[data-endpoint="${CSS.escape(id)}"]`)?.focus();
  });
  const selectEndpoint = (credential: ConversationCredential, focus: "endpoints" | "search" | "none" = "none") => {
    setEndpointByEngine((current) => ({ ...current, [credential.engine]: credential.id }));
    setQuery("");
    if (focus === "search") searchRef.current?.focus();
    if (focus === "endpoints") focusEndpoint(credential.id);
  };
  const onEndpointsKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const rows = activeGroup?.rows || [];
    const current = Math.max(0, rows.findIndex((row) => row.id === activeCredential?.id));
    if ((event.key === "ArrowRight" || event.key === "ArrowLeft") && rows.length) {
      event.preventDefault();
      selectEndpoint(rows[(current + (event.key === "ArrowRight" ? 1 : -1) + rows.length) % rows.length], "endpoints");
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      switchTab(activeTab, "rail");
    } else if (event.key === "ArrowDown" || event.key === "Enter") {
      event.preventDefault();
      searchRef.current?.focus();
    }
  };

  const onSearchKeyDown = (event: React.KeyboardEvent<HTMLInputElement>) => {
    // IME candidate confirm must not favorite / navigate (#197).
    if (isImeComposingKeyEvent(event)) return;
    const mod = event.metaKey || event.ctrlKey;
    if (isFavoriteToggleKey(event)) {
      const row = visibleRows[keyboard.activeIndex];
      if (!row) return;
      event.preventDefault();
      toggleFavorite(row.credential.id, row.model.id, row.model.label);
      return;
    }
    if (mod && !event.shiftKey && !event.altKey && /^Digit[1-9]$/.test(event.code)) {
      const row = visibleRows[Number(event.code.slice(5)) - 1];
      event.preventDefault();
      if (row) selectModel(row);
      return;
    }
    if (mod && event.shiftKey && (event.key === "ArrowDown" || event.key === "ArrowUp" || event.key === "ArrowRight" || event.key === "ArrowLeft")) {
      event.preventDefault();
      stepTab(event.key === "ArrowDown" || event.key === "ArrowRight" ? 1 : -1, "search");
      return;
    }
    const input = event.currentTarget;
    if (event.key === "ArrowUp" && !input.value && keyboard.activeIndex <= 0) {
      event.preventDefault();
      switchTab(activeTab || FAVORITES_TAB, "rail");
      return;
    }
    keyboard.onKeyDown(event);
  };

  const onRailKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (event.key === "ArrowRight" || event.key === "ArrowLeft") {
      event.preventDefault();
      stepTab(event.key === "ArrowRight" ? 1 : -1, "rail");
    } else if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      switchTab(event.key === "Home" ? tabOrder[0] : tabOrder[tabOrder.length - 1], "rail");
    } else if (event.key === "ArrowDown" || event.key === "Enter") {
      event.preventDefault();
      searchRef.current?.focus();
    }
  };

  const triggerUnavailable = Boolean(boundCredential && (!usableCredential(boundCredential) || (selectedModel && !boundModel)));
  const triggerAria = boundCredential && (boundModel || selectedModel)
    ? `${engineLabel(boundCredential.engine)} · ${boundCredential.label} · ${boundModel?.label || selectedModel}${showEffort && effortSummary ? ` · ${effortSummary}` : ""}`
    : "选择模型";
  const triggerText = boundModel?.label
    || (boundCredential && selectedModel)
    || (loading ? "加载中…" : error ? "加载失败" : "选择模型");
  const triggerEngine = boundCredential?.engine || browseCredential?.engine || "omp";

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
      aria-keyshortcuts={showEffort ? "Meta+Shift+M Meta+Shift+E" : "Meta+Shift+M"}
      data-testid="composer-model-trigger"
      data-service-tier={showEffort && activeTier ? activeTier : undefined}
      className={cn(
        "cx-press inline-flex h-8 min-w-0 max-w-[300px] items-center gap-1.5 rounded-full px-2.5 text-[13px] font-medium text-cx-fg-2",
        "hover:bg-cx-hover hover:text-cx-fg data-[state=open]:bg-cx-active data-[state=open]:text-cx-fg",
        "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
      )}
    >
      <EngineLogo engine={triggerEngine} size={14} className="shrink-0" />
      <span className={cn("min-w-0 truncate", triggerUnavailable && "text-cx-fg-3")}>{triggerText}</span>
      {triggerUnavailable ? <Badge tone="warning" className="h-[18px] shrink-0 px-1.5 text-[11px]">不可用</Badge> : null}
      {showEffort && effortText && !narrowStacked ? (
        <span className="shrink-0 font-normal text-cx-fg-4" data-testid="composer-effort-label">{effortText}</span>
      ) : null}
      {showEffort && activeTierMeta ? <Icon name="zap" size={12} className="shrink-0 text-cx-warning" aria-hidden /> : null}
      <Icon name="chevronDown" size={12} className="shrink-0 text-cx-fg-4" />
    </button>
  );

  const renderRow = (row: ModelRow, index: number) => {
    const selected = row.credential.id === selectedCredentialId && row.model.id === selectedModel;
    const favorited = favoriteKeys.has(row.key);
    const active = index === keyboard.activeIndex;
    const disabled = !usableCredential(row.credential);
    const isDefault = row.credential.default_model === row.model.id;
    const showId = row.model.label !== row.model.id;
    const rowAriaLabel = showId ? `${row.model.label}（${row.model.id}）` : row.model.id;
    const unverifiedBadge = !row.verified && !groupedByVerification;
    return (
      <Fragment key={row.key}>
        {groupedByVerification && index === firstUnverifiedIndex ? (
          <div role="presentation" className="flex items-center gap-1.5 px-2.5 pb-1 pt-3 text-[11px] font-medium text-cx-fg-4">
            <span>未验证</span>
            <Tooltip content={UNVERIFIED_DETAIL}>
              <span className="inline-flex cursor-help"><Icon name="info" size={11} /></span>
            </Tooltip>
            <span className="cx-tabular ml-auto">{visibleRows.length - firstUnverifiedIndex}</span>
          </div>
        ) : null}
        <div
          id={`${listId}-opt-${index}`}
          role="option"
          aria-label={rowAriaLabel}
          aria-selected={selected}
          aria-disabled={disabled || undefined}
          title={disabled ? credentialStatusLabel(row.credential) || "该接入点暂不可用" : showId ? row.model.id : undefined}
          data-index={index}
          data-active={active || undefined}
          data-selected={selected || undefined}
          onPointerMove={() => { if (!active) keyboard.setActiveIndex(index); }}
          onPointerDown={(event) => event.preventDefault()}
          onClick={() => selectModel(row)}
          className={cn(
            "group flex min-h-9 cursor-default select-none gap-2 rounded-lg px-2.5 py-1.5",
            narrowStacked ? "items-start" : "items-center",
            "data-[active=true]:bg-cx-hover",
            disabled && "opacity-45",
          )}
        >
          {showProvider ? <EngineLogo engine={row.credential.engine} size={15} className={cn("shrink-0", narrowStacked && "mt-0.5")} /> : null}
          <span className="flex min-w-0 flex-1 flex-col">
            <span className={cn("flex min-w-0 gap-1.5", narrowStacked ? "flex-wrap items-start" : "items-center")}>
              <span
                className={cn(
                  "text-[13.5px] leading-5",
                  selected ? "font-semibold text-cx-fg" : "font-medium text-cx-fg",
                  narrowStacked ? "break-words [overflow-wrap:anywhere]" : "truncate",
                )}
              >
                {row.model.label}
              </span>
              {isDefault ? <Badge tone="accent" className="h-[18px] shrink-0 px-1.5 text-[11px]">默认</Badge> : null}
              {unverifiedBadge ? (
                <Tooltip content={UNVERIFIED_DETAIL}>
                  <span className="inline-flex shrink-0"><Badge tone="warning" className="h-[18px] px-1.5 text-[11px]">未验证</Badge></span>
                </Tooltip>
              ) : null}
            </span>
            {showProvider ? (
              <span className={cn("text-[11.5px] leading-4 text-cx-fg-4", narrowStacked ? "break-words [overflow-wrap:anywhere]" : "truncate")}>
                {`${engineLabel(row.credential.engine)} · ${row.credential.label}`}
              </span>
            ) : null}
          </span>
          {index < 9 && !narrowStacked ? (
            <Kbd
              tone="subtle"
              className={cn("h-5 shrink-0 rounded-full px-2 text-[11px] transition-opacity", active || selected ? "opacity-100" : "opacity-60 group-hover:opacity-100")}
            >
              {splitShortcut(`mod+${index + 1}`).join("")}
            </Kbd>
          ) : null}
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
              "grid size-6 shrink-0 place-items-center rounded-md transition-colors hover:bg-cx-active",
              favorited ? "text-cx-warning" : "text-cx-fg-4/70 hover:text-cx-fg-2",
            )}
          >
            <Icon name="star" size={13} filled={favorited} />
          </button>
          <Icon name="check" size={14} className={cn("shrink-0 text-cx-accent", narrowStacked && "mt-1", selected ? "opacity-100" : "opacity-0")} />
        </div>
      </Fragment>
    );
  };

  const railTab = (tab: string, content: React.ReactNode, tooltip: React.ReactNode, label: string, dimmed = false) => {
    const selected = !searching && activeTab === tab;
    return (
      <Tooltip key={tab} content={tooltip} placement="bottom">
        <button
          type="button"
          role="tab"
          data-tab={tab}
          aria-selected={selected}
          aria-label={label}
          tabIndex={selected || (searching && tab === activeTab) ? 0 : -1}
          onClick={() => switchTab(tab, "search")}
          className={cn(
            "cx-press grid size-8 shrink-0 place-items-center rounded-lg text-cx-fg-3 outline-none transition-colors",
            "hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--cx-focus)]",
            selected && "text-cx-fg",
          )}
        >
          <span className={cn("grid place-items-center", dimmed && "opacity-45")}>{content}</span>
        </button>
      </Tooltip>
    );
  };

  const fadeStart = railEdges.start ? RAIL_FADE : 0;
  const fadeEnd = railEdges.end ? RAIL_FADE : 0;
  const railMask = fadeStart || fadeEnd
    ? `linear-gradient(to right, transparent 0, #000 ${fadeStart}px, #000 calc(100% - ${fadeEnd}px), transparent 100%)`
    : undefined;

  const rail = (
    <div className="flex h-11 shrink-0 items-center gap-1 border-b border-cx-border-subtle pl-1.5 pr-2">
      <div
        ref={railRef}
        role="tablist"
        aria-label="Agent"
        aria-orientation="horizontal"
        onKeyDown={onRailKeyDown}
        onScroll={measureRailEdges}
        className="cx-no-scrollbar relative flex h-full min-w-0 flex-1 items-center gap-0.5 overflow-x-auto overflow-y-hidden overscroll-contain px-0.5"
        style={railMask ? { maskImage: railMask, WebkitMaskImage: railMask } : undefined}
      >
        {railTab(
          FAVORITES_TAB,
          <Icon name="star" size={15} filled={favoriteRows.length > 0} className={favoriteRows.length ? "text-cx-warning" : undefined} />,
          `收藏 · ${favoriteRows.length} 个模型`,
          `查看收藏模型，共 ${favoriteRows.length} 个`,
        )}
        {railGroups.map(({ engine, rows }) => {
          const usable = rows.filter(usableCredential).length;
          const models = rows.reduce((sum, row) => sum + allCredentialModels(row).length, 0);
          return railTab(
            engine,
            <span className="relative grid place-items-center">
              <EngineLogo engine={engine} size={17} />
              <StatusDot tone={engineTone(rows)} className="absolute -bottom-1 -right-1.5 ring-2 ring-[var(--cx-overlay)]" />
            </span>,
            <span className="flex flex-col gap-0.5">
              <span className="font-medium">{engineLabel(engine)}</span>
              <span className="opacity-70">{`${rows.length} 个接入点${usable < rows.length ? `（${usable} 个可用）` : ""} · ${models} 个模型`}</span>
            </span>,
            `${engineLabel(engine)}，${rows.length} 个接入点${usable ? "" : "，均不可用"}`,
            usable === 0,
          );
        })}
        {indicator ? (
          <motion.span
            aria-hidden
            className="pointer-events-none absolute bottom-0 left-0 h-[2px] rounded-full bg-cx-accent"
            initial={false}
            animate={{ x: indicator.x, width: indicator.width, opacity: searching ? 0 : 1 }}
            transition={reduced ? { duration: 0 } : INDICATOR_SPRING}
          />
        ) : null}
      </div>
      <Tooltip content={hasUnconfiguredEngines ? "添加或管理 Agent" : "管理 Agent"} placement="bottom">
        <ConversationRouteLink
          href="/settings/agents"
          aria-label={hasUnconfiguredEngines ? "添加或管理 Agent" : "管理 Agent"}
          className="grid size-8 shrink-0 place-items-center rounded-lg text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg"
        >
          <Icon name={hasUnconfiguredEngines ? "plus" : "gear"} size={15} />
        </ConversationRouteLink>
      </Tooltip>
    </div>
  );

  const searchRow = (
    <div className="flex h-10 shrink-0 items-center gap-2.5 border-b border-cx-border-subtle px-3.5">
      <Icon name="search" size={14} className="shrink-0 text-cx-fg-4" />
      <input
        ref={searchRef}
        value={query}
        onChange={(event) => { setQuery(event.target.value); keyboard.setActiveIndex(0); }}
        onKeyDown={onSearchKeyDown}
        placeholder="搜索全部模型…"
        aria-label="搜索全部 Agent 的模型；Alt+S 收藏当前项，⌘1–9 直接选择"
        autoComplete="off"
        spellCheck={false}
        role="combobox"
        aria-expanded
        aria-controls={listId}
        aria-activedescendant={keyboard.activeIndex >= 0 && visibleRows.length ? `${listId}-opt-${keyboard.activeIndex}` : undefined}
        data-autofocus
        className="h-full min-w-0 flex-1 bg-transparent text-[13.5px] text-cx-fg outline-none placeholder:text-cx-fg-4"
      />
      {searching ? (
        <span className="cx-tabular shrink-0 text-[12px] text-cx-fg-4" aria-live="polite">
          {searchMatches.length > SEARCH_RESULT_LIMIT ? `前 ${SEARCH_RESULT_LIMIT} / ${searchMatches.length}` : searchMatches.length}
        </span>
      ) : null}
      {query ? (
        <button
          type="button"
          aria-label="清除搜索"
          onClick={() => { setQuery(""); searchRef.current?.focus(); }}
          className="grid size-5 shrink-0 place-items-center rounded-full text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg"
        >
          <Icon name="x" size={12} />
        </button>
      ) : null}
    </div>
  );

  const endpoints = showEndpoints && activeGroup ? (
    <div
      ref={endpointsRef}
      role="tablist"
      aria-label={`${engineLabel(activeGroup.engine)} 接入点`}
      aria-orientation="horizontal"
      onKeyDown={onEndpointsKeyDown}
      className="cx-no-scrollbar flex h-10 shrink-0 items-center gap-1 overflow-x-auto overflow-y-hidden overscroll-contain px-2 pt-1"
    >
      {activeGroup.rows.map((credential) => {
        const selected = credential.id === activeCredential?.id;
        const status = credentialStatusLabel(credential);
        const holdsSelection = credential.id === selectedCredentialId;
        const detail = status || `${endpointKind(credential)} · ${allCredentialModels(credential).length} 个模型`;
        return (
          <Tooltip key={credential.id} content={detail} placement="bottom">
            <button
              type="button"
              role="tab"
              data-endpoint={credential.id}
              aria-selected={selected}
              aria-label={`${credential.label}，${endpointKind(credential)}${status ? `，${status}` : ""}${holdsSelection ? "，当前使用中" : ""}`}
              tabIndex={selected ? 0 : -1}
              onClick={() => selectEndpoint(credential, "search")}
              className={cn(
                "cx-press inline-flex h-7 min-w-0 max-w-[200px] shrink-0 items-center gap-1.5 rounded-full px-2.5 text-[12.5px] text-cx-fg-3 outline-none",
                "hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--cx-focus)]",
                selected && "bg-cx-active font-medium text-cx-fg",
                !usableCredential(credential) && "opacity-60",
              )}
            >
              <StatusDot tone={credentialTone(credential)} className="shrink-0" />
              <span className="truncate">{credential.label}</span>
              {holdsSelection ? <Icon name="check" size={11} className="shrink-0 text-cx-accent" /> : null}
            </button>
          </Tooltip>
        );
      })}
    </div>
  ) : null;

  const emptyTitle = searching
    ? "没有匹配的模型"
    : favoritesMode
      ? "还没有收藏"
      : credentialStatusLabel(activeCredential) || "该接入点暂无可用模型";

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
    <div className="flex min-h-0 min-w-0 flex-1 flex-col" data-narrow-stacked={narrowStacked || undefined}>
      {rail}
      {searchRow}
      {endpoints}
      <div className="sr-only" role="status" aria-live="polite" aria-atomic="true">
        {favoriteStatus}
      </div>
      <div
        ref={modelListScrollRef}
        id={listId}
        role="listbox"
        aria-label={searching ? "搜索结果" : favoritesMode ? "收藏模型" : activeCredential ? `${engineLabel(activeCredential.engine)} · ${activeCredential.label} 的模型` : "可用模型"}
        aria-keyshortcuts="Alt+S"
        className="cx-scroll h-[min(248px,calc(100vh-400px))] min-h-[128px] overflow-y-auto overflow-x-hidden overscroll-contain p-1.5"
      >
        {visibleRows.length ? visibleRows.map(renderRow) : (
          <EmptyState
            compact
            icon={favoritesMode && !searching ? "star" : "search"}
            title={emptyTitle}
            description={favoritesMode && !searching ? "在模型行上点星标或按 Alt+S 收藏常用组合。" : undefined}
            action={favoritesMode || searching ? undefined : <SettingsLink />}
          />
        )}
      </div>
    </div>
  );

  const catalogShown = !loading && !error && listedCredentials.length > 0;

  const effortSection = showEffort && catalogShown ? (
    <div ref={effortRef} className="shrink-0 border-t border-cx-border-subtle" data-testid="composer-effort-section">
      <ComposerEffortSlider
        embedded
        levels={effortLevels}
        value={activeEffort}
        modelDefault={modelDefault}
        modelLabel={boundModel?.label || boundModel?.id}
        labelOf={levelLabel}
        onChange={chooseEffort}
        serviceTiers={serviceTiers}
        serviceTier={activeTier}
        onServiceTierChange={chooseServiceTier}
      />
    </div>
  ) : null;

  const runtimeCandidates = boundCredential ? (runtimes || []).filter((runtime) => runtime.engine === boundCredential.engine) : [];
  const showRuntime = variant === "conversation" && (runtimes !== undefined || onRuntimeChange !== undefined);
  const allRuntimesDisabled = runtimeCandidates.length > 0 && !runtimeCandidates.some((runtime) => runtime.enabled !== false);
  const runtimeDisabled = !onRuntimeChange || !boundCredential || boundCredential.present === false || allRuntimesDisabled
    || !runtimeCandidates.length;
  const selectedRuntime = runtimeCandidates.find((runtime) => runtime.key === runtimeKey);
  const runtimeValue = selectedRuntime
    ? `${selectedRuntime.adapter_id} · ${selectedRuntime.instance_id}`
    : !boundCredential || boundCredential.present === false
      ? (en ? "Select a credential first" : "请先选择凭据")
      : !runtimeCandidates.length ? (en ? "No Runtime for this Agent" : "此 Agent 没有 Runtime")
        : allRuntimesDisabled ? (en ? "All Runtime instances are disabled" : "此 Agent 的 Runtime 均已停用")
          : runtimeKey ? (en ? "Current Runtime unavailable; select again" : "当前 Runtime 不可用，请重新选择")
            : (en ? "Select Runtime" : "选择 Runtime");
  const runtimeNeedsAttention = !selectedRuntime && Boolean(runtimeKey) && !runtimeDisabled;

  const runtimeRow = showRuntime && catalogShown ? (
    <div className="shrink-0 border-t border-cx-border-subtle">
      <Select
        ariaLabel={en ? "Conversation Runtime" : "会话 Runtime"}
        value={selectedRuntime?.key || null}
        disabled={runtimeDisabled}
        onChange={(key) => {
          if (runtimeDisabled || key === runtimeKey) return;
          const runtime = runtimeCandidates.find((row) => row.key === key && row.enabled !== false);
          if (runtime) onRuntimeChange?.(runtime.key);
        }}
        placement="top-end"
        popoverClassName="w-[min(380px,calc(100vw-24px))]"
        trigger={(
          <button
            type="button"
            disabled={runtimeDisabled}
            aria-label={`${en ? "Conversation Runtime" : "会话 Runtime"}：${runtimeValue}`}
            title={en ? "Runtime that runs this conversation and reports model capabilities" : "运行本对话并上报模型能力的 Runtime"}
            className={cn(
              "flex h-10 w-full min-w-0 items-center gap-3 px-3.5 text-left text-[13px] outline-none",
              "hover:bg-cx-hover focus-visible:bg-cx-hover data-[state=open]:bg-cx-hover",
              "disabled:cursor-default disabled:hover:bg-transparent",
            )}
            data-testid="conversation-runtime-trigger"
          >
            <span className="shrink-0 font-medium text-cx-fg-2">Runtime</span>
            <span className={cn("ml-auto min-w-0 truncate", runtimeNeedsAttention ? "text-cx-warning" : "text-cx-fg-3")}>{runtimeValue}</span>
            {runtimeDisabled ? null : <Icon name="chevronRight" size={13} className="shrink-0 text-cx-fg-4" />}
          </button>
        )}
        options={runtimeCandidates.map((runtime) => ({
          value: runtime.key,
          label: `${runtime.adapter_id} · ${runtime.instance_id}`,
          description: [
            runtime.enabled === false ? (en ? "Disabled" : "已停用") : "",
            runtime.health?.healthy === true ? (en ? "Reported healthy" : "上报状态正常")
              : runtime.health?.healthy === false ? (en ? "Reported unhealthy" : "上报状态异常")
                : (en ? "Not probed" : "未探测"),
            runtime.health?.detail || "",
          ].filter(Boolean).join(" · "),
          disabled: runtime.enabled === false,
        }))}
      />
    </div>
  ) : null;

  const popover = (
    <Popover
      open={open}
      onOpenChange={setOpen}
      trigger={trigger}
      placement={variant === "conversation" ? "top-start" : "bottom-start"}
      offset={8}
      initialFocus={effortFocus ? "none" : "first"}
      ariaLabel={showEffort ? "选择 Agent、模型和思考强度" : "选择 Agent 和模型"}
      className="w-[min(400px,calc(100vw-16px))] p-0"
    >
      {body}
      {effortSection}
      {runtimeRow}
    </Popover>
  );

  return (
    <div className={cn("inline-flex min-w-0 items-center", variant === "default-model" && "w-full", className)}>
      {variant === "conversation" ? (
        <Tooltip
          content={triggerAria}
          shortcut={splitShortcut("mod+shift+m")}
          disabled={open}
        >
          <span className="inline-flex min-w-0">{popover}</span>
        </Tooltip>
      ) : popover}
    </div>
  );
}
