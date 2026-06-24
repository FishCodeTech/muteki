"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import {
  CredentialAccount,
  EngineHealth,
  SystemLoginStatus,
  WorkerModelOptions,
  WorkerSettings as WS,
  deleteCredentialAccount,
  getEngineHealth,
  getSystemLogin,
  getWorkerSettings,
  getWorkerModelOptions,
  listCredentialAccounts,
  putCredentialAccount,
  putRuntimeEnvironment,
  putWorkerSettings,
  testCredentialAccount,
  testLlmEndpoint,
  testWorkerProfileModel,
} from "@/lib/useRun";
import { useT } from "@/lib/i18n";
import { Icon } from "@/components/Icon";

/**
 * Global worker config (DESIGN_settings_panel_redesign). ONE config reused by
 * every solve — no per-run / per-worker layer. The panel is split by semantics:
 * engines · credential accounts (which CHANGE FACE by run environment) · run
 * environment (one container per solve) · scheduling · reasoning models · an
 * advanced profile-detail drawer · engine self-check.
 */

type WorkerProfile = WS["worker_profiles"][number];
type AccountType = "claude" | "codex" | "cursor" | "api";
type Backend = "local" | "container";
type NetworkMode = "bridge" | "host" | "none";

const BASE_ENGINES = ["claude", "codex", "cursor"] as const;
const ORDINARY_PROFILE_ROLES = new Set(["race", "bootstrap", "explore", "respond"]);

const profileName = (p: WorkerProfile): string => p.name || p.id;

const profileHasOrdinaryWorkerRole = (p: WorkerProfile): boolean =>
  (p.roles || []).some((r) => ORDINARY_PROFILE_ROLES.has(String(r)));

const selectedOrdinaryProfiles = (profiles: WorkerProfile[], selectedRefs: string[]): WorkerProfile[] => {
  const selected = new Set(selectedRefs);
  return profiles.filter((p) =>
    profileHasOrdinaryWorkerRole(p)
    && (selected.has(profileName(p)) || selected.has(p.id) || selected.has(p.engine))
  );
};

const profileCapacity = (profiles: WorkerProfile[], selectedRefs: string[]): number =>
  selectedOrdinaryProfiles(profiles, selectedRefs)
    .reduce((sum, p) => sum + Math.max(1, Number(p.max_running ?? 1) || 1), 0);

const syncProfileCapacityToMaxWorkers = (
  profiles: WorkerProfile[],
  selectedRefs: string[],
  maxWorkers: number
): WorkerProfile[] => {
  const selected = selectedOrdinaryProfiles(profiles, selectedRefs);
  if (selected.length === 0) return profiles;
  const floor = selected.length;
  const target = Math.max(floor, Math.max(1, Math.floor(maxWorkers) || 1));
  const current = profileCapacity(profiles, selectedRefs);
  if (current === target) return profiles;

  const next = profiles.map((p) => ({ ...p }));
  const selectedNames = new Set(selected.map(profileName));
  const eligible = next
    .filter((p) => selectedNames.has(profileName(p)))
    .sort((a, b) =>
      (Number(a.priority ?? 100) - Number(b.priority ?? 100))
      || profileName(a).localeCompare(profileName(b))
    );
  let total = profileCapacity(next, selectedRefs);
  if (total < target) {
    let idx = 0;
    while (total < target) {
      const p = eligible[idx % eligible.length];
      p.max_running = Math.max(1, Number(p.max_running ?? 1) || 1) + 1;
      total += 1;
      idx += 1;
    }
  } else {
    const shrink = [...eligible].reverse();
    let idx = 0;
    while (total > target && shrink.some((p) => Math.max(1, Number(p.max_running ?? 1) || 1) > 1)) {
      const p = shrink[idx % shrink.length];
      const currentMax = Math.max(1, Number(p.max_running ?? 1) || 1);
      if (currentMax > 1) {
        p.max_running = currentMax - 1;
        total -= 1;
      }
      idx += 1;
    }
  }
  return next;
};

