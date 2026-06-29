"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import {
  CredentialAccount,
  EngineHealth,
  SystemLoginStatus,
  WorkerImageStatus,
  WorkerModelOptions,
  WorkerSettings as WS,
  deleteCredentialAccount,
  getEngineHealth,
  getSystemLogin,
  getWorkerImageStatus,
  getWorkerSettings,
  getWorkerModelOptions,
  listCredentialAccounts,
  pullWorkerImage,
  putCredentialAccount,
  putRuntimeEnvironment,
  putWorkerSettings,
  testCredentialAccount,
  testLlmEndpoint,
  testWorkerProfileModel,
  fetchProfilesHealth,
  testProfileHealth,
  importHostCodexAuth,
  type ProfileHealth,
} from "@/lib/useRun";
import { useT } from "@/lib/i18n";
import { Icon, IconName } from "@/components/Icon";

/**
 * Global worker config — ONE config reused by every solve (no per-run /
 * per-worker layer). Redesign v2 (DESIGN_settings_redesign): the panel is a
 * modal with a LEFT tab rail + RIGHT content area, organised by user intent
 * rather than backend structure:
 *
 *   Roster    — who dispatches: engine + name + model (the SOLE model entry
 *               point) + an enabled toggle (config kept when off) + a per-card
 *               readiness chain; expand for race / capacity / priority / delete.
 *   Accounts  — one credential per dispatched engine; hard blocker in container.
 *   Runtime   — backend (container/local) + network mode + worker-image health.
 *   Budget    — scheduling + budgets; review is one toggle that reveals its form.
 *   Advanced  — reasoning models (planner/titler) + engine self-check.
 *
 * A persistent config-health bar surfaces cross-tab fatal dependencies (missing
 * account, capacity shortfall) and deep-links to the failing control. Edits live
 * in a DRAFT buffer: validation runs live so readiness updates as you type, but
 * nothing persists until Save. The header badge + footer make "draft vs active"
 * unmistakable — the highest-risk failure mode is an operator believing a config
 * is live when the change that validates it was never saved.
 */

type WorkerProfile = WS["worker_profiles"][number];
type AccountType = "claude" | "codex" | "cursor" | "api";
type Backend = "local" | "container";
type NetworkMode = "bridge" | "host" | "none";
type Tab = "roster" | "accounts" | "runtime" | "budget" | "advanced";
type Severity = "ok" | "amber" | "red";

const BASE_ENGINES = ["claude", "codex", "cursor"] as const;
const ORDINARY_PROFILE_ROLES = new Set(["race", "bootstrap", "explore", "respond"]);
// env var + default that govern the container worker image (server-side:
// muteki/solver/container_exec.py WORKER_IMAGE). Surfaced in the Runtime tab so
// the operator knows what to set; documented in .env.example.
const WORKER_IMAGE_ENV = "MUTEKI_WORKER_IMAGE";
const WORKER_IMAGE_DEFAULT = "snowywar/muteki-worker:latest";

// prefer the human-readable label; fall back to name/id. After the identity
// migration a profile's id/name is an opaque seat id (seat_claude_ab12cd), so
// without the label the UI would show that instead of e.g. "claude-local".
const profileName = (p: WorkerProfile): string => (p as { label?: string }).label || p.name || p.id;
const profileRefName = (p: WorkerProfile): string => p.name || p.id || profileName(p);
const isEnabled = (p: WorkerProfile): boolean => p.enabled !== false;

const profileHasOrdinaryWorkerRole = (p: WorkerProfile): boolean =>
  (p.roles || []).some((r) => ORDINARY_PROFILE_ROLES.has(String(r)));

const enabledOrdinaryProfiles = (profiles: WorkerProfile[]): WorkerProfile[] =>
  profiles.filter((p) => isEnabled(p) && profileHasOrdinaryWorkerRole(p));

const enabledDispatchRefs = (profiles: WorkerProfile[]): string[] =>
  enabledOrdinaryProfiles(profiles).map(profileRefName);

const profileCapacity = (profiles: WorkerProfile[]): number =>
  enabledOrdinaryProfiles(profiles)
    .reduce((sum, p) => sum + Math.max(1, Number(p.max_running ?? 1) || 1), 0);

// ── worker composer: per-base-engine defaults for a freshly-added instance ──
// Mirrors apps/web/worker_config.py DEFAULT_WORKER_PROFILES so a new instance is
// valid the moment it's created (the backend re-normalizes, but we want sensible
// fields immediately and a stable credential_account that shares the engine's one
// subscription — three codex instances all authenticate as "codex-main").
const ENGINE_DEFAULTS: Record<string, { transport: string; wire_api: string; roles: string[] }> = {
  claude: { transport: "claude_code", wire_api: "", roles: ["race", "bootstrap", "explore", "review"] },
  codex: { transport: "codex_cli", wire_api: "responses", roles: ["race", "bootstrap", "explore", "review"] },
  cursor: { transport: "cursor_agent", wire_api: "", roles: ["race", "bootstrap", "explore", "review"] },
};

// A unique profile id/name for a new instance of `engine` that won't collide with
// existing profiles (the backend dedupes by id, so "codex" + "codex-2" + "codex-3"
// stay distinct). First instance of an engine gets the bare base name when free.
const nextInstanceId = (engine: string, existing: WorkerProfile[]): string => {
  const taken = new Set(existing.map((p) => p.name || p.id));
  if (!taken.has(engine)) return engine;
  for (let n = 2; n < 999; n += 1) {
    const id = `${engine}-${n}`;
    if (!taken.has(id)) return id;
  }
  return `${engine}-${Date.now()}`;
};

const makeInstance = (engine: string, existing: WorkerProfile[], runtime: string): WorkerProfile => {
  const d = ENGINE_DEFAULTS[engine] ?? ENGINE_DEFAULTS.claude;
  const id = nextInstanceId(engine, existing);
  const maxPrio = existing.reduce((m, p) => Math.max(m, Number(p.priority ?? 100)), 0);
  return {
    id,
    name: id,
    engine,
    transport: d.transport,
    auth: "subscription",
    credential_mode: "subscription",
    credential_account: `${engine}-main`,
    wire_api: d.wire_api,
    runtime,
    roles: [...d.roles],
    race: true,
    max_running: 1,
    max_review_running: 0,
    priority: maxPrio + 10,
    model: "",
    enabled: true,
  };
};

export function WorkerSettings({ open, onClose }: { open: boolean; onClose: () => void }) {
  const t = useT();
  const [cfg, setCfg] = useState<WS | null>(null);
  const [tab, setTab] = useState<Tab>("runtime");
  // DRAFT buffer dirtiness: true the moment any field is edited, cleared on
  // (re)load and after a successful save. Header badge + footer key off this so
  // the operator always knows whether they're looking at the live config or an
  // unsaved draft.
  const [dirty, setDirty] = useState(false);
  const [engines, setEngines] = useState<string[]>([]);
  const [startWorkers, setStartWorkers] = useState(3);
  const [maxWorkers, setMaxWorkers] = useState(10);
  const [workerBackend, setWorkerBackend] = useState<Backend>("container");
  const [runtimeId, setRuntimeId] = useState("docker-web");
  // 坑 C: users pick a NETWORK mode, not an image/recipe. networkMode is the UI
  // control; it resolves to the matching container runtime preset on save.
  const [networkMode, setNetworkMode] = useState<NetworkMode>("bridge");
  // P2-v3: worker-image health (daemon / pulled / version) + one-click pull.
  const [imageStatus, setImageStatus] = useState<WorkerImageStatus | null>(null);
  const [imageLoading, setImageLoading] = useState(false);
  const [pulling, setPulling] = useState(false);
  const [raceScout, setRaceScout] = useState(true);
  const [raceTimeout, setRaceTimeout] = useState(720);
  const [reviewEnabled, setReviewEnabled] = useState(true);
  const [reviewEngine, setReviewEngine] = useState("claude-sub-container");
  const [reviewTimeout, setReviewTimeout] = useState(420);
  const [reviewMaxConcurrent, setReviewMaxConcurrent] = useState(1);
  const [reviewCandidateThreshold, setReviewCandidateThreshold] = useState(5);
  const [reviewFallback, setReviewFallback] = useState(false);
  const [reviewPolicy, setReviewPolicy] = useState<NonNullable<WS["stage_policy"]["coordinator"]["review"]>>({});
  const [wallClockBudget, setWallClockBudget] = useState(0);
  const [maxTotalWorkers, setMaxTotalWorkers] = useState(0);
  const [costBudgetUsd, setCostBudgetUsd] = useState(0);
  const [plannerModel, setPlannerModel] = useState("deepseek-v4-pro");
  const [titlerModel, setTitlerModel] = useState("deepseek-v4-flash");
  const [llmBaseUrl, setLlmBaseUrl] = useState("");
  const [llmTest, setLlmTest] = useState<{ ok: boolean; detail: string } | null>(null);
  const [llmTesting, setLlmTesting] = useState(false);
  const [accounts, setAccounts] = useState<CredentialAccount[]>([]);
  const [sysLogin, setSysLogin] = useState<Record<string, SystemLoginStatus>>({});
  const [accountId, setAccountId] = useState("claude-main");
  const [accountType, setAccountType] = useState<AccountType>("claude");
  // For a custom endpoint (type "api") this names WHICH agent the base_url+key
  // overrides — persisted as the account's ENGINE marker so the panel can bind &
  // display it instead of an orphan "api".
  const [accountApiEngine, setAccountApiEngine] = useState<"claude" | "codex" | "cursor">("claude");
  const [accountSecret, setAccountSecret] = useState("");
  const [accountBaseUrl, setAccountBaseUrl] = useState("");
  const [accountStatus, setAccountStatus] = useState<"idle" | "saving" | "saved" | "error">("idle");
  // When editing an existing account: its id + the metadata we surface. base_url
  // is non-sensitive so we pre-fill its actual value; the SECRET is write-only so
  // we never get it back — we only know it's PRESENT (has_secret) and show that as
  // a read-only indicator. Null = the form is in "add new" mode. In edit mode the
  // secret may be left blank to keep the stored credential.
  const [editingAccount, setEditingAccount] = useState<
    null | { account_id: string; base_url_set: boolean; has_secret: boolean; updated_at?: number | null }
  >(null);
  // Whether the SECRET field shows plaintext (eye toggle). Always reset to masked
  // when (re)loading the form so a revealed secret never carries across accounts.
  const [showSecret, setShowSecret] = useState(false);
  const [formTest, setFormTest] = useState<{ ok: boolean; detail: string; layer?: string; testing?: boolean } | null>(null);
  const [workerProfiles, setWorkerProfiles] = useState<WorkerProfile[]>([]);
  // Per-profile readiness from the SERVER (single source of truth shared with the
  // dispatch precheck). `profileHealth` holds the cheap binding-depth verdict for
  // every profile (re-fetched when the config/backend changes). `profileAuth`
  // caches the DEEP auth verdict from a "测连通" click, keyed by a tuple that
  // includes the account's updated_at so a freshly re-uploaded token (same
  // account_id) busts the stale verdict rather than showing a phantom pass/fail.
  const [profileHealth, setProfileHealth] = useState<Record<string, ProfileHealth>>({});
  const [profileAuth, setProfileAuth] = useState<Record<string, ProfileHealth & { testing?: boolean }>>({});
  // account_id currently being re-imported from the host ~/.codex (button spinner).
  const [importingCodex, setImportingCodex] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [customModelOpen, setCustomModelOpen] = useState<Record<string, boolean>>({});
  // composer drag state: index of the card being dragged, and the index it's
  // hovering over (for the drop-indicator). Null when no drag is active.
  const [dragIdx, setDragIdx] = useState<number | null>(null);
  const [dragOverIdx, setDragOverIdx] = useState<number | null>(null);
  const [modelOptions, setModelOptions] = useState<WorkerModelOptions>({ allow_custom: true, models: {} });
  const [modelTest, setModelTest] = useState<Record<string, { ok: boolean; detail: string; testing?: boolean }>>({});
  const [status, setStatus] = useState<"idle" | "saving" | "saved" | "error">("idle");
  const [health, setHealth] = useState<EngineHealth[] | null>(null);
  const [checking, setChecking] = useState(false);

  const modalRef = useRef<HTMLDivElement | null>(null);
  const triggerRef = useRef<HTMLElement | null>(null);

  useEffect(() => {
    if (!open) return;
    setStatus("idle");
    setDirty(false);
    setTab("runtime");
    setLlmTest(null);
    setCustomModelOpen({});
    getWorkerSettings().then((c) => {
      if (!c) return;
      setCfg(c);
      setEngines(c.engines);
      setStartWorkers(c.start_workers);
      setMaxWorkers(c.max_workers);
      setWorkerBackend(c.worker_backend ?? "container");
      setRaceScout(c.race_scout ?? true);
      setRaceTimeout(c.race_timeout ?? 720);
      const rv = c.stage_policy?.coordinator?.review ?? {};
      setReviewPolicy(rv);
      setReviewEnabled(rv.enabled ?? true);
      setReviewEngine(rv.engine ?? "claude-sub-container");
      setReviewTimeout(rv.timeout ?? 420);
      setReviewMaxConcurrent(rv.max_concurrent ?? 1);
      setReviewCandidateThreshold(rv.candidate_spike_threshold ?? 5);
      setReviewFallback(rv.allow_review_fallback ?? false);
      setWallClockBudget(c.wall_clock_budget ?? 0);
      setMaxTotalWorkers(c.max_total_workers ?? 0);
      setCostBudgetUsd(c.cost_budget_usd ?? 0);
      setPlannerModel(c.llm_profiles?.planner?.model ?? "deepseek-v4-pro");
      setTitlerModel(c.llm_profiles?.titler?.model ?? "deepseek-v4-flash");
      setLlmBaseUrl(c.llm_profiles?.planner?.base_url ?? "");
      setWorkerProfiles(c.worker_profiles ?? []);
      const rt = (c.worker_profiles ?? [])[0]?.runtime
        || (c.worker_backend === "local" ? "local" : "docker-web");
      setRuntimeId(rt);
      const rtNet = (c.runtime_profiles ?? []).find((r) => r.id === rt)?.network;
      setNetworkMode((rtNet === "host" || rtNet === "none") ? rtNet : "bridge");
    });
    listCredentialAccounts().then(setAccounts);
    getSystemLogin().then(setSysLogin);
    getWorkerModelOptions().then(setModelOptions);
    fetchProfilesHealth().then((hs) =>
      setProfileHealth(Object.fromEntries(hs.map((h) => [h.profile_id, h]))));
    setImageLoading(true);
    getWorkerImageStatus().then((s) => { setImageStatus(s); setImageLoading(false); });
  }, [open]);

  const refreshImage = () => {
    setImageLoading(true);
    getWorkerImageStatus().then((s) => { setImageStatus(s); setImageLoading(false); });
  };
  const doPullImage = async () => {
    setPulling(true);
    await pullWorkerImage();
    setPulling(false);
    refreshImage();
  };

  // Esc-to-close + focus trap + focus restore.
  useEffect(() => {
    if (!open) return;
    triggerRef.current = (document.activeElement as HTMLElement) ?? null;
    const modal = modalRef.current;
    const focusables = () =>
      modal
        ? Array.from(
            modal.querySelectorAll<HTMLElement>(
              'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])'
            )
          ).filter((el) => el.offsetParent !== null)
        : [];
    focusables()[0]?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        onClose();
        return;
      }
      if (e.key !== "Tab") return;
      const list = focusables();
      if (list.length === 0) return;
      const first = list[0];
      const last = list[list.length - 1];
      const active = document.activeElement as HTMLElement | null;
      if (e.shiftKey && (active === first || !modal?.contains(active))) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && active === last) {
        e.preventDefault();
        first.focus();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("keydown", onKey, true);
      triggerRef.current?.focus?.();
    };
  }, [open, onClose]);

  const localRuntimeId = useMemo(
    () => (cfg?.runtime_profiles ?? []).find((r) => r.backend === "local")?.id ?? "local",
    [cfg]
  );
  // 坑 C: resolve the chosen NETWORK mode to the matching container runtime preset
  // id (bridge→docker-web, host→docker-host-target, none→docker-offline).
  const containerRuntimeForNetwork = useMemo(() => {
    const presets = (cfg?.runtime_profiles ?? []).filter((r) => r.backend === "container");
    return (net: NetworkMode): string => {
      const match = presets.find((r) => r.network === net);
      if (match) return match.id;
      const fallback: Record<NetworkMode, string> = {
        bridge: "docker-web", host: "docker-host-target", none: "docker-offline",
      };
      return fallback[net];
    };
  }, [cfg]);

  // Per-profile readiness comes from the SERVER (single source of truth shared
  // with the dispatch precheck). A profile whose binding verdict is `blocked`
  // (e.g. container-mode with no bound account) won't start a worker.
  const profileBlocker = (p: WorkerProfile): string | null => {
    if (!isEnabled(p)) return null;
    const h = profileHealth[profileName(p)] ?? profileHealth[p.id];
    return h?.status === "blocked" ? (h.blocker || p.engine) : null;
  };

  const blockedProfiles = useMemo(
    () => workerProfiles.filter((p) => profileBlocker(p) !== null),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [workerProfiles, profileHealth]
  );

  const enabledProfiles = useMemo(
    () => workerProfiles.filter(isEnabled),
    [workerProfiles]
  );

  const engineOptions = useMemo(
    () => workerProfiles.length > 0
      ? workerProfiles.map((p) => p.name || p.id)
      : ["claude", "codex", "cursor"],
    [workerProfiles]
  );
  const reviewOptions = useMemo(() => {
    const reviewProfiles = (workerProfiles || []).filter((p) =>
      isEnabled(p) && ((p.roles || []).includes("review"))
    );
    return reviewProfiles.length > 0
      ? reviewProfiles.map((p) => p.name || p.id)
      : engineOptions;
  }, [workerProfiles, engineOptions]);

  useEffect(() => {
    if (!open || reviewOptions.length === 0) return;
    if (!reviewEngine || !reviewOptions.includes(reviewEngine)) {
      setReviewEngine(reviewOptions[0]);
    }
  }, [open, reviewEngine, reviewOptions]);

  // max_workers is a READ-ONLY derived ceiling = sum of enabled ordinary roster
  // seats' per-run concurrency. The operator edits per-seat capacity in the
  // roster; the global max tracks that sum live (up AND down).
  const selectedCapacity = useMemo(
    () => profileCapacity(workerProfiles),
    [workerProfiles]
  );
  const derivedMaxWorkers = Math.max(1, selectedCapacity);
  // Keep the persisted/displayed maxWorkers mirror in lock-step with the roster
  // sum so the (read-only) budget field reflects edits before save.
  useEffect(() => {
    if (selectedCapacity > 0 && maxWorkers !== derivedMaxWorkers) {
      setMaxWorkers(derivedMaxWorkers);
    }
    if (selectedCapacity > 0 && startWorkers > derivedMaxWorkers) {
      setStartWorkers(derivedMaxWorkers);
    }
  }, [derivedMaxWorkers, maxWorkers, selectedCapacity, startWorkers]);

  // ── config-health bar severity (Codex hard rule) ────────────────────────────
  //   red   = a run cannot start OR an enabled profile cannot execute
  //   amber = runnable but degraded / risky / needs attention
  //   ok    = valid + actionable
  //
  // Three honest account states per profile (no more "green = an account of this
  // engine exists" lie):
  //   unbound   — required account not bound / not present  → blocked (red)
  //   unverified— bound, but no deep auth probe has confirmed it can authenticate
  //   verified  — a "测连通" auth hello (or the binding-only check for a profile
  //               that needs no auth probe) confirmed it.
  type AcctState = "unbound" | "unverified" | "verified" | "auth_failed" | "n/a";
  const profileAcctState = (p: WorkerProfile): AcctState => {
    if (!isEnabled(p)) return "n/a";
    const key = profileName(p);
    const binding = profileHealth[key] ?? profileHealth[p.id];
    const auth = profileAuth[key];
    if (binding?.status === "blocked") return "unbound";
    if (auth && !auth.testing) return auth.status === "ok" ? "verified" : "auth_failed";
    // binding ok and the kernel said this profile needs no auth probe (e.g. a bare
    // host subscription) → it's as verified as it gets without a hello.
    if (binding?.status === "ok" && binding.detail === "no auth probe required") return "verified";
    return binding?.status === "ok" ? "unverified" : "n/a";
  };
  const acctStates = useMemo(
    () => enabledProfiles.map(profileAcctState),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [enabledProfiles, profileHealth, profileAuth]
  );

  const rosterSev: Severity = enabledProfiles.length === 0 ? "amber"
    : blockedProfiles.length > 0 ? "red" : "ok";
  const acctSev: Severity =
    acctStates.some((s) => s === "unbound" || s === "auth_failed") ? "red"
    : acctStates.some((s) => s === "unverified") ? "amber" : "ok";
  const budgetSev: Severity = wallClockBudget === 0 && maxTotalWorkers === 0 && costBudgetUsd === 0 ? "amber" : "ok";

  if (!open) return null;

  const mark = () => { setDirty(true); setStatus("idle"); };

  const networkLabel = (n: NetworkMode) =>
    n === "host" ? t("settings.netHost") : n === "none" ? t("settings.netNone") : t("settings.netBridge");
  const backendLabel = workerBackend === "container"
    ? `${t("settings.runtimeContainer")} · ${networkLabel(networkMode)}`
    : t("settings.runtimeLocal");

  const baseEngineForRef = (ref: string, profiles: WorkerProfile[]): string | undefined => {
    const exact = profiles.find((p) => profileName(p) === ref || p.id === ref || p.name === ref);
    if (exact?.engine) return exact.engine;
    if ((BASE_ENGINES as readonly string[]).includes(ref)) return ref;
    return ref.split("-").find((part) => (BASE_ENGINES as readonly string[]).includes(part));
  };
  const alignProfileRef = (
    ref: string | undefined,
    nextProfiles: WorkerProfile[],
    prevProfiles: WorkerProfile[],
    fallback?: string
  ): string | undefined => {
    if (!ref) return fallback;
    const nextByName = new Map<string, WorkerProfile>();
    for (const p of nextProfiles) {
      for (const key of [profileName(p), profileRefName(p), p.id, p.name]) {
        if (key) nextByName.set(key, p);
      }
    }
    const exact = nextByName.get(ref);
    if (exact) return profileRefName(exact);
    const base = baseEngineForRef(ref, prevProfiles) || baseEngineForRef(ref, nextProfiles);
    const mapped = base ? nextProfiles.find((p) => p.engine === base) : undefined;
    return mapped ? profileRefName(mapped) : fallback;
  };
  const alignProfileRefs = (
    refs: string[],
    nextProfiles: WorkerProfile[],
    prevProfiles: WorkerProfile[]
  ): string[] => {
    const out: string[] = [];
    for (const ref of refs) {
      const mapped = alignProfileRef(ref, nextProfiles, prevProfiles);
      if (mapped && !out.includes(mapped)) out.push(mapped);
    }
    return out;
  };

  // The chip's display label tracks the run environment, not the profile's
  // historical name. In local mode show the base engine (from the authoritative
  // `engine` field); in container mode keep the profile name. Duplicates keep
  // their full name so chips stay distinguishable.
  const engineLabel = (name: string): string => {
    if (workerBackend !== "local") return name;
    const prof = (workerProfiles || []).find((p) => (p.name || p.id) === name);
    if (!prof?.engine) return name;
    const sameEngine = (workerProfiles || []).filter((p) => p.engine === prof.engine);
    return sameEngine.length > 1 ? name : prof.engine;
  };

  const save = async () => {
    if (enabledDispatchRefs(workerProfiles).length === 0) {
      setStatus("error");
      return;
    }
    setStatus("saving");
    // 1) run environment write-back (unifies backend + runtime across all
    //    profiles) — first so worker_profiles below reflect the chosen runtime.
    const wantedRuntime = workerBackend === "local"
      ? localRuntimeId
      : containerRuntimeForNetwork(networkMode);
    const rtCfg = await putRuntimeEnvironment(workerBackend, wantedRuntime);
    const currentById = new Map(workerProfiles.map((p) => [p.id, p]));
    const currentByName = new Map<string, WorkerProfile>();
    for (const p of workerProfiles) {
      for (const key of [profileName(p), profileRefName(p), p.id, p.name]) {
        if (key) currentByName.set(key, p);
      }
    }
    const engineCounts = workerProfiles.reduce<Record<string, number>>((acc, p) => {
      acc[p.engine] = (acc[p.engine] ?? 0) + 1;
      return acc;
    }, {});
    const currentBySingleEngine = new Map(
      workerProfiles
        .filter((p) => engineCounts[p.engine] === 1)
        .map((p) => [p.engine, p])
    );
    const mergeProfileEdits = (p: WorkerProfile): WorkerProfile => {
      const current = currentById.get(p.id)
        || currentByName.get(profileName(p))
        || currentBySingleEngine.get(p.engine);
      if (!current) return p;
      const editable: Partial<WorkerProfile> = { ...current };
      delete editable.id;
      delete editable.name;
      delete editable.runtime;
      return { ...p, ...editable, id: p.id, name: p.name, runtime: p.runtime };
    };
    // Card ORDER is authoritative for the profile list and its priority. Drive
    // profilesToSave from live composer state, apply the unified runtime, pull
    // back backend-side field changes, and stamp priority strictly from order
    // (top card = priority 10). The enabled ordinary roster is also the dispatch
    // roster, so the saved max_workers sum cannot lag behind visible cards.
    const rtById = new Map((rtCfg?.worker_profiles ?? []).map((p) => [p.id, p]));
    const profilesToSave = workerProfiles.map((p, i) => {
      const base = { ...(rtById.get(p.id) ?? p), id: p.id, name: p.name };
      const merged = mergeProfileEdits(base);
      return { ...merged, runtime: wantedRuntime, priority: (i + 1) * 10, enabled: isEnabled(p) };
    });
    const nextEngines = alignProfileRefs(enabledDispatchRefs(profilesToSave), profilesToSave, workerProfiles);
    if (nextEngines.length === 0) {
      setStatus("error");
      return;
    }
    const nextMaxWorkers = Math.max(1, profileCapacity(profilesToSave));
    const nextStartWorkers = Math.min(startWorkers, nextMaxWorkers);
    const nextReviewEngine = alignProfileRef(
      reviewEngine || reviewOptions[0] || engines[0],
      profilesToSave,
      workerProfiles,
      nextEngines[0]
    ) || nextEngines[0];
    // 2) the rest of the roster + budgets + models
    const res = await putWorkerSettings({
      engines: nextEngines,
      // The settings UI does not expose a separate race-only subset. Keep the
      // race roster aligned with the visible enabled seats so a stale hidden
      // subset cannot silently drop an engine from dispatch.
      race_engines: nextEngines,
      start_workers: nextStartWorkers,
      max_workers: nextMaxWorkers,
      worker_backend: workerBackend,
      race_scout: raceScout,
      race_timeout: raceTimeout,
      wall_clock_budget: wallClockBudget,
      max_total_workers: maxTotalWorkers,
      cost_budget_usd: costBudgetUsd,
      stage_policy: {
        prepare: {},
        race: { enabled: raceScout, timeout: raceTimeout, engines: nextEngines },
        coordinator: {
          wall_clock_budget: wallClockBudget,
          review: {
            ...reviewPolicy,
            enabled: reviewEnabled,
            engine: nextReviewEngine,
            timeout: reviewTimeout,
            max_concurrent: reviewMaxConcurrent,
            candidate_spike_threshold: reviewCandidateThreshold,
            allow_review_fallback: reviewFallback,
          },
        },
        budgets: { max_total_workers: maxTotalWorkers, cost_budget_usd: costBudgetUsd },
      },
      llm_profiles: {
        planner: { provider: "deepseek", model: plannerModel, base_url: llmBaseUrl },
        titler: { provider: "deepseek", model: titlerModel, base_url: llmBaseUrl },
      },
      worker_profiles: profilesToSave,
    });
    if (res) {
      setCfg(res);
      setEngines(res.engines ?? nextEngines);
      setStartWorkers(res.start_workers ?? nextStartWorkers);
      setReviewEngine(res.stage_policy?.coordinator?.review?.engine ?? nextReviewEngine);
      setWorkerBackend(res.worker_backend ?? workerBackend);
      setWorkerProfiles(res.worker_profiles ?? profilesToSave);
      // max_workers is derived server-side (Σ dispatched seats' concurrency) —
      // adopt the authoritative value so the read-only field never drifts.
      if (typeof res.max_workers === "number") setMaxWorkers(res.max_workers);
      setStatus("saved");
      setDirty(false);
    } else setStatus("error");
  };

  const refreshAccounts = async () => {
    setAccounts(await listCredentialAccounts());
    setSysLogin(await getSystemLogin());
    // An account write/delete may have changed credential material for a bound
    // account_id — bust the cached deep-auth verdicts (they could now be stale,
    // e.g. a freshly re-uploaded token after a 403) and re-pull binding health.
    setProfileAuth({});
    fetchProfilesHealth().then((hs) =>
      setProfileHealth(Object.fromEntries(hs.map((h) => [h.profile_id, h]))));
  };

  const isDefaultLikeAccountId = (id: string) =>
    id.trim() === "" || /^(claude|codex|cursor)-main$/.test(id.trim());

  const onAccountTypeChange = (next: AccountType) => {
    setAccountType(next);
    const targetEngine = next === "api" ? accountApiEngine : next;
    if (isDefaultLikeAccountId(accountId)) setAccountId(`${targetEngine}-main`);
  };

  const onAccountApiEngineChange = (next: "claude" | "codex" | "cursor") => {
    setAccountApiEngine(next);
    if (isDefaultLikeAccountId(accountId)) setAccountId(`${next}-main`);
  };

  // Pre-fill the account form for a specific engine and bring it into view (used
  // by the per-card "configure account →" deep-link and the Accounts blocker).
  const prefillAccount = (engine: "claude" | "codex" | "cursor") => {
    setTab("accounts");
    setEditingAccount(null);
    setAccountType(engine);
    setAccountApiEngine(engine);
    if (isDefaultLikeAccountId(accountId)) setAccountId(`${engine}-main`);
    setAccountSecret("");
    setAccountBaseUrl("");
    setShowSecret(false);
    setAccountStatus("idle");
    setFormTest(null);
  };

  // Reset the form back to a clean "add new" state (also used to cancel an edit).
  const resetAccountForm = () => {
    setEditingAccount(null);
    setAccountType("claude");
    setAccountApiEngine("claude");
    setAccountId("claude-main");
    setAccountSecret("");
    setAccountBaseUrl("");
    setShowSecret(false);
    setAccountStatus("idle");
    setFormTest(null);
  };

  // Load an existing account row into the form for viewing/editing. The server
  // echoes the stored values (base_url + the secret), so the form is pre-filled
  // and the operator edits in place; the secret stays masked until revealed.
  const editAccount = (a: CredentialAccount) => {
    const isEndpoint = a.mode === "custom_endpoint";
    const target = (a.details?.target_engine as string | null) || null;
    // present account ⇒ a credential exists on disk (token/auth.json/api_key).
    const hasSecret = Boolean(a.present);
    setEditingAccount({
      account_id: a.account_id,
      base_url_set: Boolean(a.details?.base_url),
      has_secret: hasSecret,
      updated_at: a.updated_at,
    });
    setAccountId(a.account_id);
    if (isEndpoint) {
      setAccountType("api");
      // a.engine carries the target_engine for a marked endpoint; fall back to
      // the explicit detail, else default to claude.
      setAccountApiEngine(
        (target || (a.engine !== "api" ? a.engine : "claude")) as "claude" | "codex" | "cursor",
      );
    } else {
      setAccountType(a.engine as AccountType);
      if (a.engine === "claude" || a.engine === "codex" || a.engine === "cursor") {
        setAccountApiEngine(a.engine);
      }
    }
    // Pre-fill the real values so the operator sees and edits them in place. The
    // server echoes the secret (details.secret_value) per the operator's request;
    // it stays masked in a password field until the show/hide toggle reveals it.
    setAccountBaseUrl((a.details?.base_url_value as string | undefined) || "");
    setAccountSecret((a.details?.secret_value as string | undefined) || "");
    setShowSecret(false);
    setAccountStatus("idle");
    setFormTest(null);
    // scroll the form into view so the edit is obvious.
    requestAnimationFrame(() =>
      modalRef.current?.querySelector(".ws2-form")?.scrollIntoView({ behavior: "smooth", block: "nearest" }));
  };

  const saveAccount = async (): Promise<CredentialAccount | null> => {
    // A blank secret is allowed ONLY when editing an existing account — the
    // backend then keeps the stored credential and updates metadata only.
    const secretRequired = !editingAccount;
    if (!accountId.trim() || (secretRequired && !accountSecret.trim())) {
      setAccountStatus("error");
      return null;
    }
    setAccountStatus("saving");
    setFormTest(null);
    const engine = accountType;
    const secret = accountSecret.trim();   // "" on edit = keep existing
    const saved = await putCredentialAccount(
      accountId.trim(),
      engine === "codex"
        ? { engine, codex_auth_json: secret }
        : engine === "api"
          ? { engine, secret, base_url: accountBaseUrl, target_engine: accountApiEngine }
          : { engine, secret }
    );
    if (saved) {
      setAccountStatus("saved");
      // Re-pin the edit snapshot to the freshly-saved row so subsequent saves
      // still count as edits (and the "editing" banner stays accurate). Keep the
      // fields showing the values the server now holds (secret re-masked).
      setEditingAccount({
        account_id: saved.account_id,
        base_url_set: Boolean(saved.details?.base_url),
        has_secret: Boolean(saved.present),
        updated_at: saved.updated_at,
      });
      setAccountBaseUrl((saved.details?.base_url_value as string | undefined) || "");
      setAccountSecret((saved.details?.secret_value as string | undefined) || "");
      setShowSecret(false);
      await refreshAccounts();
      return saved;
    }
    setAccountStatus("error");
    return null;
  };

  const saveAndTestAccount = async () => {
    const saved = await saveAccount();
    if (!saved) return;
    setFormTest({ ok: false, detail: "", testing: true });
    const r = await testCredentialAccount(saved.account_id, saved.engine, workerBackend);
    setFormTest({ ...r, testing: false });
  };

  const removeAccount = async (id: string) => {
    if (await deleteCredentialAccount(id)) await refreshAccounts();
  };

  // "测连通" for a profile — the DEEP auth probe (same verdict as the dispatch
  // precheck). Result caches into profileAuth keyed by profile name; it's the
  // honest "verified" signal the badge upgrades amber→ok on.
  const runProfileTest = async (p: WorkerProfile) => {
    const key = profileName(p);
    setProfileAuth((s) => ({
      ...s,
      [key]: { ...(s[key] as ProfileHealth), profile_id: key, testing: true } as ProfileHealth & { testing?: boolean },
    }));
    const r = await testProfileHealth(p.id);
    if (r) setProfileAuth((s) => ({ ...s, [key]: { ...r, testing: false } }));
    else setProfileAuth((s) => { const next = { ...s }; delete next[key]; return next; });
    // a deep probe also refreshes the binding verdict (e.g. an account bound
    // since the modal opened) — re-pull the cheap batch.
    fetchProfilesHealth().then((hs) =>
      setProfileHealth(Object.fromEntries(hs.map((h) => [h.profile_id, h]))));
  };

  // One-click: re-import a codex account from the host's ~/.codex/auth.json after
  // `codex login`. `codex login` refreshes the host file but container workers
  // mount the account COPY, so a fresh login must be re-imported to take effect.
  const importCodexFromHost = async (accountId: string) => {
    setImportingCodex(accountId);
    const r = await importHostCodexAuth(accountId);
    setImportingCodex(null);
    if (r.ok) {
      await refreshAccounts();   // busts cached auth verdicts + re-pulls binding health
      setStatus("saved");
    } else {
      setStatus("error");
      // surface the server's reason (host file missing / container) inline
      setProfileAuth((s) => ({ ...s }));
      window.alert(r.detail || "import failed");
    }
  };

  const runLlmTest = async () => {
    setLlmTesting(true);
    const r = await testLlmEndpoint("planner", llmBaseUrl, plannerModel);
    setLlmTest({ ok: r.ok, detail: r.detail });
    setLlmTesting(false);
  };

  const runModelTest = async (profile: WorkerProfile) => {
    setModelTest((p) => ({ ...p, [profile.id]: { ok: false, detail: "", testing: true } }));
    const r = await testWorkerProfileModel(profile, profile.model ?? "", workerBackend);
    setModelTest((p) => ({ ...p, [profile.id]: { ok: r.ok, detail: r.detail, testing: false } }));
  };

  const runSelfCheck = async () => {
    setChecking(true);
    setHealth(await getEngineHealth(workerBackend));
    setChecking(false);
  };

  const updateProfile = (id: string, patch: Partial<WorkerProfile>) => {
    setWorkerProfiles((prev) => prev.map((p) => (p.id === id ? { ...p, ...patch } : p)));
    mark();
  };

  // ── worker composer ─────────────────────────────────────────────────────────
  const addInstance = (engine: string) => {
    const runtime = workerBackend === "local" ? localRuntimeId : containerRuntimeForNetwork(networkMode);
    setWorkerProfiles((prev) => {
      const inst = makeInstance(engine, prev, runtime);
      const nm = profileRefName(inst);
      const next = [...prev, inst];
      setEngines((cur) => (cur.includes(nm) ? cur : [...cur, nm]));
      return next;
    });
    mark();
  };

  const removeInstance = (id: string) => {
    setWorkerProfiles((prev) => {
      const gone = prev.find((p) => p.id === id);
      const next = prev.filter((p) => p.id !== id);
      if (gone) {
        const nm = profileRefName(gone);
        const baseStillUsed = next.some((p) => p.engine === gone.engine);
        setEngines((cur) => cur.filter((e) => e !== nm && (baseStillUsed || e !== gone.engine)));
      }
      return next;
    });
    mark();
  };

  const moveInstance = (from: number, to: number) => {
    if (from === to || from < 0 || to < 0) return;
    setWorkerProfiles((prev) => {
      if (from >= prev.length || to >= prev.length) return prev;
      const next = [...prev];
      const [moved] = next.splice(from, 1);
      next.splice(to, 0, moved);
      return next.map((p, i) => ({ ...p, priority: (i + 1) * 10 }));
    });
    mark();
  };

  const toggleInstanceRace = (id: string) => {
    setWorkerProfiles((prev) => prev.map((p) => (p.id === id ? { ...p, race: !p.race } : p)));
    mark();
  };

  // Enable / disable a card WITHOUT deleting it (Codex P0). Enabling also makes
  // sure the profile is in the dispatch roster; disabling drops it from the
  // roster (by name) but keeps the profile + all its tuned fields.
  const toggleInstanceEnabled = (id: string) => {
    setWorkerProfiles((prev) => {
      const target = prev.find((p) => p.id === id);
      if (!target) return prev;
      const nextEnabled = !isEnabled(target);
      const nm = profileRefName(target);
      setEngines((cur) => {
        if (nextEnabled) return cur.includes(nm) ? cur : [...cur, nm];
        const baseStillEnabled = prev.some((p) => p.id !== id && p.engine === target.engine && isEnabled(p));
        return cur.filter((e) => e !== nm && (baseStillEnabled || e !== target.engine));
      });
      return prev.map((p) => (p.id === id ? { ...p, enabled: nextEnabled } : p));
    });
    mark();
  };

  const sysLoginLabel = (s: SystemLoginStatus | undefined) =>
    s === "present" ? t("settings.sysLoginPresent")
      : s === "absent" ? t("settings.sysLoginAbsent")
        : t("settings.sysLoginUnknown");

  const dot = (s: Severity) => <span className={`ws-dot ${s}`} aria-hidden />;

  // Rail order — and the default-open tab — follow the setup dependency chain:
  // you must decide WHERE workers run before you know which credentials are
  // needed, before you can configure a runnable worker. So the panel opens on
  // Runtime (the first step), then accounts → roster → budget → advanced.
  const tabs: { id: Tab; icon: IconName; label: string; sev?: Severity }[] = [
    { id: "runtime", icon: "cpu", label: t("settings.tabRuntime"), sev: "ok" },
    { id: "accounts", icon: "lock", label: t("settings.tabAccounts"), sev: acctSev },
    { id: "roster", icon: "layers", label: t("settings.tabRoster"), sev: rosterSev },
    { id: "budget", icon: "target", label: t("settings.tabBudget"), sev: budgetSev },
    { id: "advanced", icon: "gear", label: t("settings.tabAdvanced") },
  ];

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal worker-settings ws2" ref={modalRef} onClick={(e) => e.stopPropagation()} role="dialog" aria-modal="true" aria-label={t("settings.title")}>
        <div className="modal-head">
          <div>
            <span>{t("settings.title")}</span>
            <p>{t("settings.globalScope")}</p>
          </div>
          <button className="modal-x" onClick={onClose} title={t("settings.close")} aria-label={t("settings.close")}><Icon name="x" size={15} /></button>
        </div>

        {/* config-health bar: draft/active badge + clickable severity segments
            that deep-link to the failing control. Reflects the DRAFT buffer. */}
        <div className="ws2-health" aria-label={t("settings.healthLabel")}>
          <span className={`ws2-draft ${dirty ? "dirty" : "clean"}`}>
            <Icon name={dirty ? "pencil" : "check"} size={11} />
            {dirty ? t("settings.healthDraft") : t("settings.healthActive")}
          </span>
          <span className="ws2-health-sep" aria-hidden>|</span>
          <button type="button" className="ws2-seg" onClick={() => setTab("roster")}>
            {dot(rosterSev)}
            {rosterSev === "ok" ? t("settings.healthRosterOk")
              : enabledProfiles.length === 0 ? t("settings.healthRosterEmpty")
                : t("settings.healthRosterBlocked", { n: blockedProfiles.length })}
          </button>
          <button type="button" className="ws2-seg" onClick={() => setTab("accounts")}>
            {dot(acctSev)}
            {acctSev === "ok" ? t("settings.healthAcctOk")
              : acctSev === "red" ? t("settings.healthAcctMissing", {
                  profiles: enabledProfiles
                    .filter((p) => { const s = profileAcctState(p); return s === "unbound" || s === "auth_failed"; })
                    .map(profileName).join(", "),
                })
                : t("settings.healthAcctUnverified", {
                  n: acctStates.filter((s) => s === "unverified").length,
                })}
          </button>
          <button type="button" className="ws2-seg" onClick={() => setTab("runtime")}>
            {dot("ok")}
            {workerBackend === "container"
              ? t("settings.healthRuntimeContainer", { net: networkLabel(networkMode) })
              : t("settings.healthRuntimeLocal")}
          </button>
          <button type="button" className="ws2-seg" onClick={() => setTab("budget")}>
            {dot(budgetSev)}
            {budgetSev === "amber" ? t("settings.healthBudgetInf") : t("settings.healthBudgetCap")}
          </button>
        </div>

        <div className="ws2-body">
          <nav className="ws2-rail" aria-label={t("settings.title")}>
            {tabs.map((tb) => (
              <button
                key={tb.id}
                type="button"
                className={`ws2-tab ${tab === tb.id ? "on" : ""}`}
                onClick={() => setTab(tb.id)}
                aria-current={tab === tb.id}
              >
                <Icon name={tb.icon} size={16} />
                <span>{tb.label}</span>
                {/* tab dot flags REAL blockers only (red); amber "runnable but
                    degraded" signals (e.g. unlimited budget) stay in the top
                    health bar so the rail isn't noisy with non-blocking dots. */}
                {tb.sev === "red" && <span className="ws2-tdot red" aria-hidden />}
              </button>
            ))}
          </nav>

          <div className="ws2-content">
            {/* ── ROSTER ─────────────────────────────────────────────────── */}
            {tab === "roster" && (
              <section>
                <div className="ws-section-head">
                  <h3>{t("settings.tabRoster")}</h3>
                  <span>{t("settings.rosterHint")}</span>
                </div>
                {workerProfiles.length === 0 ? (
                  <div className="ws2-empty">
                    <h4>{t("settings.emptyTitle")}</h4>
                    <ol className="ws2-steps">
                      <li className="done">
                        <span className="ws2-stepn done"><Icon name="check" size={11} /></span>
                        <span>{t("settings.emptyStep1Done", { env: backendLabel })}
                          <button type="button" className="ws-jump" onClick={() => setTab("runtime")}>{t("settings.emptyGoRuntime")}</button>
                        </span>
                      </li>
                      <li className={workerBackend === "container" ? "now" : ""}>
                        <span className={`ws2-stepn ${workerBackend === "container" ? "now" : "done"}`}>{workerBackend === "container" ? "2" : <Icon name="check" size={11} />}</span>
                        <span>{workerBackend === "container" ? t("settings.emptyStep2") : t("settings.emptyStep2Local")}
                          {workerBackend === "container" && (
                            <button type="button" className="ws-jump" onClick={() => setTab("accounts")}>{t("settings.emptyGoAccounts")}</button>
                          )}
                        </span>
                      </li>
                      <li className={workerBackend === "container" ? "" : "now"}>
                        <span className={`ws2-stepn ${workerBackend === "container" ? "" : "now"}`}>3</span>
                        <span>{t("settings.emptyStep3")}</span>
                      </li>
                    </ol>
                  </div>
                ) : (
                  <p className="ws2-sub">{t("settings.rosterSub")}</p>
                )}

                <ol className="ws2-roster">
                  {workerProfiles.map((p, i) => {
                    const opts = modelOptions.models[p.engine] ?? [];
                    const modelValue = p.model ?? "";
                    const known = opts.some((o) => o.id === modelValue);
                    const customOpen = !!customModelOpen[p.id] || (modelValue !== "" && !known);
                    const sel = customOpen ? "__custom__" : modelValue === "" ? "" : known ? modelValue : "__custom__";
                    const minfo = modelTest[p.id];
                    const dragging = dragIdx === i;
                    const dropTarget = dragOverIdx === i && dragIdx !== null && dragIdx !== i;
                    const en = isEnabled(p);
                    const blocker = profileBlocker(p);
                    const open = !!expanded[p.id];
                    return (
                      <li
                        key={p.id}
                        className={`ws2-card${dragging ? " dragging" : ""}${dropTarget ? " drop-target" : ""}${en ? "" : " disabled"}${blocker ? " blocked" : ""}`}
                        draggable
                        onDragStart={(ev) => { setDragIdx(i); ev.dataTransfer.effectAllowed = "move"; }}
                        onDragOver={(ev) => { ev.preventDefault(); setDragOverIdx(i); ev.dataTransfer.dropEffect = "move"; }}
                        onDragEnd={() => { setDragIdx(null); setDragOverIdx(null); }}
                        onDrop={(ev) => {
                          ev.preventDefault();
                          if (dragIdx !== null) moveInstance(dragIdx, i);
                          setDragIdx(null); setDragOverIdx(null);
                        }}
                      >
                        <div className="ws2-card-row">
                          <span className="ws-card-grip" aria-hidden title={t("settings.composerDrag")}>⋮⋮</span>
                          <span className={`ws-card-engine eng-${p.engine}`}>{engineLabel(p.engine)}</span>
                          <input
                            className="ws-card-name"
                            value={p.name || p.id}
                            spellCheck={false}
                            aria-label={t("settings.composerName")}
                            onChange={(ev) => updateProfile(p.id, { name: ev.target.value })}
                          />
                          <div className="ws-card-model">
                            <select value={sel} onChange={(ev) => {
                              const v = ev.target.value;
                              if (v === "__custom__") {
                                setCustomModelOpen((cur) => ({ ...cur, [p.id]: true }));
                                return;
                              }
                              setCustomModelOpen((cur) => {
                                if (!cur[p.id]) return cur;
                                const next = { ...cur };
                                delete next[p.id];
                                return next;
                              });
                              updateProfile(p.id, { model: v });
                            }}>
                              <option value="">{t("settings.modelDefault")}</option>
                              {opts.map((o) => <option value={o.id} key={o.id}>{o.label}</option>)}
                              <option value="__custom__">{t("settings.customModel")}</option>
                            </select>
                            {customOpen && (
                              <input
                                className="ws-card-model-custom"
                                value={modelValue}
                                placeholder={t("settings.customModel")}
                                spellCheck={false}
                                onChange={(ev) => updateProfile(p.id, { model: ev.target.value })}
                              />
                            )}
                          </div>
                          <span className={`ws2-ready ${blocker ? "r" : en ? "g" : "a"}`}>
                            <Icon name={blocker ? "alert" : en ? "check" : "dot"} size={12} />
                            {blocker ? t("settings.readyBlocked") : en ? t("settings.readyOk") : t("settings.readyDisabled")}
                          </span>
                          <button
                            type="button"
                            className={`ws2-toggle ${en ? "on" : ""}`}
                            onClick={() => toggleInstanceEnabled(p.id)}
                            role="switch"
                            aria-checked={en}
                            aria-label={en ? t("settings.cardDisabled") : t("settings.cardEnabled")}
                            title={t("settings.cardEnableHint")}
                          ><span className="ws2-knob" /></button>
                          <button
                            type="button"
                            className={`ws2-caret ${open ? "open" : ""}`}
                            onClick={() => setExpanded((e) => ({ ...e, [p.id]: !e[p.id] }))}
                            aria-expanded={open}
                            aria-label={t("settings.cardExpand")}
                          ><Icon name="chevronRight" size={14} /></button>
                        </div>

                        {blocker && (
                          <div className="ws2-blocker">
                            <Icon name="alert" size={12} />
                            <span>{t("settings.blockerAccount", { engine: blocker })}</span>
                            <button type="button" className="ws-jump" onClick={() => prefillAccount(blocker as "claude" | "codex" | "cursor")}>
                              {t("settings.blockerGoAccount")}
                            </button>
                          </div>
                        )}

                        {open && (
                          <div className="ws2-expand">
                            <label className="ws2-xf">
                              <span>{t("settings.composerRace")}</span>
                              <button type="button" className={`ws2-toggle sm ${p.race ? "on" : ""}`}
                                onClick={() => toggleInstanceRace(p.id)} role="switch" aria-checked={!!p.race}>
                                <span className="ws2-knob" />
                              </button>
                            </label>
                            <label className="ws2-xf">
                              <span>{t("settings.composerMaxRunning")}</span>
                              <input type="number" min={1} value={p.max_running ?? 1}
                                onChange={(ev) => updateProfile(p.id, { max_running: Math.max(1, Number(ev.target.value) || 1) })} />
                            </label>
                            <label className="ws2-xf">
                              <span>{t("settings.profileMaxReviewRunning")}</span>
                              <input type="number" min={0} value={p.max_review_running ?? 0}
                                onChange={(ev) => updateProfile(p.id, { max_review_running: Math.max(0, Number(ev.target.value) || 0) })} />
                            </label>
                            <label className="ws2-xf">
                              <span>{t("settings.profilePriority")}</span>
                              <input type="number" min={0} value={p.priority ?? (i + 1) * 10} readOnly title={t("settings.composerPriority")} />
                            </label>
                            <div className="ws2-xf-actions">
                              <span className="ws-card-test">
                                {minfo && !minfo.testing && (
                                  minfo.ok
                                    ? <span className="ws-ok"><Icon name="check" size={12} /></span>
                                    : <span className="ws-bad" title={minfo.detail}><Icon name="x" size={12} /></span>
                                )}
                                <button className="ws-mini-btn" type="button" disabled={minfo?.testing}
                                  onClick={() => runModelTest(p)} title={minfo?.detail || t("settings.testModel")}>
                                  <Icon name="plug" size={12} /> {minfo?.testing ? t("settings.testing") : t("settings.testModel")}
                                </button>
                              </span>
                              <button type="button" className="ws-mini-btn danger" onClick={() => removeInstance(p.id)}>
                                <Icon name="x" size={12} /> {t("settings.composerRemove")}
                              </button>
                            </div>
                          </div>
                        )}
                      </li>
                    );
                  })}
                </ol>

                <div className="ws-add-row">
                  {BASE_ENGINES.map((e) => (
                    <button key={e} type="button" className="ws-add-btn"
                      onClick={() => addInstance(e)} title={t("settings.composerAddTitle")}>
                      <span className="ws-add-plus" aria-hidden>+</span> {e}
                    </button>
                  ))}
                </div>
              </section>
            )}

            {/* ── ACCOUNTS ───────────────────────────────────────────────── */}
            {tab === "accounts" && (
              <section>
                <div className="ws-section-head">
                  <h3>{t("settings.tabAccounts")} <span className="ws-tag">{backendLabel}</span></h3>
                  <span>{t("settings.testsAgainst")} {backendLabel}</span>
                </div>
                <p className="ws2-sub">{workerBackend === "container" ? t("settings.acctSub") : t("settings.credLocalNote")}</p>

                {/* One row PER ENABLED PROFILE (not per engine): readiness keys
                    off the profile's OWN bound account, so a profile with an empty
                    binding shows red even when another account of the same engine
                    exists — the original false-green, structurally gone. */}
                <div className="ws2-acct-list">
                  {enabledProfiles.length === 0 && (
                    <p className="ws2-sub">{t("settings.acctNoProfiles")}</p>
                  )}
                  {enabledProfiles.map((p) => {
                    const key = profileName(p);
                    const state = profileAcctState(p);
                    const auth = profileAuth[key];
                    // SINGLE SOURCE OF TRUTH (plan §3.4): the binding label comes
                    // from the backend health verdict (binding_kind), NOT the literal
                    // credential_account field — that split caused the "未绑定 vs 已绑定"
                    // same-row contradiction. inherited → "自动: <id>", not "未绑定".
                    const binding = profileHealth[key] ?? profileHealth[p.id];
                    const bindKind = binding?.binding_kind ?? (p.credential_account ? "explicit" : "inherited");
                    const effCred = binding?.effective_credential_id || String(p.credential_account || "");
                    const acct = accounts.find((a) => a.account_id === effCred);
                    return (
                      <div className={`ws2-acct ${bindKind === "missing" || state === "auth_failed" ? "miss" : ""}`} key={p.id}>
                        <span className={`ws-card-engine eng-${p.engine}`}>{p.engine}</span>
                        <span className="ws2-acct-id">
                          <strong>{key}</strong>
                          {" · "}
                          {bindKind === "missing"
                            ? <em className="ws-bad">{t("settings.acctUnbound")}</em>
                            : bindKind === "inherited"
                              ? (effCred
                                  ? <em className="ws-muted" title={t("settings.bindingInheritedHint")}>{t("settings.bindingAuto", { id: effCred })}</em>
                                  : <em className="ws-muted" title={t("settings.bindingHostHint")}>{t("settings.bindingHost")}</em>)
                              : <code>{effCred}</code>}
                          {acct?.mode === "custom_endpoint" && <em> · {t("settings.modeCustomEndpoint")}</em>}
                        </span>
                        <span className="ws2-acct-state">
                          {bindKind === "missing"
                            ? <span className="ws-bad"><Icon name="x" size={12} /> {t("settings.acctUnbound")}</span>
                            : state === "auth_failed"
                              ? <span className="ws-bad" title={auth?.detail}><Icon name="x" size={12} /> {t("settings.profileAuthFailed", { layer: auth?.layer || "auth" })}</span>
                              : state === "verified"
                                ? <span className="ws-ok"><Icon name="check" size={12} /> {t("settings.profileVerified")}</span>
                                : <span className="ws-muted" title={t("settings.profileUnverifiedHint")}>{t("settings.profileUnverified")}</span>}
                          {bindKind !== "missing" ? (
                            <button className="ws-mini-btn" type="button" disabled={auth?.testing}
                              onClick={() => runProfileTest(p)} title={auth?.detail || t("settings.testConn")}>
                              <Icon name="plug" size={12} /> {auth?.testing ? t("settings.testing") : t("settings.testConn")}
                            </button>
                          ) : (
                            <button className="ws-save sm" type="button" onClick={() => prefillAccount(p.engine as "claude" | "codex" | "cursor")}>
                              <Icon name="plug" size={12} /> {t("settings.acctAdd")}
                            </button>
                          )}
                          {/* codex re-auth: after `codex login`, one click re-imports the
                              fresh host ~/.codex/auth.json into the bound account (the host
                              file and the mounted account copy drift otherwise). */}
                          {bindKind !== "missing" && effCred && p.engine === "codex" && (
                            <button className="ws-mini-btn" type="button"
                              disabled={importingCodex === effCred}
                              onClick={() => importCodexFromHost(effCred)}
                              title={t("settings.importHostCodexHint")}>
                              <Icon name="refresh" size={12} /> {importingCodex === effCred ? t("settings.importing") : t("settings.importHostCodex")}
                            </button>
                          )}
                        </span>
                      </div>
                    );
                  })}
                </div>

                {accounts.length > 0 && (
                  <div className="ws-account-list">
                    {accounts.map((a) => (
                      <div
                        className={`ws-account-row clickable${editingAccount?.account_id === a.account_id ? " editing" : ""}`}
                        key={a.account_id}
                        role="button"
                        tabIndex={0}
                        title={t("settings.editAccountHint")}
                        onClick={() => editAccount(a)}
                        onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); editAccount(a); } }}
                      >
                        <code>{a.account_id}</code>
                        <span>{a.engine}</span>
                        <span>{a.mode === "custom_endpoint" ? t("settings.modeCustomEndpoint") : a.mode}</span>
                        {a.details?.base_url ? (
                          <span className="ws-account-tag">{t("settings.baseUrlSet")}</span>
                        ) : <span />}
                        <Icon name="pencil" size={13} className="ws-account-edit-hint" />
                        <button
                          className="modal-x"
                          type="button"
                          onClick={(e) => { e.stopPropagation(); removeAccount(a.account_id); }}
                          aria-label={t("settings.deleteAccount")}
                        >
                          <Icon name="x" size={13} />
                        </button>
                      </div>
                    ))}
                  </div>
                )}

                <div className={`ws2-form${editingAccount ? " editing" : ""}`}>
                  <div className="ws-section-head">
                    <h3>{editingAccount ? t("settings.acctEditTitle") : t("settings.acctFormTitle")}</h3>
                    {editingAccount && (
                      <button type="button" className="ws-link-btn" onClick={resetAccountForm}>
                        {t("settings.cancelEdit")}
                      </button>
                    )}
                  </div>
                  {editingAccount && (
                    <div className="ws-note ws-note-info">
                      {t("settings.editingAccountNote").replace("{id}", editingAccount.account_id)}
                      {editingAccount.updated_at
                        ? ` · ${t("settings.lastUpdated")} ${new Date(editingAccount.updated_at * 1000).toLocaleString()}`
                        : ""}
                    </div>
                  )}
                  <div className="ws-grid">
                    <div className="ws-field">
                      <label>{t("settings.accountId")}</label>
                      <input
                        value={accountId}
                        onChange={(e) => setAccountId(e.target.value)}
                        disabled={!!editingAccount}
                        title={editingAccount ? t("settings.accountIdLocked") : undefined}
                      />
                    </div>
                    <div className="ws-field">
                      <label>{t("settings.accountType")}</label>
                      <select value={accountType} onChange={(e) => onAccountTypeChange(e.target.value as AccountType)}>
                        <option value="claude">{t("settings.typeClaudeToken")}</option>
                        <option value="codex">{t("settings.typeCodexAuth")}</option>
                        <option value="cursor">{t("settings.typeCursorKey")}</option>
                        <option value="api">{t("settings.typeCustomEndpoint")}</option>
                      </select>
                    </div>
                    {accountType === "api" && (
                      <>
                        <div className="ws-field">
                          <label>{t("settings.accountTargetEngine")}</label>
                          <select value={accountApiEngine}
                            onChange={(e) => onAccountApiEngineChange(e.target.value as "claude" | "codex" | "cursor")}>
                            <option value="claude">claude</option>
                            <option value="codex">codex</option>
                            <option value="cursor">cursor</option>
                          </select>
                        </div>
                        {accountApiEngine === "codex" && (
                          <p className="ws-field ws-span-all ws-warn-note">
                            <Icon name="x" size={12} /> {t("settings.endpointCodexWarn")}
                          </p>
                        )}
                        {accountApiEngine === "cursor" && (
                          <p className="ws-field ws-span-all ws-warn-note">
                            <Icon name="x" size={12} /> {t("settings.endpointCursorWarn")}
                          </p>
                        )}
                        {accountApiEngine === "claude" && (
                          <p className="ws-field ws-span-all ws-muted-note">
                            {t("settings.endpointClaudeHint")}
                          </p>
                        )}
                        <div className="ws-field">
                          <label>{t("settings.baseUrl")}</label>
                          <input
                            value={accountBaseUrl}
                            onChange={(e) => setAccountBaseUrl(e.target.value)}
                            placeholder="https://api.example.com/v1"
                          />
                        </div>
                      </>
                    )}
                    <div className="ws-field ws-span-all">
                      <label>
                        {accountType === "codex" ? t("settings.codexAuthJson") : t("settings.secret")}
                        {editingAccount && <span className="ws-label-hint"> · {t("settings.secretEditHint")}</span>}
                      </label>
                      {/* The stored secret is echoed into this field (operator opted
                          into edit-in-place). It's masked by default; the eye toggle
                          reveals plaintext. Reset to masked whenever the form reloads. */}
                      <div className="ws-secret-wrap">
                        {accountType === "codex" ? (
                          <textarea
                            className={showSecret ? "" : "ws-secret-masked"}
                            value={accountSecret}
                            onChange={(e) => setAccountSecret(e.target.value)}
                            rows={3} spellCheck={false}
                            placeholder={editingAccount ? t("settings.keepCurrentSecret") : '{"OPENAI_API_KEY":"..."}'}
                          />
                        ) : (
                          <input
                            type={showSecret ? "text" : "password"}
                            value={accountSecret}
                            onChange={(e) => setAccountSecret(e.target.value)}
                            spellCheck={false}
                            placeholder={editingAccount ? t("settings.keepCurrentSecret") : undefined}
                          />
                        )}
                        {accountSecret && (
                          <button
                            type="button"
                            className="ws-secret-eye"
                            onClick={() => setShowSecret((s) => !s)}
                            aria-label={showSecret ? t("settings.hideSecret") : t("settings.showSecret")}
                            title={showSecret ? t("settings.hideSecret") : t("settings.showSecret")}
                          >
                            <Icon name={showSecret ? "eyeOff" : "eye"} size={15} />
                          </button>
                        )}
                      </div>
                    </div>
                  </div>
                  {accountType === "api" && (
                    <div className="ws-note ws-note-info">
                      {t("settings.accountTargetEngineHint").replace("{id}", `${accountApiEngine}-main`)}
                    </div>
                  )}
                  <div className="ws-foot">
                    <span className={`ws-status ${accountStatus}`}>
                      {accountStatus === "saved" ? t("settings.saved")
                        : accountStatus === "error" ? t("settings.invalid")
                          : accountStatus === "saving" ? "..." : ""}
                    </span>
                    {formTest && (
                      <span className={formTest.testing ? "ws-muted" : formTest.ok ? "ws-ok" : "ws-bad"} title={formTest.detail}>
                        {formTest.testing ? t("settings.testing")
                          : formTest.ok ? <><Icon name="check" size={13} /> {t("settings.ok")}</>
                            : <><Icon name="x" size={13} /> {formTest.layer ? `${formTest.layer}: ` : ""}{formTest.detail.slice(0, 48)}</>}
                      </span>
                    )}
                    <button className="ws-mini-btn" type="button" onClick={saveAndTestAccount} disabled={accountStatus === "saving" || formTest?.testing}>
                      <Icon name="plug" size={13} /> {t("settings.saveAndTest")}
                    </button>
                    <button className="ws-save" onClick={() => { void saveAccount(); }} disabled={accountStatus === "saving"}>{editingAccount ? t("settings.saveChanges") : t("settings.saveAccount")}</button>
                  </div>
                </div>
              </section>
            )}

            {/* ── RUNTIME ────────────────────────────────────────────────── */}
            {tab === "runtime" && (
              <section>
                <div className="ws-section-head">
                  <h3>{t("settings.tabRuntime")} <span className="ws-tag">{t("settings.secRuntimeTag")}</span></h3>
                  <span>{t("settings.secRuntimeHint")}</span>
                </div>
                <p className="ws2-sub">{t("settings.runtimeSub")}</p>
                <div className="ws-grid">
                  <div className="ws-field">
                    <label>{t("settings.runWhere")}</label>
                    <select value={workerBackend} onChange={(e) => { setWorkerBackend(e.target.value as Backend); mark(); }}>
                      <option value="container">{t("settings.runtimeContainer")}</option>
                      <option value="local">{t("settings.runtimeLocal")}</option>
                    </select>
                  </div>
                  <div className="ws-field">
                    {/* 坑 C: not an image picker — selects the container's NETWORK
                        mode, mapped to the matching container runtime preset. */}
                    <label>{t("settings.network")}</label>
                    <select value={networkMode} disabled={workerBackend === "local"}
                      onChange={(e) => { setNetworkMode(e.target.value as NetworkMode); mark(); }}>
                      {workerBackend === "local"
                        ? <option>{t("settings.recipeLocalNA")}</option>
                        : (<>
                            <option value="bridge">{t("settings.netBridge")}</option>
                            <option value="host">{t("settings.netHost")}</option>
                            <option value="none">{t("settings.netNone")}</option>
                          </>)}
                    </select>
                  </div>
                </div>
                {/* Local mode doesn't use the worker image at all — show why
                    instead of an alarming red health block. Container mode gets
                    the full image-health row + the env var that names it. */}
                {workerBackend === "local" ? (
                  <>
                    <p className="ws2-sub"><Icon name="help" size={12} /> {t("settings.netLocalNote")}</p>
                    <div className="ws-note ws-note-info" style={{ marginTop: 4, marginBottom: 0 }}>
                      {t("settings.imgLocalSkip")}
                    </div>
                  </>
                ) : (
                  <>
                    <div className="ws-image" style={{ marginTop: 6 }}>
                      <div className={`ws-image-dot ${imageStatus?.overall ?? "unknown"}`} aria-hidden />
                      <div className="ws-image-body">
                        <code className="ws-image-name">{imageStatus?.image ?? "…"}</code>
                        <div className="ws-image-checks">
                          <span className={imageStatus?.daemon.ok ? "ok" : "bad"}>
                            {t("settings.imgDaemon")}: {imageLoading ? "…" : imageStatus?.daemon.ok ? "✓" : "✗"}
                          </span>
                          <span className={imageStatus?.pulled.ok ? "ok" : "bad"}>
                            {t("settings.imgPulled")}: {imageLoading ? "…" : imageStatus?.pulled.ok ? "✓" : "✗"}
                          </span>
                          <span className={
                            imageStatus?.version.status === "match" ? "ok"
                            : imageStatus?.version.status === "mismatch" ? "bad" : "muted"}>
                            {t("settings.imgVersion")}: {imageLoading ? "…" : imageStatus?.version.actual ?? "—"}
                            {imageStatus?.version.status === "mismatch" && imageStatus?.version.expected
                              ? ` (≠ ${imageStatus.version.expected})` : ""}
                          </span>
                        </div>
                      </div>
                      <div className="ws-image-actions">
                        <button type="button" className="ws-btn" onClick={refreshImage} disabled={imageLoading || pulling}>
                          {t("settings.imgRefresh")}
                        </button>
                        <button type="button" className="ws-btn primary" onClick={doPullImage} disabled={pulling || !imageStatus?.daemon.ok}>
                          {pulling ? t("settings.imgPulling") : t("settings.imgPull")}
                        </button>
                      </div>
                    </div>
                    <p className="ws2-sub" style={{ marginTop: 8, marginBottom: 0 }}>
                      <Icon name="terminal" size={12} />
                      <code className="ws2-env">{WORKER_IMAGE_ENV}</code>
                      {t("settings.imgEnvNote", { default: WORKER_IMAGE_DEFAULT })}
                    </p>
                  </>
                )}
              </section>
            )}

            {/* ── BUDGET ─────────────────────────────────────────────────── */}
            {tab === "budget" && (
              <section>
                <div className="ws-section-head"><h3>{t("settings.tabBudget")}</h3><span>{t("settings.secSchedule")}</span></div>
                <p className="ws2-sub">{t("settings.budgetSub")}</p>
                <div className="ws-grid">
                  <div className="ws-field">
                    <label>{t("settings.startWorkers")}</label>
                    <input type="number" min={1} max={maxWorkers} value={startWorkers}
                      onChange={(e) => { setStartWorkers(Math.max(1, Math.min(maxWorkers, parseInt(e.target.value) || 1))); mark(); }} />
                  </div>
                  <div className="ws-field">
                    <label>{t("settings.maxWorkers")}</label>
                    {/* Read-only: max_workers is derived = Σ dispatched seats'
                        concurrency. Edit per-seat capacity in the roster instead. */}
                    <input type="number" value={maxWorkers} readOnly disabled
                      className="ws2-derived" title={t("settings.maxWorkersDerived")} />
                    <span className="ws-field-hint">{t("settings.maxWorkersDerived")}</span>
                  </div>
                  <div className="ws-field">
                    <label>{t("settings.raceScout")}</label>
                    <select value={raceScout ? "1" : "0"} onChange={(e) => { setRaceScout(e.target.value === "1"); mark(); }}>
                      <option value="1">{t("settings.enabled")}</option>
                      <option value="0">{t("settings.disabled")}</option>
                    </select>
                  </div>
                  <div className="ws-field">
                    <label>{t("settings.raceTimeout")}</label>
                    <input type="number" min={1} value={raceTimeout}
                      onChange={(e) => { setRaceTimeout(Math.max(1, parseInt(e.target.value) || 720)); mark(); }} />
                  </div>
                  <div className="ws-field">
                    <label>{t("settings.wallClockBudget")}</label>
                    <input type="number" min={0} value={wallClockBudget}
                      onChange={(e) => { setWallClockBudget(Math.max(0, parseInt(e.target.value) || 0)); mark(); }} />
                  </div>
                  <div className="ws-field">
                    <label>{t("settings.maxTotalWorkers")}</label>
                    <input type="number" min={0} value={maxTotalWorkers}
                      onChange={(e) => { setMaxTotalWorkers(Math.max(0, parseInt(e.target.value) || 0)); mark(); }} />
                  </div>
                  <div className="ws-field">
                    <label>{t("settings.costBudgetUsd")}</label>
                    <input type="number" min={0} step="0.01" value={costBudgetUsd}
                      onChange={(e) => { setCostBudgetUsd(Math.max(0, parseFloat(e.target.value) || 0)); mark(); }} />
                  </div>
                </div>

                <div className="ws2-review">
                  <div className="ws2-review-head">
                    <button type="button" className={`ws2-toggle ${reviewEnabled ? "on" : ""}`}
                      onClick={() => { setReviewEnabled(!reviewEnabled); mark(); }} role="switch" aria-checked={reviewEnabled}>
                      <span className="ws2-knob" />
                    </button>
                    <b>{t("settings.reviewToggle")}</b>
                    {!reviewEnabled && <span className="ws2-pill">{t("settings.reviewRevealHint")}</span>}
                  </div>
                  <div className="ws-note ws-note-info ws2-review-note">
                    {t("settings.reviewBtwNote")}
                  </div>
                  {reviewEnabled && (
                    <div className="ws-grid ws2-review-sub">
                      <div className="ws-field">
                        <label>{t("settings.reviewEngine")}</label>
                        <select value={reviewEngine} disabled={reviewOptions.length === 0}
                          onChange={(e) => { setReviewEngine(e.target.value); mark(); }}>
                          {reviewOptions.map((e) => <option value={e} key={e}>{engineLabel(e)}</option>)}
                        </select>
                      </div>
                      <div className="ws-field">
                        <label>{t("settings.reviewTimeout")}</label>
                        <input type="number" min={60} value={reviewTimeout}
                          onChange={(e) => { setReviewTimeout(Math.max(60, parseInt(e.target.value) || 420)); mark(); }} />
                      </div>
                      <div className="ws-field">
                        <label>{t("settings.reviewMaxConcurrent")}</label>
                        <input type="number" min={1} value={reviewMaxConcurrent}
                          onChange={(e) => { setReviewMaxConcurrent(Math.max(1, parseInt(e.target.value) || 1)); mark(); }} />
                      </div>
                      <div className="ws-field">
                        <label>{t("settings.reviewCandidateThreshold")}</label>
                        <input type="number" min={1} value={reviewCandidateThreshold}
                          onChange={(e) => { setReviewCandidateThreshold(Math.max(1, parseInt(e.target.value) || 5)); mark(); }} />
                      </div>
                      <div className="ws-field">
                        <label>{t("settings.reviewFallback")}</label>
                        <select value={reviewFallback ? "1" : "0"} onChange={(e) => { setReviewFallback(e.target.value === "1"); mark(); }}>
                          <option value="0">{t("settings.disabled")}</option>
                          <option value="1">{t("settings.enabled")}</option>
                        </select>
                      </div>
                    </div>
                  )}
                </div>
              </section>
            )}

            {/* ── ADVANCED ───────────────────────────────────────────────── */}
            {tab === "advanced" && (
              <section>
                <div className="ws-section-head">
                  <h3>{t("settings.advReasoning")}</h3>
                  <span>{t("settings.reasonHint")}</span>
                </div>
                <div className="ws-note ws-note-info">{t("settings.reasonKeyNote")}</div>
                <div className="ws-grid">
                  <div className="ws-field ws-span-all">
                    <label>{t("settings.baseUrlEmptyDeepseek")}</label>
                    <input value={llmBaseUrl} placeholder="https://api.deepseek.com/v1" onChange={(e) => { setLlmBaseUrl(e.target.value); mark(); }} />
                  </div>
                  <div className="ws-field">
                    <label>{t("settings.plannerModel")}</label>
                    <input value={plannerModel} onChange={(e) => { setPlannerModel(e.target.value); mark(); }} />
                  </div>
                  <div className="ws-field">
                    <label>{t("settings.titlerModel")}</label>
                    <input value={titlerModel} onChange={(e) => { setTitlerModel(e.target.value); mark(); }} />
                  </div>
                </div>
                <div className="ws-foot" style={{ justifyContent: "flex-start" }}>
                  <button className="ws-mini-btn" type="button" onClick={runLlmTest} disabled={llmTesting}>
                    <Icon name="plug" size={13} /> {llmTesting ? t("settings.testing") : t("settings.testConn")}
                  </button>
                  {llmTest && (
                    <span className={llmTest.ok ? "ws-ok" : "ws-bad"} title={llmTest.detail}>
                      {llmTest.ok ? <><Icon name="check" size={13} /> {t("settings.ok")}</> : <><Icon name="x" size={13} /> {llmTest.detail.slice(0, 60)}</>}
                    </span>
                  )}
                </div>

                <div className="ws-section-head" style={{ marginTop: 22 }}>
                  <h3>{t("settings.advDiagnostics")} <span className="ws-tag">{backendLabel}</span></h3>
                  <span>{workerBackend === "container" ? t("settings.selfcheckContainerNote") : t("settings.selfcheckHostNote")}</span>
                </div>
                <div className="ws-image" style={{ marginTop: 4 }}>
                  <Icon name="refresh" size={15} />
                  <div className="ws-image-body">
                    <span className="ws-muted">{health ? "" : t("settings.diagNeverRun")}</span>
                  </div>
                  <div className="ws-image-actions">
                    <button className="ws-btn primary" onClick={runSelfCheck} disabled={checking}>
                      {checking ? t("settings.checking") : t("settings.diagRun")}
                    </button>
                  </div>
                </div>
                {health && (
                  <div className="ws-sc-list">
                    {health.map((h) => {
                      const isLocal = (h.backend ?? workerBackend) === "local";
                      const unpinned = isLocal && h.bin_source && h.bin_source !== "env";
                      const envVar = h.bin_env || `MUTEKI_${h.engine.toUpperCase()}_BIN`;
                      return (
                        <div key={h.engine} className={`ws-sc-row ${h.healthy ? "ok" : "bad"}`}>
                          <div className="ws-sc-top">
                            <span className="ws-sc-dot" />
                            <span className="ws-sc-name">{h.engine}</span>
                            <span className="ws-sc-ver">{h.version || "-"}</span>
                            <span className="ws-sc-detail">{h.healthy ? t("settings.ok") : (h.detail || t("settings.bad"))}</span>
                          </div>
                          {h.bin && (
                            <div className="ws-sc-bin">
                              <code className="ws-sc-binpath" title={h.bin}>{h.bin}</code>
                              {h.bin_source === "env" && <span className="ws-sc-pin" title={t("settings.binPinned")}>📌</span>}
                              {unpinned && (
                                <span className="ws-sc-binwarn" title={t("settings.binUnpinnedHint").replace("{env}", envVar)}>
                                  ⚠ {t("settings.binUnpinned").replace("{env}", envVar)}
                                </span>
                              )}
                            </div>
                          )}
                        </div>
                      );
                    })}
                  </div>
                )}
              </section>
            )}
          </div>
        </div>

        <div className="ws-savebar ws2-savebar">
          <span className="ws2-foot-note">
            {status === "saved" && !dirty ? <><Icon name="check" size={13} /> {t("settings.footSaved")}</>
              : acctSev === "red" || blockedProfiles.length > 0
                ? <><Icon name="alert" size={13} /> {t("settings.footError")}</>
                : dirty
                  ? <><Icon name="help" size={13} /> {t("settings.footDirty")}</>
                  : <><Icon name="check" size={13} /> {t("settings.footSaved")}</>}
          </span>
          <span className="ws2-foot-actions">
            <button className="ws-btn" onClick={onClose}>{t("settings.discard")}</button>
            <button className="ws-save" onClick={save} disabled={status === "saving" || engines.length === 0}>
              <Icon name="check" size={13} /> {t("settings.save")}
            </button>
          </span>
        </div>
      </div>
    </div>
  );
}