export function WorkerSettings({ open, onClose }: { open: boolean; onClose: () => void }) {
  const t = useT();
  const [cfg, setCfg] = useState<WS | null>(null);
  const [engines, setEngines] = useState<string[]>([]);
  const [startWorkers, setStartWorkers] = useState(3);
  const [maxWorkers, setMaxWorkers] = useState(10);
  const [workerBackend, setWorkerBackend] = useState<Backend>("container");
  const [runtimeId, setRuntimeId] = useState("docker-web");
  // 坑 C: users pick a NETWORK mode, not an image/recipe. networkMode is the UI
  // control; it resolves to the matching container runtime preset on save.
  const [networkMode, setNetworkMode] = useState<NetworkMode>("bridge");
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
  // display it instead of an orphan "api". The account id auto-aligns to the
  // agent's default reference (<engine>-main) unless the operator typed a custom one.
  const [accountApiEngine, setAccountApiEngine] = useState<"claude" | "codex" | "cursor">("claude");
  const [accountSecret, setAccountSecret] = useState("");
  const [accountBaseUrl, setAccountBaseUrl] = useState("");
  const [accountStatus, setAccountStatus] = useState<"idle" | "saving" | "saved" | "error">("idle");
  const [acctTest, setAcctTest] = useState<Record<string, { ok: boolean; detail: string; layer?: string; testing?: boolean }>>({});
  const [formTest, setFormTest] = useState<{ ok: boolean; detail: string; layer?: string; testing?: boolean } | null>(null);
  const [workerProfiles, setWorkerProfiles] = useState<WorkerProfile[]>([]);
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
    setLlmTest(null);
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
      // derive the run's current runtime from the enabled profiles' shared runtime
      const rt = (c.worker_profiles ?? [])[0]?.runtime
        || (c.worker_backend === "local" ? "local" : "docker-web");
      setRuntimeId(rt);
      // derive the NETWORK mode from that runtime preset's network value (坑 C).
      const rtNet = (c.runtime_profiles ?? []).find((r) => r.id === rt)?.network;
      setNetworkMode((rtNet === "host" || rtNet === "none") ? rtNet : "bridge");
    });
    listCredentialAccounts().then(setAccounts);
    getSystemLogin().then(setSysLogin);
    getWorkerModelOptions().then(setModelOptions);
  }, [open]);

  // Esc-to-close + focus trap + focus restore (unchanged from prior panel).
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
  // id (bridge→docker-web, host→docker-host-target, none→docker-offline). Falls
  // back to the first preset of that network, then docker-web. There is ONE image;
  // these presets only differ in network/resources, not the image.
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

  // which base engines are dispatched? (engines name profiles; map to base engine)
  const dispatchedEngines = useMemo(() => {
    const profByName = new Map((workerProfiles || []).map((p) => [p.name || p.id, p]));
    const set = new Set<string>();
    for (const e of engines) {
      const p = profByName.get(e);
      set.add(p ? p.engine : e);
    }
    return set;
  }, [engines, workerProfiles]);

  // account registered for a base engine?
  const accountForEngine = (engine: string) =>
    accounts.find((a) => a.engine === engine && a.present);

  const engineOptions = useMemo(
    () => workerProfiles.length > 0
      ? workerProfiles.map((p) => p.name || p.id)
      : ["claude", "codex", "cursor"],
    [workerProfiles]
  );
  const reviewOptions = useMemo(() => {
    const reviewProfiles = (workerProfiles || []).filter((p) =>
      p.enabled !== false && ((p.roles || []).includes("review"))
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

  useEffect(() => {
    if (!open || workerProfiles.length === 0) return;
    setStartWorkers((prev) => Math.min(prev, maxWorkers));
    setWorkerProfiles((prev) => syncProfileCapacityToMaxWorkers(prev, engines, maxWorkers));
  }, [open, engines, maxWorkers, workerProfiles.length]);

  const selectedCapacity = useMemo(
    () => profileCapacity(workerProfiles, engines),
    [workerProfiles, engines]
  );

  if (!open) return null;

  // human label for the backend a test runs against — so "测连通" is never
  // ambiguous about whether it hit the host or a container. Container shows the
  // live network mode (坑 C: there's one image, network is the meaningful axis).
  const networkLabel = (n: NetworkMode) =>
    n === "host" ? t("settings.netHost") : n === "none" ? t("settings.netNone") : t("settings.netBridge");
  const backendLabel = workerBackend === "container"
    ? `${t("settings.runtimeContainer")} · ${networkLabel(networkMode)}`
    : t("settings.runtimeLocal");

  const toggleEngine = (e: string) =>
    setEngines((prev) => (prev.includes(e) ? prev.filter((x) => x !== e) : [...prev, e]));

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
    const nextByName = new Map(nextProfiles.map((p) => [profileName(p), p]));
    if (nextByName.has(ref)) return ref;
    const base = baseEngineForRef(ref, prevProfiles) || baseEngineForRef(ref, nextProfiles);
    const mapped = base ? nextProfiles.find((p) => p.engine === base) : undefined;
    return mapped ? profileName(mapped) : fallback;
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

  // The chip's display label must track the run environment, not the profile's
  // historical name: a profile literally named "claude-sub-container" is wrong on
  // screen once the run is local (it isn't in a container). In local mode show the
  // base engine (claude/codex/cursor, taken from the authoritative `engine` field —
  // not by stripping the name); in container mode keep the profile name, which does
  // describe the container. If two enabled profiles share one base engine, local
  // mode keeps the full name for the duplicates so every chip stays distinguishable.
  const engineLabel = (name: string): string => {
    if (workerBackend !== "local") return name;
    const prof = (workerProfiles || []).find((p) => (p.name || p.id) === name);
    if (!prof?.engine) return name;
    const sameEngine = (workerProfiles || []).filter((p) => p.engine === prof.engine);
    return sameEngine.length > 1 ? name : prof.engine;
  };

  const save = async () => {
    if (engines.length === 0) {
      setStatus("error");
      return;
    }
    setStatus("saving");
    // 1) run environment write-back (unifies backend + runtime across all
    //    enabled profiles, DESIGN §5) — do this first so worker_profiles below
    //    reflect the chosen runtime.
    // 坑 C: container runtime is resolved from the chosen NETWORK mode, not a
    // user-picked image/recipe id.
    const wantedRuntime = workerBackend === "local"
      ? localRuntimeId
      : containerRuntimeForNetwork(networkMode);
    const rtCfg = await putRuntimeEnvironment(workerBackend, wantedRuntime);
    const currentById = new Map(workerProfiles.map((p) => [p.id, p]));
    const currentByName = new Map(workerProfiles.map((p) => [profileName(p), p]));
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
    let profilesToSave = rtCfg?.worker_profiles?.length
      ? rtCfg.worker_profiles.map(mergeProfileEdits)
      : workerProfiles.map((p) => ({ ...p, runtime: wantedRuntime }));
    const nextEngines = alignProfileRefs(engines, profilesToSave, workerProfiles);
    if (nextEngines.length === 0) {
      setStatus("error");
      return;
    }
    profilesToSave = syncProfileCapacityToMaxWorkers(
      profilesToSave,
      nextEngines,
      maxWorkers
    );
    const nextReviewEngine = alignProfileRef(
      reviewEngine || reviewOptions[0] || engines[0],
      profilesToSave,
      workerProfiles,
      nextEngines[0]
    ) || nextEngines[0];
    // 2) the rest of the roster + budgets + models
    const res = await putWorkerSettings({
      engines: nextEngines,
      start_workers: startWorkers,
      max_workers: maxWorkers,
      worker_backend: workerBackend,
      race_scout: raceScout,
      race_timeout: raceTimeout,
      wall_clock_budget: wallClockBudget,
      max_total_workers: maxTotalWorkers,
      cost_budget_usd: costBudgetUsd,
      stage_policy: {
        prepare: {},
        race: { enabled: raceScout, timeout: raceTimeout, engines: [] },
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
      setReviewEngine(res.stage_policy?.coordinator?.review?.engine ?? nextReviewEngine);
      setWorkerBackend(res.worker_backend ?? workerBackend);
      setWorkerProfiles(res.worker_profiles ?? profilesToSave);
      setStatus("saved");
    } else setStatus("error");
  };

  const refreshAccounts = async () => {
    setAccounts(await listCredentialAccounts());
    setSysLogin(await getSystemLogin());
  };

  // An account id the operator hasn't personalized — safe to auto-realign to the
  // <engine>-main convention when the type / target agent changes. A custom id is
  // left untouched (the operator owns it; the hint still names the recommended one).
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

  const saveAccount = async (): Promise<CredentialAccount | null> => {
    if (!accountId.trim() || !accountSecret.trim()) {
      setAccountStatus("error");
      return null;
    }
    setAccountStatus("saving");
    setFormTest(null);
    const engine = accountType;
    const saved = await putCredentialAccount(
      accountId.trim(),
      engine === "codex"
        ? { engine, codex_auth_json: accountSecret }
        : engine === "api"
          ? { engine, secret: accountSecret, base_url: accountBaseUrl, target_engine: accountApiEngine }
          : { engine, secret: accountSecret }
    );
    if (saved) {
      setAccountSecret("");
      setAccountStatus("saved");
      await refreshAccounts();
      return saved;
    }
    setAccountStatus("error");
    return null;
  };

  // Save the account THEN immediately test it (against the current run backend)
  // — so registering a credential can be verified in one click, not after
  // hunting for it in the engine rows above.
  const saveAndTestAccount = async () => {
    const saved = await saveAccount();
    if (!saved) return;
    setFormTest({ ok: false, detail: "", testing: true });
    // account engine "api" maps to a base engine for the probe; the store
    // already knows the resolved engine, so use it.
    const r = await testCredentialAccount(saved.account_id, saved.engine, workerBackend);
    setFormTest({ ...r, testing: false });
  };

  const removeAccount = async (id: string) => {
    if (await deleteCredentialAccount(id)) await refreshAccounts();
  };

  const runAccountTest = async (acct: CredentialAccount) => {
    setAcctTest((p) => ({ ...p, [acct.account_id]: { ok: false, detail: "", testing: true } }));
    const r = await testCredentialAccount(acct.account_id, acct.engine, workerBackend);
    setAcctTest((p) => ({ ...p, [acct.account_id]: { ...r, testing: false } }));
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
    // self-check the CURRENT run environment: local → host CLI+auth, container
    // → docker run --rm verifying the worker image's CLIs launch.
    setHealth(await getEngineHealth(workerBackend));
    setChecking(false);
  };

  const updateProfile = (id: string, patch: Partial<WorkerProfile>) =>
    setWorkerProfiles((prev) => prev.map((p) => (p.id === id ? { ...p, ...patch } : p)));

  const sysLoginLabel = (s: SystemLoginStatus | undefined) =>
    s === "present" ? t("settings.sysLoginPresent")
      : s === "absent" ? t("settings.sysLoginAbsent")
        : t("settings.sysLoginUnknown");

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal worker-settings" ref={modalRef} onClick={(e) => e.stopPropagation()} role="dialog" aria-modal="true" aria-label={t("settings.title")}>
        <div className="modal-head">
          <div>
            <span>{t("settings.title")}</span>
            <p>{t("settings.globalScope")}</p>
          </div>
          <button className="modal-x" onClick={onClose} title={t("settings.close")} aria-label={t("settings.close")}><Icon name="x" size={15} /></button>
        </div>

        <div className="ws-summary" aria-label={t("settings.summary")}>
          <span><b>{engines.length}</b>{t("settings.summaryEngines")}</span>
          <span><b>{workerBackend === "local" ? t("settings.runtimeLocal") : networkLabel(networkMode)}</b>{t("settings.summaryRuntime")}</span>
          <span><b>{accounts.length}</b>{t("settings.summaryAccounts")}</span>
          <span><b>{workerProfiles.length}</b>{t("settings.summaryProfiles")}</span>
        </div>

        <div className="ws-scroll">
          {/* 1 · engines */}
          <section className="ws-section">
            <div className="ws-section-head">
              <h3>{t("settings.secEngines")}</h3>
              <span>{t("settings.secEnginesHint")}</span>
            </div>
            <div className="ws-engines">
              {engineOptions.map((e) => (
                <button
                  key={e}
                  type="button"
                  className={`ws-engine ${engines.includes(e) ? "on" : ""}`}
                  onClick={() => toggleEngine(e)}
                  aria-pressed={engines.includes(e)}
                  title={engineLabel(e) === e ? undefined : e}
                >{engineLabel(e)}</button>
              ))}
            </div>
          </section>

          {/* 2 · run environment (defines the single per-run container) */}
          <section className="ws-section">
            <div className="ws-section-head">
              <h3>{t("settings.secRuntime")} <span className="ws-tag">{t("settings.secRuntimeTag")}</span></h3>
              <span>{t("settings.secRuntimeHint")}</span>
            </div>
            <div className="ws-grid">
              <div className="ws-field">
                <label>{t("settings.runWhere")}</label>
                <select value={workerBackend} onChange={(e) => setWorkerBackend(e.target.value as Backend)}>
                  <option value="container">{t("settings.runtimeContainer")}</option>
                  <option value="local">{t("settings.runtimeLocal")}</option>
                </select>
              </div>
              <div className="ws-field">
                {/* 坑 C: NOT an image/recipe picker — there's one generic worker
                    image. This only selects the container's NETWORK mode; it maps to
                    the matching container runtime preset (bridge/host/none). Memory/
                    CPU live in the Advanced drawer. */}
                <label>{t("settings.network")}</label>
                <select value={networkMode} disabled={workerBackend === "local"}
                  onChange={(e) => setNetworkMode(e.target.value as NetworkMode)}>
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
          </section>

          {/* 3 · credential accounts — CHANGES FACE by run environment */}
          <section className="ws-section">
            <div className="ws-section-head">
              <h3>{t("settings.secCredentials")} <span className="ws-tag">{backendLabel}</span></h3>
              <span>{t("settings.testsAgainst")} {backendLabel}</span>
            </div>
            {workerBackend === "container" ? (
              <div className="ws-note ws-note-warn">{t("settings.credContainerWarn")}</div>
            ) : (
              <div className="ws-note ws-note-info">{t("settings.credLocalNote")}</div>
            )}
            <div className="ws-cred-list">
              {BASE_ENGINES.filter((e) => dispatchedEngines.has(e)).map((e) => {
                const acct = accountForEngine(e);
                const tinfo = acct ? acctTest[acct.account_id] : undefined;
                const profile = workerProfiles.find((p) => p.engine === e);
                const opts = profile ? (modelOptions.models[profile.engine] ?? []) : [];
                const known = opts.some((o) => o.id === (profile?.model ?? ""));
                const selected = !profile || (profile.model ?? "") === "" ? "" : known ? (profile.model ?? "") : "__custom__";
                const minfo = profile ? modelTest[profile.id] : undefined;
                let stateEl;
                if (workerBackend === "container") {
                  stateEl = acct
                    ? <span className="ws-ok"><Icon name="check" size={12} /> {t("settings.credRegistered")}</span>
                    : <span className="ws-bad"><Icon name="x" size={12} /> {t("settings.credMissing")}</span>;
                } else {
                  const s = acct ? "present" : sysLogin[e];
                  stateEl = s === "present"
                    ? <span className="ws-ok"><Icon name="check" size={12} /> {acct ? t("settings.credRegistered") : t("settings.sysLoginPresent")}</span>
                    : <span className="ws-muted">{sysLoginLabel(s)}</span>;
                }
                return (
                  <div className={`ws-cred-row ${workerBackend === "container" && !acct ? "missing" : ""}`} key={e}>
                    <code>{e}</code>
                    <span className="ws-cred-acct">
                      {acct ? acct.account_id : (workerBackend === "container" ? "—" : t("settings.runtimeLocal"))}
                      {acct?.mode === "custom_endpoint" && <em> · {t("settings.modeCustomEndpoint")}</em>}
                    </span>
                    {stateEl}
                    {acct && (
                      <span className="ws-cred-test">
                        {tinfo && !tinfo.testing && (
                          tinfo.ok
                            ? <span className="ws-ok"><Icon name="check" size={12} /> {t("settings.ok")}</span>
                            : <span className="ws-bad" title={tinfo.detail}><Icon name="x" size={12} /> {tinfo.layer ? `${tinfo.layer}` : t("settings.bad")}</span>
                        )}
                        <button className="ws-mini-btn" type="button" disabled={tinfo?.testing}
                          onClick={() => runAccountTest(acct)} title={tinfo?.detail || t("settings.testConn")}>
                          <Icon name="plug" size={12} /> {tinfo?.testing ? t("settings.testing") : t("settings.testConn")}
                        </button>
                      </span>
                    )}
                    {profile && (
                      <div className="ws-cred-model" aria-label={`${e} ${t("settings.secWorkerModels")}`}>
                        <span className="ws-model-label">{t("settings.secWorkerModels")}</span>
                        <select value={selected} onChange={(ev) => {
                          const v = ev.target.value;
                          if (v !== "__custom__") updateProfile(profile.id, { model: v });
                        }}>
                          <option value="">{t("settings.modelDefault")}</option>
                          {opts.map((o) => <option value={o.id} key={o.id}>{o.label}</option>)}
                          <option value="__custom__">{t("settings.customModel")}</option>
                        </select>
                        <input
                          value={profile.model ?? ""}
                          placeholder={t("settings.customModel")}
                          onChange={(ev) => updateProfile(profile.id, { model: ev.target.value })}
                          spellCheck={false}
                        />
                        <span className="ws-model-test">
                          {minfo && !minfo.testing && (
                            minfo.ok
                              ? <span className="ws-ok"><Icon name="check" size={12} /> {t("settings.ok")}</span>
                              : <span className="ws-bad" title={minfo.detail}><Icon name="x" size={12} /> {t("settings.bad")}</span>
                          )}
                          <button className="ws-mini-btn" type="button" disabled={minfo?.testing}
                            onClick={() => runModelTest(profile)} title={minfo?.detail || t("settings.testModel")}>
                            <Icon name="plug" size={12} /> {minfo?.testing ? t("settings.testing") : t("settings.testModel")}
                          </button>
                        </span>
                      </div>
                    )}
                  </div>
                );
              })}
            </div>

            {accounts.length > 0 && (
              <div className="ws-account-list">
                {accounts.map((a) => (
                  <div className="ws-account-row" key={a.account_id}>
                    <code>{a.account_id}</code>
                    <span>{a.engine}</span>
                    <span>{a.mode === "custom_endpoint" ? t("settings.modeCustomEndpoint") : a.mode}</span>
                    <button className="modal-x" type="button" onClick={() => removeAccount(a.account_id)} aria-label={t("settings.deleteAccount")}>
                      <Icon name="x" size={13} />
                    </button>
                  </div>
                ))}
              </div>
            )}

            <div className="ws-grid ws-account-form">
              <div className="ws-field">
                <label>{t("settings.accountId")}</label>
                <input value={accountId} onChange={(e) => setAccountId(e.target.value)} />
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
                  <div className="ws-field">
                    <label>{t("settings.baseUrl")}</label>
                    <input value={accountBaseUrl} onChange={(e) => setAccountBaseUrl(e.target.value)} />
                  </div>
                </>
              )}
              <div className="ws-field ws-span-all">
                <label>{accountType === "codex" ? t("settings.codexAuthJson") : t("settings.secret")}</label>
                {accountType === "codex" ? (
                  <textarea value={accountSecret} onChange={(e) => setAccountSecret(e.target.value)} rows={3} spellCheck={false} placeholder='{"OPENAI_API_KEY":"..."}' />
                ) : (
                  <input type="password" value={accountSecret} onChange={(e) => setAccountSecret(e.target.value)} spellCheck={false} />
                )}
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
              <button className="ws-save" onClick={() => { void saveAccount(); }} disabled={accountStatus === "saving"}>{t("settings.saveAccount")}</button>
            </div>
          </section>

          {/* 4 · scheduling & budget */}
          <section className="ws-section">
            <div className="ws-section-head"><h3>{t("settings.secSchedule")}</h3></div>
            <div className="ws-grid">
              <div className="ws-field">
                <label>{t("settings.startWorkers")}</label>
                <input type="number" min={1} max={maxWorkers} value={startWorkers}
                  onChange={(e) => setStartWorkers(Math.max(1, parseInt(e.target.value) || 1))} />
              </div>
              <div className="ws-field">
                <label>{t("settings.maxWorkers")}</label>
                <input type="number" min={1} value={maxWorkers}
                  onChange={(e) => setMaxWorkers(Math.max(1, parseInt(e.target.value) || 1))} />
              </div>
              <div className="ws-field">
                <label>{t("settings.raceScout")}</label>
                <select value={raceScout ? "1" : "0"} onChange={(e) => setRaceScout(e.target.value === "1")}>
                  <option value="1">{t("settings.enabled")}</option>
                  <option value="0">{t("settings.disabled")}</option>
                </select>
              </div>
              <div className="ws-field">
                <label>{t("settings.raceTimeout")}</label>
                <input type="number" min={1} value={raceTimeout}
                  onChange={(e) => setRaceTimeout(Math.max(1, parseInt(e.target.value) || 720))} />
              </div>
              <div className="ws-field">
                <label>{t("settings.wallClockBudget")}</label>
                <input type="number" min={0} value={wallClockBudget}
                  onChange={(e) => setWallClockBudget(Math.max(0, parseInt(e.target.value) || 0))} />
              </div>
              <div className="ws-field">
                <label>{t("settings.maxTotalWorkers")}</label>
                <input type="number" min={0} value={maxTotalWorkers}
                  onChange={(e) => setMaxTotalWorkers(Math.max(0, parseInt(e.target.value) || 0))} />
              </div>
              <div className="ws-field">
                <label>{t("settings.costBudgetUsd")}</label>
                <input type="number" min={0} step="0.01" value={costBudgetUsd}
                  onChange={(e) => setCostBudgetUsd(Math.max(0, parseFloat(e.target.value) || 0))} />
              </div>
              <div className="ws-field">
                <label>{t("settings.reviewEnabled")}</label>
                <select value={reviewEnabled ? "1" : "0"} onChange={(e) => setReviewEnabled(e.target.value === "1")}>
                  <option value="1">{t("settings.enabled")}</option>
                  <option value="0">{t("settings.disabled")}</option>
                </select>
              </div>
              <div className="ws-field">
                <label>{t("settings.reviewEngine")}</label>
                <select value={reviewEngine} disabled={!reviewEnabled || reviewOptions.length === 0}
                  onChange={(e) => setReviewEngine(e.target.value)}>
                  {reviewOptions.map((e) => (
                    <option value={e} key={e}>{engineLabel(e)}</option>
                  ))}
                </select>
              </div>
              <div className="ws-field">
                <label>{t("settings.reviewTimeout")}</label>
                <input type="number" min={60} value={reviewTimeout} disabled={!reviewEnabled}
                  onChange={(e) => setReviewTimeout(Math.max(60, parseInt(e.target.value) || 420))} />
              </div>
              <div className="ws-field">
                <label>{t("settings.reviewMaxConcurrent")}</label>
                <input type="number" min={1} value={reviewMaxConcurrent} disabled={!reviewEnabled}
                  onChange={(e) => setReviewMaxConcurrent(Math.max(1, parseInt(e.target.value) || 1))} />
              </div>
              <div className="ws-field">
                <label>{t("settings.reviewCandidateThreshold")}</label>
                <input type="number" min={1} value={reviewCandidateThreshold} disabled={!reviewEnabled}
                  onChange={(e) => setReviewCandidateThreshold(Math.max(1, parseInt(e.target.value) || 5))} />
              </div>
              <div className="ws-field">
                <label>{t("settings.reviewFallback")}</label>
                <select value={reviewFallback ? "1" : "0"} disabled={!reviewEnabled}
                  onChange={(e) => setReviewFallback(e.target.value === "1")}>
                  <option value="0">{t("settings.disabled")}</option>
                  <option value="1">{t("settings.enabled")}</option>
                </select>
              </div>
            </div>
            <div className={`ws-note ${selectedCapacity < maxWorkers ? "ws-note-warn" : "ws-note-info"}`}>
              {t("settings.capacityLinked", { capacity: selectedCapacity, max: maxWorkers })}
            </div>
          </section>

          {/* 6 · reasoning models (planner/titler) — endpoint configurable, key in .env */}
          <section className="ws-section">
            <div className="ws-section-head">
              <h3>{t("settings.secReason")}</h3>
              <span>{t("settings.reasonHint")}</span>
            </div>
            <div className="ws-note ws-note-info">{t("settings.reasonKeyNote")}</div>
            <div className="ws-grid">
              <div className="ws-field ws-span-all">
                <label>{t("settings.baseUrlEmptyDeepseek")}</label>
                <input value={llmBaseUrl} placeholder="https://api.deepseek.com/v1" onChange={(e) => setLlmBaseUrl(e.target.value)} />
              </div>
              <div className="ws-field">
                <label>{t("settings.plannerModel")}</label>
                <input value={plannerModel} onChange={(e) => setPlannerModel(e.target.value)} />
              </div>
              <div className="ws-field">
                <label>{t("settings.titlerModel")}</label>
                <input value={titlerModel} onChange={(e) => setTitlerModel(e.target.value)} />
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
          </section>

          {/* 7 · advanced: profile details (collapsed) */}
          {workerProfiles.length > 0 && (
            <details className="ws-section ws-details">
              <summary>
                <Icon name="chevronRight" size={14} />
                <span>{t("settings.advProfiles")}</span>
                <em>{t("settings.advProfilesHint")}</em>
              </summary>
              <div className="ws-profile-mini">
                {workerProfiles.map((p) => (
                  <div className="ws-profile-mini-row" key={p.id}>
                    <code title={p.id}>{p.id}</code>
                    <label>{t("settings.profileMaxRunning")}
                      <input type="number" min={1} value={p.max_running ?? 1}
                        onChange={(e) => updateProfile(p.id, { max_running: Math.max(1, parseInt(e.target.value) || 1) })} />
                    </label>
                    <label>{t("settings.profileMaxReviewRunning")}
                      <input type="number" min={0} value={p.max_review_running ?? 0}
                        onChange={(e) => updateProfile(p.id, { max_review_running: Math.max(0, parseInt(e.target.value) || 0) })} />
                    </label>
                    <label>{t("settings.profilePriority")}
                      <input type="number" min={0} value={p.priority ?? 100}
                        onChange={(e) => updateProfile(p.id, { priority: Math.max(0, parseInt(e.target.value) || 0) })} />
                    </label>
                    <label>{t("settings.profileModel")}
                      <input value={p.model ?? ""} placeholder={t("settings.profileModel")}
                        onChange={(e) => updateProfile(p.id, { model: e.target.value })} />
                    </label>
                  </div>
                ))}
              </div>
            </details>
          )}

          {/* 8 · engine self-check — checks the CURRENT run environment */}
          <section className="ws-section ws-selfcheck">
            <div className="ws-sc-head">
              <div className="ws-section-head">
                <h3>{t("settings.selfcheck")} <span className="ws-tag">{backendLabel}</span></h3>
                <span>{workerBackend === "container" ? t("settings.selfcheckContainerNote") : t("settings.selfcheckHostNote")}</span>
              </div>
              <button className="ws-mini-btn" onClick={runSelfCheck} disabled={checking}>
                <Icon name="refresh" size={13} /> {checking ? t("settings.checking") : t("settings.runCheck")}
              </button>
            </div>
            {health && (
              <div className="ws-sc-list">
                {health.map((h) => {
                  // local mode + auto-discovered (not env-pinned) bin → warn that the
                  // default PATH resolution may pick the wrong CLI version, and point
                  // at the env var that pins it. Container mode bins are baked, no warn.
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
        </div>

        <div className="ws-savebar">
          <span className={`ws-status ${status}`}>
            {status === "saved" ? t("settings.saved")
              : status === "error" ? t("settings.invalid")
                : status === "saving" ? "..." : ""}
          </span>
          <button className="ws-save" onClick={save} disabled={status === "saving"}>{t("settings.save")}</button>
        </div>
      </div>
    </div>
  );
}
