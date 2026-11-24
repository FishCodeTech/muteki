"use client";

import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { useRouter } from "next/navigation";
import {
  Badge,
  Button,
  Card,
  CardBody,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
  Chip,
  Dialog,
  Label,
  NativeSelect,
  TextArea,
  TextField,
  type Tone,
} from "@muteki/ui";
import { Icon, type IconName } from "@muteki/ui";
import type { CompetitionInfo, ConnectionView } from "@/lib/competition-events";
import {
  createConnection,
  fetchBrowserSession,
  fetchCompetitions,
  fetchConnections,
  fetchPlatformKinds,
  fetchPlatformSecrets,
  probeConnection,
  registerCompetition,
  revokeBrowserSession,
  revokePlatformSecret,
  unregisterCompetition,
  unregisterConnection,
  updateCompetitionPolicy,
  writeBrowserSession,
  writePlatformSecret,
  type BrowserSessionStatus,
  type CommandReceiptView,
  type PlatformKindView,
  type PlatformSecretMetadata,
} from "@/lib/useCompetition";

import { createReadCache, readCacheIdentity } from "@/lib/readCache";

const connectionsCache = createReadCache<ConnectionView[]>(30_000, 1);
const competitionsCache = createReadCache<CompetitionInfo[]>(30_000, 1);
const secretsCache = createReadCache<PlatformSecretMetadata[]>(30_000, 1);
const kindsCache = createReadCache<PlatformKindView[]>(60_000, 1);

type Section = "competitions" | "connections";
type ModalKind = "new-connection" | "new-competition" | "connection-detail" | null;
type ConfirmAction = "secret" | "browser" | "competition" | "connection" | null;
type Feedback = { kind: "ok" | "bad"; detail: string } | null;

const FALLBACK_PLATFORMS: { value: string; label: string; icon: IconName }[] = [
  { value: "mock", label: "本地模拟", icon: "cpu" },
  { value: "ctfd", label: "CTFd", icon: "flag" },
  { value: "rctf", label: "rCTF", icon: "flag" },
  { value: "gzctf", label: "GZCTF", icon: "flag" },
  { value: "generic_browser", label: "浏览器会话", icon: "globe" },
];

function kindIcon(raw: string | undefined): IconName {
  const value = (raw || "").toLowerCase();
  if (value === "cpu" || value === "globe" || value === "flag" || value === "plug") {
    return value;
  }
  return "plug";
}

function kindsToPlatforms(kinds: PlatformKindView[]): { value: string; label: string; icon: IconName }[] {
  if (!kinds.length) return FALLBACK_PLATFORMS;
  return kinds.map((item) => ({
    value: item.id,
    label: item.source === "extension" ? `${item.label} · 扩展` : item.label,
    icon: kindIcon(item.icon),
  }));
}
const CAP_LABELS: [string, string][] = [
  ["sync", "同步"],
  ["artifacts", "附件"],
  ["dynamic_instances", "动态实例"],
  ["submit", "提交"],
  ["scoreboard", "计分板"],
];

const STATUS_LABELS: Record<string, { label: string; tone: "ok" | "warn" | "bad" | "muted" }> = {
  active: { label: "活动", tone: "ok" },
  disabled: { label: "已停用", tone: "muted" },
  auth_required: { label: "需要重新认证", tone: "bad" },
};

const SCHEDULER_LABELS: Record<string, { label: string; tone: "ok" | "warn" | "muted" }> = {
  running: { label: "调度运行中", tone: "ok" },
  paused: { label: "调度已暂停", tone: "warn" },
  stopped: { label: "调度已停止", tone: "muted" },
};

const EMPTY_STORAGE = '{"cookies":[],"origins":[]}';
const TSEC_VPN_COMMAND = "sudo /opt/homebrew/sbin/openvpn --config ~/Downloads/<tsecbench-vpn-config>.ovpn";

function platformMeta(
  kind: string,
  platforms: { value: string; label: string; icon: IconName }[] = FALLBACK_PLATFORMS,
) {
  return platforms.find((item) => item.value === kind) || {
    value: kind,
    label: kind || "未知平台",
    icon: "plug" as IconName,
  };
}

function receiptError(receipt: CommandReceiptView, fallback: string): string {
  return receipt.error ? `${receipt.error.code}: ${receipt.error.message}` : fallback;
}

function probeFields(connection: ConnectionView): Array<{ key: string; label: string; value: string }> {
  const caps = connection.capabilities ?? {};
  const detail = (
    caps.detail && typeof caps.detail === "object" ? caps.detail : {}
  ) as Record<string, unknown>;
  const labels: Record<string, string> = {
    platform_type: "平台类型",
    platform_version: "版本",
    identity: "身份",
    clock_skew_seconds: "时钟偏差",
    rate_limit: "限速",
    schema_hash: "Schema 摘要",
  };
  return (Object.keys(labels) as Array<keyof typeof labels>)
    .map((key) => {
      const raw = caps[key] ?? detail[key];
      return raw == null ? null : { key, label: labels[key], value: String(raw) };
    })
    .filter((item): item is { key: string; label: string; value: string } => item !== null);
}

function StatusDot({ tone }: { tone: string }) {
  return <span className={`competition-status-dot ${tone}`} aria-hidden />;
}

export function CompetitionCenter() {
  const router = useRouter();
  const [connections, setConnections] = useState<ConnectionView[]>(() => connectionsCache.snapshot("list") || []);
  const [competitions, setCompetitions] = useState<CompetitionInfo[]>(() => competitionsCache.snapshot("list") || []);
  const [platforms, setPlatforms] = useState(() => kindsToPlatforms(kindsCache.snapshot("list") || []));
  const [secrets, setSecrets] = useState<PlatformSecretMetadata[]>(() => secretsCache.snapshot("list") || []);
  const [browserSession, setBrowserSession] = useState<BrowserSessionStatus | null>(null);
  const [section, setSection] = useState<Section>("competitions");
  const [modal, setModal] = useState<ModalKind>(null);
  const [detailConnectionId, setDetailConnectionId] = useState("");
  const [loading, setLoading] = useState(true);
  const [connectionsLoading, setConnectionsLoading] = useState(() => !connectionsCache.snapshot("list"));
  const [competitionsLoading, setCompetitionsLoading] = useState(() => !competitionsCache.snapshot("list"));
  const loadSequence = useRef(0);
  const [busy, setBusy] = useState(false);
  const [probing, setProbing] = useState("");
  const [feedback, setFeedback] = useState<Feedback>(null);
  const [confirmAction, setConfirmAction] = useState<ConfirmAction>(null);
  const [pendingCompetitionId, setPendingCompetitionId] = useState("");
  const focusCredentials = useRef(false);

  const [newKind, setNewKind] = useState("ctfd");
  const [newUrl, setNewUrl] = useState("");
  const [newAccount, setNewAccount] = useState("");
  const [newCredential, setNewCredential] = useState("");
  const [newCredentialRef, setNewCredentialRef] = useState("");

  const [registerConnectionId, setRegisterConnectionId] = useState("");
  const [externalId, setExternalId] = useState("");
  const [title, setTitle] = useState("");
  const [automationMode, setAutomationMode] = useState("assisted");
  const [maxConcurrentRuns, setMaxConcurrentRuns] = useState(3);

  const [secretMode, setSecretMode] = useState<"rotate" | "update">("rotate");
  const [secretValue, setSecretValue] = useState("");
  const [showSecret, setShowSecret] = useState(false);
  const [loadError, setLoadError] = useState("");
  const [secretsUnavailable, setSecretsUnavailable] = useState(false);

  useEffect(() => {
    setSecretValue("");
    setShowSecret(false);
    setStorageState(EMPTY_STORAGE);
    setExpiresAt("");
  }, [detailConnectionId, modal]);
  const [expiresAt, setExpiresAt] = useState("");
  const [storageState, setStorageState] = useState(EMPTY_STORAGE);

  const load = useCallback(async (refresh = true) => {
    const sequence = ++loadSequence.current;
    const identity = readCacheIdentity();
    const current = () => sequence === loadSequence.current && identity === readCacheIdentity();
    const errors = new Map<string, string>();
    setLoading(true);
    setLoadError("");
    const read = async <T,>(label: string, request: Promise<T>, apply: (data: T) => void, settled?: () => void) => {
      try {
        const data = await request;
        if (current()) apply(data);
      } catch (cause) {
        if (current()) {
          errors.set(label, `${label}读取失败：${cause instanceof Error ? cause.message : String(cause)}`);
          setLoadError([...errors.values()].join("；"));
        }
      } finally { if (current()) settled?.(); }
    };
    await Promise.all([
      read("平台连接", connectionsCache.read("list", fetchConnections, refresh), data => {
        setConnections(data);
        setRegisterConnectionId(value => value || data[0]?.connection_id || "");
      }, () => setConnectionsLoading(false)),
      read("比赛列表", competitionsCache.read("list", fetchCompetitions, refresh), setCompetitions, () => setCompetitionsLoading(false)),
      read("凭据状态", secretsCache.read("list", fetchPlatformSecrets, refresh), data => {
        setSecrets(data); setSecretsUnavailable(false);
      }, () => setSecretsUnavailable(errors.has("凭据状态"))),
      read("平台类型", kindsCache.read("list", fetchPlatformKinds, refresh), data => setPlatforms(kindsToPlatforms(data))),
    ]);
    if (current()) setLoading(false);
  }, []);

  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const focus = params.get("focus");
    if (focus === "credentials" || focus === "connections") {
      focusCredentials.current = true;
      setSection("connections");
    }
    void load(false);
    return () => { loadSequence.current += 1; };
  }, [load]);

  useEffect(() => {
    if (!focusCredentials.current || connectionsLoading || !connections.length) return;
    focusCredentials.current = false;
    openConnectionDetail(connections[0].connection_id);
  }, [connectionsLoading, connections]);

  const detailConnection = useMemo(
    () => connections.find((item) => item.connection_id === detailConnectionId) || null,
    [connections, detailConnectionId],
  );
  const selectedSecret = useMemo(
    () => detailConnection
      ? secrets.find((item) => item.connection_id === detailConnection.connection_id && item.status === "active") || null
      : null,
    [secrets, detailConnection],
  );
  const pendingCompetition = useMemo(
    () => competitions.find((item) => item.competition_id === pendingCompetitionId) || null,
    [competitions, pendingCompetitionId],
  );

  useEffect(() => {
    if (!detailConnection || detailConnection.platform_kind !== "generic_browser") {
      setBrowserSession(null);
      return;
    }
    let cancelled = false;
    fetchBrowserSession(detailConnection.connection_id)
      .then((session) => {
        if (!cancelled) setBrowserSession(session);
      })
      .catch((exc) => {
        if (!cancelled) {
          setFeedback({ kind: "bad", detail: exc instanceof Error ? exc.message : String(exc) });
        }
      });
    return () => {
      cancelled = true;
    };
  }, [detailConnection]);

  const runningCount = competitions.filter((item) => item.scheduler_state === "running").length;
  const activeConnections = connections.filter((item) => item.status === "active").length;
  const credentialCount = connections.filter((item) => item.has_credential).length;
  const workspaceTarget = useMemo(() => {
    if (!competitions.length) return null;
    return (
      competitions.find((item) => item.scheduler_state === "running")
      ?? competitions[0]
    );
  }, [competitions]);

  const openWorkspace = useCallback(() => {
    if (!workspaceTarget) {
      setSection("competitions");
      setFeedback({ kind: "bad", detail: "请先登记至少一场比赛。" });
      return;
    }
    router.push(`/competitions/${encodeURIComponent(workspaceTarget.competition_id)}`);
  }, [router, workspaceTarget]);

  const openConnectionDetail = (connectionId: string) => {
    setDetailConnectionId(connectionId);
    setModal("connection-detail");
    setFeedback(null);
  };

  const closeModal = () => {
    if (busy) return;
    setModal(null);
    setDetailConnectionId("");
  };

  const beginNewConnection = () => {
    setModal("new-connection");
    setFeedback(null);
  };

  const beginNewCompetition = (connectionId?: string) => {
    if (connectionId) setRegisterConnectionId(connectionId);
    setModal("new-competition");
    setFeedback(null);
  };

  const submitConnection = async (event: FormEvent) => {
    event.preventDefault();
    if (newCredential && newCredentialRef.trim()) {
      setFeedback({ kind: "bad", detail: "一次性凭据和已有 Secret 引用只能填一项。" });
      return;
    }
    setBusy(true);
    setFeedback(null);
    try {
      const receipt = await createConnection({
        platform_kind: newKind,
        base_url: newUrl.trim(),
        account_key: newAccount.trim(),
        ...(newCredentialRef.trim() ? { credential_ref: newCredentialRef.trim() } : {}),
        ...(newCredential ? { credential: newCredential } : {}),
      });
      if (receipt.state === "failed" || receipt.state === "conflict") {
        setFeedback({ kind: "bad", detail: receiptError(receipt, "创建连接失败") });
        return;
      }
      setNewUrl("");
      setNewAccount("");
      setNewCredential("");
      setNewCredentialRef("");
      setFeedback({ kind: "ok", detail: "连接已创建。凭据只保存为引用，页面不会回显原文。" });
      await load();
      setModal(null);
      setSection("connections");
      if (receipt.aggregate?.id) openConnectionDetail(receipt.aggregate.id);
    } catch (exc) {
      setFeedback({ kind: "bad", detail: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setBusy(false);
    }
  };

  const probe = async (connectionId: string) => {
    const checkingTsecVpn = connections.find(
      (item) => item.connection_id === connectionId,
    )?.platform_kind === "tsecbench";
    setProbing(connectionId);
    setFeedback(null);
    try {
      const receipt = await probeConnection(connectionId);
      if (receipt.state === "failed" || receipt.state === "conflict") {
        setFeedback({
          kind: "bad",
          detail: receiptError(receipt, checkingTsecVpn ? "VPN 连接检查失败" : "测试连接失败"),
        });
      } else {
        setFeedback({
          kind: "ok",
          detail: checkingTsecVpn ? "VPN 与 Tsecbench 平台连接正常。" : "测试已完成，能力与身份已回填。",
        });
      }
      await load();
    } catch (exc) {
      setFeedback({ kind: "bad", detail: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setProbing("");
    }
  };

  const submitCompetition = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setFeedback(null);
    try {
      const receipt = await registerCompetition({
        connection_id: registerConnectionId,
        external_competition_id: externalId.trim(),
        ...(title.trim() ? { title: title.trim() } : {}),
      });
      if (receipt.state === "failed" || receipt.state === "conflict") {
        setFeedback({ kind: "bad", detail: receiptError(receipt, "登记比赛失败") });
        return;
      }
      const newId = receipt.aggregate?.id || "";
      if (!newId) throw new Error("同步回执缺少比赛对象 ID");
      const policy = await updateCompetitionPolicy(newId, {
        automation_mode: automationMode,
        max_concurrent_runs: maxConcurrentRuns,
      });
      if (policy.error) {
        throw new Error(`${policy.error.code}: ${policy.error.message}`);
      }
      setExternalId("");
      setTitle("");
      setModal(null);
      setFeedback({ kind: "ok", detail: "首次同步与策略已受理，正在打开比赛工作区。" });
      await load();
      router.push(`/competitions/${encodeURIComponent(newId)}`);
    } catch (exc) {
      setFeedback({ kind: "bad", detail: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setBusy(false);
    }
  };

  const submitSecret = async () => {
    if (!detailConnection || !secretValue) return;
    setBusy(true);
    setFeedback(null);
    try {
      const receipt = await writePlatformSecret(detailConnection.connection_id, {
        credential: secretValue,
        mode: secretMode,
        expires_at: expiresAt,
      });
      if (receipt.state === "failed" || receipt.state === "conflict") {
        setFeedback({ kind: "bad", detail: receiptError(receipt, "写入凭据失败") });
        return;
      }
      setSecretValue("");
      setShowSecret(false);
      setFeedback({
        kind: "ok",
        detail: secretMode === "rotate" ? "凭据已轮换，旧引用失效。" : "凭据已更新，引用保持不变。",
      });
      await load();
    } catch (exc) {
      setFeedback({ kind: "bad", detail: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setBusy(false);
    }
  };

  const confirmRevokeSecret = async () => {
    if (!detailConnection) return;
    setBusy(true);
    setFeedback(null);
    try {
      const receipt = await revokePlatformSecret(detailConnection.connection_id);
      if (receipt.state === "failed" || receipt.state === "conflict") {
        setFeedback({ kind: "bad", detail: receiptError(receipt, "撤销凭据失败") });
        return;
      }
      setFeedback({ kind: "ok", detail: "平台凭据引用已撤销。" });
      await load();
    } catch (exc) {
      setFeedback({ kind: "bad", detail: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setBusy(false);
      setConfirmAction(null);
    }
  };

  const importBrowser = async (renew: boolean) => {
    if (!detailConnection) return;
    setBusy(true);
    setFeedback(null);
    try {
      const parsed = renew ? {} : JSON.parse(storageState) as Record<string, unknown>;
      const receipt = await writeBrowserSession(detailConnection.connection_id, {
        storage_state: parsed,
        expires_at: expiresAt,
        renew,
      });
      if (receipt.state === "failed" || receipt.state === "conflict") {
        setFeedback({ kind: "bad", detail: receiptError(receipt, renew ? "续期失败" : "导入失败") });
        return;
      }
      if (!renew) setStorageState(EMPTY_STORAGE);
      setFeedback({ kind: "ok", detail: renew ? "浏览器会话已续期。" : "浏览器会话已导入，页面不保存明文。" });
      setBrowserSession(await fetchBrowserSession(detailConnection.connection_id));
    } catch (exc) {
      setFeedback({ kind: "bad", detail: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setBusy(false);
    }
  };

  const confirmRevokeBrowser = async () => {
    if (!detailConnection) return;
    setBusy(true);
    setFeedback(null);
    try {
      const receipt = await revokeBrowserSession(detailConnection.connection_id);
      if (receipt.state === "failed" || receipt.state === "conflict") {
        setFeedback({ kind: "bad", detail: receiptError(receipt, "撤销浏览器会话失败") });
        return;
      }
      setFeedback({ kind: "ok", detail: "浏览器会话已撤销。" });
      setBrowserSession({ present: false, expired: false });
    } catch (exc) {
      setFeedback({ kind: "bad", detail: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setBusy(false);
      setConfirmAction(null);
    }
  };

  const confirmUnregisterCompetition = async () => {
    const target = pendingCompetition;
    if (!target) return;
    setBusy(true);
    setFeedback(null);
    try {
      const receipt = await unregisterCompetition(target.competition_id);
      if (receipt.state === "failed" || receipt.state === "conflict") {
        setFeedback({ kind: "bad", detail: receiptError(receipt, "移除比赛失败") });
        return;
      }
      setFeedback({ kind: "ok", detail: "比赛已从清单移除。历史记录保留，可用相同远端 ID 重新登记。" });
      setPendingCompetitionId("");
      await load();
    } catch (exc) {
      setFeedback({ kind: "bad", detail: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setBusy(false);
      setConfirmAction(null);
    }
  };

  const confirmUnregisterConnection = async () => {
    if (!detailConnection) return;
    setBusy(true);
    setFeedback(null);
    try {
      const receipt = await unregisterConnection(detailConnection.connection_id);
      if (receipt.state === "failed" || receipt.state === "conflict") {
        setFeedback({ kind: "bad", detail: receiptError(receipt, "移除连接失败") });
        return;
      }
      setFeedback({ kind: "ok", detail: "平台连接已从清单移除。可用相同地址与账户标识重新添加。" });
      closeModal();
      await load();
    } catch (exc) {
      setFeedback({ kind: "bad", detail: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setBusy(false);
      setConfirmAction(null);
    }
  };

  const registerReady = Boolean(
    registerConnectionId
    && externalId.trim()
    && connections.find((item) => item.connection_id === registerConnectionId)?.status === "active",
  );

  const boundCompetitions = detailConnection
    ? competitions.filter((row) => row.connection_id === detailConnection.connection_id)
    : [];

  return (
    <div className="competition-hub">
      <header className="competition-hub-head">
        <div>
          <span>比赛工作区</span>
          <h1>比赛大厅</h1>
          <p>先配置平台连接，再登记比赛并进入调度工作区。</p>
        </div>
        <div className="competition-hub-head-actions">
          <Button variant="ghost" onClick={() => void load()} disabled={loading}>
            <Icon name="refresh" size={14} />
            {loading ? "正在刷新…" : "刷新"}
          </Button>
          {section === "connections" ? (
            <Button variant="primary" className="primary" onClick={beginNewConnection}>
              <Icon name="plus" size={14} />
              添加连接
            </Button>
          ) : (
            <Button
              variant="primary"
              className="primary"
              onClick={() => beginNewCompetition()}
              disabled={!connections.length}
            >
              <Icon name="plus" size={14} />
              登记比赛
            </Button>
          )}
        </div>
      </header>

      <nav className="competition-hub-flow" aria-label="使用步骤">
        <ol className="competition-hub-steps">
          <li>
            <button
              type="button"
              className={`competition-hub-step${section === "connections" ? " on" : ""}${connections.length ? " done" : ""}`}
              aria-current={section === "connections" ? "step" : undefined}
              onClick={() => setSection("connections")}
            >
              <span className="competition-hub-step-index" aria-hidden>
                {connections.length ? <Icon name="check" size={14} /> : "1"}
              </span>
              <div className="competition-hub-step-copy">
                <strong>平台连接</strong>
                <span>{connections.length} 条 · {activeConnections} 活动</span>
              </div>
            </button>
          </li>
          <li aria-hidden className="competition-hub-step-join">
            <Icon name="arrowRight" size={14} />
          </li>
          <li>
            <button
              type="button"
              className={`competition-hub-step${section === "competitions" ? " on" : ""}${competitions.length ? " done" : ""}`}
              aria-current={section === "competitions" ? "step" : undefined}
              onClick={() => setSection("competitions")}
            >
              <span className="competition-hub-step-index" aria-hidden>
                {competitions.length ? <Icon name="check" size={14} /> : "2"}
              </span>
              <div className="competition-hub-step-copy">
                <strong>登记比赛</strong>
                <span>{competitions.length} 场 · {runningCount} 调度中</span>
              </div>
            </button>
          </li>
          <li aria-hidden className="competition-hub-step-join">
            <Icon name="arrowRight" size={14} />
          </li>
          <li>
            <button
              type="button"
              className={`competition-hub-step competition-hub-step-go${workspaceTarget ? "" : " disabled"}`}
              onClick={openWorkspace}
              disabled={!workspaceTarget}
            >
              <span className="competition-hub-step-index" aria-hidden>3</span>
              <div className="competition-hub-step-copy">
                <strong>进入工作区</strong>
                <span>
                  {workspaceTarget
                    ? `打开「${workspaceTarget.title || workspaceTarget.external_competition_id}」`
                    : "同步题目、调度 Run、跟踪提交"}
                </span>
              </div>
              {workspaceTarget ? <Icon name="arrowRight" size={14} className="competition-hub-step-arrow" /> : null}
            </button>
          </li>
        </ol>
      </nav>

      {loadError ? <div className="competition-hub-feedback bad" role="alert">
        <span>{loadError}。已显示的数据保留至下次成功刷新。</span>
        <Button size="sm" variant="secondary" onClick={() => void load()}>重试读取</Button>
      </div> : null}
      {feedback ? (
        <div className={`competition-hub-feedback ${feedback.kind}`} role={feedback.kind === "bad" ? "alert" : "status"}>
          <Icon name={feedback.kind === "ok" ? "checkCircle" : "circleAlert"} size={14} />
          <span>{feedback.detail}</span>
        </div>
      ) : null}

      {section === "competitions" ? (
        <section className="competition-hub-panel" aria-label="比赛列表">
          <div className="competition-hub-panel-head">
            <div>
              <div className="competition-hub-panel-title">
                <h2>我的比赛</h2>
                <Chip size="sm" variant="soft">{competitions.length}</Chip>
              </div>
              <p>每场比赛对应一个远端赛事，进入工作区后可同步题目并调度 Run。</p>
            </div>
            {!connections.length ? (
              <Button variant="outline" onClick={() => setSection("connections")}>
                先去添加连接
              </Button>
            ) : null}
          </div>

          {competitionsLoading ? (
            <div className="competition-hub-empty">
              <Icon name="loader" size={18} />
              <strong>正在读取比赛列表</strong>
            </div>
          ) : null}

          {!competitionsLoading && competitions.length ? (
            <div className="competition-card-grid">
              {competitions.map((item) => {
                const scheduler = SCHEDULER_LABELS[item.scheduler_state] ?? {
                  label: item.scheduler_state,
                  tone: "muted" as const,
                };
                const chipTone: Tone = scheduler.tone === "ok" ? "success" : scheduler.tone === "warn" ? "warning" : "neutral";
                const connection = connections.find((row) => row.connection_id === item.connection_id);
                const meta = connection ? platformMeta(connection.platform_kind, platforms) : null;
                return (
                  <Card key={item.competition_id} className="competition-card">
                    <CardHeader className="competition-card-header">
                      <div className="competition-card-icon">
                        <Icon name="trophy" size={18} />
                      </div>
                      <div className="competition-card-copy">
                        <CardTitle>{item.title || item.external_competition_id}</CardTitle>
                        <CardDescription>
                          {meta ? `${meta.label} · ${connection?.account_key}` : item.external_competition_id}
                        </CardDescription>
                      </div>
                      <Chip
                        size="sm"
                        variant="soft"
                        tone={chipTone}
                        className={`competition-chip-${scheduler.tone}`}
                      >
                        <StatusDot tone={scheduler.tone} />
                        {scheduler.label}
                      </Chip>
                    </CardHeader>
                    <CardBody className="competition-card-meta">
                      <div>
                        <span>外部 ID</span>
                        <strong>{item.external_competition_id}</strong>
                      </div>
                      <div>
                        <span>平台地址</span>
                        <strong>{connection?.canonical_base_url || "—"}</strong>
                      </div>
                    </CardBody>
                    <CardFooter className="competition-card-actions">
                      <Button
                        variant="danger-soft"
                        className="danger"
                        onClick={() => {
                          setPendingCompetitionId(item.competition_id);
                          setConfirmAction("competition");
                        }}
                      >
                        移除
                      </Button>
                      <Button
                        variant="primary"
                        className="primary"
                        onClick={() => router.push(`/competitions/${encodeURIComponent(item.competition_id)}`)}
                      >
                        打开工作区
                        <Icon name="arrowRight" size={14} />
                      </Button>
                    </CardFooter>
                  </Card>
                );
              })}
            </div>
          ) : null}

          {!competitionsLoading && !competitions.length && !loadError ? (
            <div className="competition-hub-empty">
              <Icon name="trophy" size={22} />
              <strong>还没有登记比赛</strong>
              <span>
                {connections.length
                  ? "选择一条平台连接，填写远端比赛 ID 后即可开始同步与调度。"
                  : "请先在「平台连接」中添加 CTFd、rCTF、GZCTF 或本地模拟连接。"}
              </span>
              {connections.length ? (
                <Button variant="primary" className="primary" onClick={() => beginNewCompetition()}>
                  登记比赛
                </Button>
              ) : (
                <Button variant="primary" className="primary" onClick={() => setSection("connections")}>
                  去添加连接
                </Button>
              )}
            </div>
          ) : null}
        </section>
      ) : (
        <section className="competition-hub-panel" aria-label="平台连接">
          <div className="competition-hub-panel-head">
            <div>
              <div className="competition-hub-panel-title">
                <h2>平台连接</h2>
                <Chip size="sm" variant="soft">{connections.length}</Chip>
              </div>
              <p>
                保存平台地址与账户标识；凭据以 Secret 引用存储，页面不回显原文。
                已配置 {credentialCount} / {connections.length} 条。
              </p>
            </div>
          </div>

          {connectionsLoading ? (
            <div className="competition-hub-empty">
              <Icon name="loader" size={18} />
              <strong>正在读取平台连接</strong>
            </div>
          ) : null}

          {!connectionsLoading && connections.length ? (
            <div className="competition-card-grid">
              {connections.map((item) => {
                const meta = platformMeta(item.platform_kind, platforms);
                const status = STATUS_LABELS[item.status] ?? { label: item.status, tone: "muted" as const };
                const statusChipTone: Tone = status.tone === "ok" ? "success" : status.tone === "warn" ? "warning" : status.tone === "bad" ? "danger" : "neutral";
                const bound = competitions.filter((row) => row.connection_id === item.connection_id);
                return (
                  <Card key={item.connection_id} className="competition-card">
                    <CardHeader className="competition-card-header">
                      <div className="competition-card-icon">
                        <Icon name={meta.icon} size={18} />
                      </div>
                      <div className="competition-card-copy">
                        <CardTitle>{meta.label} · {item.account_key}</CardTitle>
                        <CardDescription>{item.canonical_base_url || "未填写地址"}</CardDescription>
                      </div>
                    </CardHeader>
                    <CardBody className="competition-card-tags">
                      <Chip size="sm" variant="soft" tone={statusChipTone} className={`competition-chip-${status.tone}`}>
                        <StatusDot tone={status.tone} />
                        {status.label}
                      </Chip>
                      <Chip size="sm" variant="soft" tone={item.has_credential ? "success" : "warning"} className={item.has_credential ? "competition-chip-ok" : "competition-chip-warn"}>
                        <StatusDot tone={item.has_credential ? "ok" : "warn"} />
                        {item.has_credential ? "已配凭据" : "未配凭据"}
                      </Chip>
                      {bound.length ? (
                        <Chip size="sm" variant="soft">
                          {bound.length} 场比赛
                        </Chip>
                      ) : null}
                    </CardBody>
                    {item.platform_kind === "tsecbench" ? (
                      <CardBody className="competition-vpn-command">
                        <span>先在终端连接当前测评批次的 VPN</span>
                        <code>{TSEC_VPN_COMMAND}</code>
                      </CardBody>
                    ) : null}
                    <CardFooter className="competition-card-actions">
                      <Button
                        variant="ghost"
                        disabled={probing === item.connection_id}
                        onClick={() => void probe(item.connection_id)}
                      >
                        {probing === item.connection_id
                          ? (item.platform_kind === "tsecbench" ? "检查中…" : "测试中…")
                          : (item.platform_kind === "tsecbench" ? "检查 VPN 连接" : "测试")}
                      </Button>
                      <Button variant="outline" onClick={() => openConnectionDetail(item.connection_id)}>
                        凭据与管理
                      </Button>
                      <Button variant="primary" className="primary" onClick={() => beginNewCompetition(item.connection_id)}>
                        登记比赛
                      </Button>
                    </CardFooter>
                  </Card>
                );
              })}
            </div>
          ) : null}

          {!connectionsLoading && !connections.length && !loadError ? (
            <div className="competition-hub-empty">
              <Icon name="plug" size={22} />
              <strong>还没有平台连接</strong>
              <span>添加 CTFd、rCTF、GZCTF、浏览器会话或本地模拟连接。本地模拟可不填凭据。</span>
              <Button variant="primary" className="primary" onClick={beginNewConnection}>
                添加连接
              </Button>
            </div>
          ) : null}
        </section>
      )}

      <Dialog
        open={modal === "new-connection"}
        onOpenChange={(open) => { if (!open) closeModal(); }}
        dismissable={!busy}
        size="lg"
        title="添加平台连接"
        description="保存平台地址和账户标识。一次性凭据提交后转为 Secret 引用。"
        footer={<>
          <Button variant="ghost" disabled={busy} onClick={closeModal}>取消</Button>
          <Button type="submit" form="competition-connection-form" variant="primary" loading={busy} disabled={!newUrl.trim() || !newAccount.trim()}>
            创建连接
          </Button>
        </>}
      >
        <form
          id="competition-connection-form"
          noValidate
          className="grid grid-cols-1 gap-3 sm:grid-cols-2"
          onSubmit={(event) => void submitConnection(event)}
        >
          <div>
            <Label htmlFor="competition-new-kind">平台类型</Label>
            <NativeSelect id="competition-new-kind" data-autofocus value={newKind} onChange={(event) => setNewKind(event.target.value)}>
              {platforms.map((item) => (
                <option key={item.value} value={item.value}>{item.label}</option>
              ))}
            </NativeSelect>
          </div>
          <TextField
            label="平台地址"
            required
            value={newUrl}
            placeholder={newKind === "mock" ? "mock://local" : "https://ctf.example.com"}
            onChange={(event) => setNewUrl(event.target.value)}
          />
          <TextField
            label="账户标识"
            required
            value={newAccount}
            placeholder="用户名或本地团队名"
            onChange={(event) => setNewAccount(event.target.value)}
          />
          <TextField
            label="一次性凭据（可选）"
            type="password"
            agentSecret
            autoComplete="new-password"
            value={newCredential}
            placeholder={newKind === "tsecbench" ? "BENCHMARK_TOKEN" : "提交后转换为 Secret 引用"}
            onChange={(event) => setNewCredential(event.target.value)}
          />
          {newKind === "tsecbench" ? (
            <div className="competition-vpn-command sm:col-span-2">
              <span>下载当前测评批次的 VPN 配置后，在终端执行</span>
              <code>{TSEC_VPN_COMMAND}</code>
            </div>
          ) : null}
          <TextField
            containerClassName="sm:col-span-2"
            label="已有 Secret 引用（可选）"
            agentSecret
            value={newCredentialRef}
            placeholder="secret://…"
            autoComplete="off"
            onChange={(event) => setNewCredentialRef(event.target.value)}
          />
        </form>
      </Dialog>

      <Dialog
        open={modal === "new-competition"}
        onOpenChange={(open) => { if (!open) closeModal(); }}
        dismissable={!busy}
        size="lg"
        title="登记并首次同步"
        description="所选连接需要处于活动状态。策略会在首次同步受理后立即保存。"
        footer={<>
          <Button variant="ghost" disabled={busy} onClick={closeModal}>取消</Button>
          <Button type="submit" form="competition-register-form" variant="primary" loading={busy} disabled={!registerReady}>
            登记并同步
          </Button>
        </>}
      >
        <form
          id="competition-register-form"
          noValidate
          className="grid grid-cols-1 gap-3 sm:grid-cols-2"
          onSubmit={(event) => void submitCompetition(event)}
        >
          <div className="sm:col-span-2">
            <Label htmlFor="competition-register-connection">平台连接</Label>
            <NativeSelect
              id="competition-register-connection"
              data-autofocus
              required
              value={registerConnectionId}
              onChange={(event) => setRegisterConnectionId(event.target.value)}
            >
              <option value="" disabled>选择平台连接</option>
              {connections.map((item) => (
                <option key={item.connection_id} value={item.connection_id}>
                  {platformMeta(item.platform_kind, platforms).label} · {item.canonical_base_url} · {item.account_key}
                </option>
              ))}
            </NativeSelect>
          </div>
          <TextField
            label="远端比赛 ID"
            required
            value={externalId}
            placeholder="远端比赛 id"
            onChange={(event) => setExternalId(event.target.value)}
          />
          <TextField
            label="标题（可选）"
            value={title}
            placeholder="本机显示名"
            onChange={(event) => setTitle(event.target.value)}
          />
          <div>
            <Label htmlFor="competition-register-automation">自动化策略</Label>
            <NativeSelect id="competition-register-automation" value={automationMode} onChange={(event) => setAutomationMode(event.target.value)}>
              <option value="observe">仅观察，不提交</option>
              <option value="assisted">审批后提交</option>
              <option value="autonomous">符合 Gate 和预算时自动提交</option>
            </NativeSelect>
          </div>
          <TextField
            label="最大并发 Run"
            type="number"
            min={1}
            max={64}
            value={maxConcurrentRuns}
            onChange={(event) => setMaxConcurrentRuns(Number(event.target.value) || 1)}
          />
          {registerConnectionId && connections.find((item) => item.connection_id === registerConnectionId)?.status !== "active" ? (
            <p className="text-cx-xs leading-5 text-cx-warning sm:col-span-2">请先测试所选连接，等状态变为活动后再登记。</p>
          ) : null}
        </form>
      </Dialog>

      <Dialog
        open={modal === "connection-detail" && Boolean(detailConnection)}
        onOpenChange={(open) => { if (!open) closeModal(); }}
        dismissable={!busy}
        size="lg"
        title={detailConnection ? `${platformMeta(detailConnection.platform_kind, platforms).label} · ${detailConnection.account_key}` : undefined}
        description={detailConnection ? (detailConnection.canonical_base_url || "尚未填写平台地址") : undefined}
        footer={detailConnection ? <>
          <Button
            variant="danger-soft"
            agentConfirm="destructive"
            className="mr-auto"
            disabled={busy || boundCompetitions.length > 0}
            onClick={() => setConfirmAction("connection")}
          >
            移除连接
          </Button>
          <Button variant="ghost" onClick={closeModal}>关闭</Button>
        </> : undefined}
      >
        {detailConnection ? (
          <div className="competition-detail-body">
            <div className="competition-readonly-grid">
              <div>
                <span>连接状态</span>
                <strong className={STATUS_LABELS[detailConnection.status]?.tone || "muted"}>
                  {STATUS_LABELS[detailConnection.status]?.label || detailConnection.status}
                </strong>
              </div>
              <div>
                <span>绑定比赛</span>
                <strong>{boundCompetitions.length}</strong>
              </div>
              <div>
                <span>最近错误</span>
                <strong>{detailConnection.last_error || "无"}</strong>
              </div>
            </div>

            <div className="competition-cap-row">
              {Object.keys(detailConnection.capabilities ?? {}).length
                ? CAP_LABELS.map(([key, label]) => (
                  <span key={key} className={detailConnection.capabilities?.[key] ? "on" : ""}>
                    {label}{detailConnection.capabilities?.[key] ? "" : "（不支持）"}
                  </span>
                ))
                : <span className="empty">
                  尚未探测能力。{detailConnection.platform_kind === "tsecbench" ? "请从连接卡片检查 VPN 连接。" : "点击测试连接后回填。"}
                </span>}
            </div>

            {detailConnection.platform_kind === "tsecbench" ? (
              <div className="competition-vpn-command">
                <span>OpenVPN 由用户在终端管理</span>
                <code>{TSEC_VPN_COMMAND}</code>
              </div>
            ) : null}

            {probeFields(detailConnection).length ? (
              <dl className="connection-probe-details">
                {probeFields(detailConnection).map((field) => (
                  <div key={field.key}>
                    <dt>{field.label}</dt>
                    <dd>{field.value}</dd>
                  </div>
                ))}
              </dl>
            ) : null}

            <section className="competition-secret-card">
              <header>
                <div>
                  <h3>平台凭据</h3>
                  <p>轮换会让旧引用失效；更新只改引用里的值。</p>
                </div>
                {secretsUnavailable ? <Badge tone="warning">读取失败</Badge> : selectedSecret ? (
                  <Badge tone="success">有效引用</Badge>
                ) : (
                  <Badge tone="warning">未配置</Badge>
                )}
              </header>
              {selectedSecret ? (
                <div className="competition-secret-meta">
                  <code>{selectedSecret.reference}</code>
                  <small>
                    {selectedSecret.key} · 轮换代 {selectedSecret.rotation || 1} · 到期 {selectedSecret.expires_at || "未设置"}
                  </small>
                </div>
              ) : null}
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <div>
                  <Label htmlFor="competition-secret-mode">操作</Label>
                  <NativeSelect id="competition-secret-mode" value={secretMode} onChange={(event) => setSecretMode(event.target.value as "rotate" | "update")}>
                    <option value="rotate">轮换（旧引用失效）</option>
                    <option value="update">更新（引用保持）</option>
                  </NativeSelect>
                </div>
                <TextField label="到期时间（可选）" type="datetime-local" value={expiresAt} onChange={(event) => setExpiresAt(event.target.value)} />
                <div className="flex flex-col gap-2 sm:col-span-2">
                  <TextField
                    label="一次性凭据"
                    type={showSecret ? "text" : "password"}
                    agentSecret
                    value={secretValue}
                    autoComplete="new-password"
                    placeholder="Token、Cookie 或平台密码"
                    onChange={(event) => setSecretValue(event.target.value)}
                  />
                  <Button size="sm" variant="ghost" className="self-start" aria-pressed={showSecret} onClick={() => setShowSecret((value) => !value)}>
                    {showSecret ? "隐藏凭据" : "显示凭据"}
                  </Button>
                </div>
              </div>
              <div className="competition-editor-actions">
                <Button variant="primary" disabled={busy || !secretValue} onClick={() => void submitSecret()}>
                  {secretMode === "rotate" ? "轮换凭据" : "更新凭据"}
                </Button>
                <Button
                  variant="danger-soft"
                  agentConfirm="destructive"
                  disabled={busy || !selectedSecret}
                  onClick={() => setConfirmAction("secret")}
                >
                  撤销凭据
                </Button>
              </div>
            </section>

            {detailConnection.platform_kind === "generic_browser" ? (
              <section className="competition-secret-card">
                <header>
                  <div>
                    <h3>浏览器会话</h3>
                    <p>导入 Playwright storage state。</p>
                  </div>
                  <Badge tone={browserSession?.present ? (browserSession.expired ? "warning" : "success") : "neutral"}>
                    {browserSession?.present ? (browserSession.expired ? "会话已过期" : "会话有效") : "尚未导入"}
                  </Badge>
                </header>
                <TextArea
                  label="storage state JSON"
                  agentSecret
                  className="font-mono text-cx-xs"
                  rows={5}
                  value={storageState}
                  onChange={(event) => setStorageState(event.target.value)}
                />
                <div className="competition-editor-actions">
                  <Button variant="primary" disabled={busy} onClick={() => void importBrowser(false)}>导入</Button>
                  <Button variant="secondary" disabled={busy || !browserSession?.present} onClick={() => void importBrowser(true)}>续期</Button>
                  <Button variant="danger-soft" agentConfirm="destructive" disabled={busy || !browserSession?.present} onClick={() => setConfirmAction("browser")}>撤销会话</Button>
                </div>
              </section>
            ) : null}

            {boundCompetitions.length ? (
              <section className="competition-bound-list">
                <h3>使用此连接的比赛</h3>
                {boundCompetitions.map((row) => (
                  <Button
                    key={row.competition_id}
                    variant="outline"
                    onClick={() => {
                      closeModal();
                      setSection("competitions");
                      router.push(`/competitions/${encodeURIComponent(row.competition_id)}`);
                    }}
                  >
                    <strong>{row.title || row.external_competition_id}</strong>
                    <span>{SCHEDULER_LABELS[row.scheduler_state]?.label || row.scheduler_state}</span>
                  </Button>
                ))}
              </section>
            ) : null}

            {boundCompetitions.length ? (
              <p className="text-cx-xs leading-5 text-cx-warning">
                此连接仍绑定 {boundCompetitions.length} 场比赛，请先从「我的比赛」移除这些比赛。
              </p>
            ) : null}
          </div>
        ) : null}
      </Dialog>

      <Dialog
        open={confirmAction !== null}
        onOpenChange={(open) => { if (!open && !busy) setConfirmAction(null); }}
        dismissable={!busy}
        size="sm"
        tone="danger"
        title={confirmAction === "browser"
          ? "撤销浏览器会话"
          : confirmAction === "competition"
            ? "从清单移除比赛"
            : confirmAction === "connection"
              ? "从清单移除连接"
              : "撤销当前连接凭据"}
        description={confirmAction === "browser"
          ? "浏览器 storage state 将从本地安全存储中删除。"
          : confirmAction === "competition"
            ? "只从本地清单隐藏，不会删除远端平台比赛。"
            : confirmAction === "connection"
              ? "只从本地清单隐藏；关联凭据与浏览器会话将一并清理。"
              : "撤销后，依赖该引用的同步和提交将无法继续认证。"}
        footer={<>
          <Button variant="ghost" disabled={busy} onClick={() => setConfirmAction(null)}>取消</Button>
          <Button
            variant="danger"
            agentConfirm="destructive"
            loading={busy}
            onClick={() => void (
              confirmAction === "browser"
                ? confirmRevokeBrowser()
                : confirmAction === "competition"
                  ? confirmUnregisterCompetition()
                  : confirmAction === "connection"
                    ? confirmUnregisterConnection()
                    : confirmRevokeSecret()
            )}
          >
            {confirmAction === "competition" || confirmAction === "connection" ? "移除" : "撤销"}
          </Button>
        </>}
      >
        <div className="action-dialog-summary">
          <strong>
            {confirmAction === "competition"
              ? (pendingCompetition?.title || pendingCompetition?.external_competition_id)
              : confirmAction === "connection"
                ? `${platformMeta(detailConnection?.platform_kind || "", platforms).label} · ${detailConnection?.account_key || ""}`
                : (detailConnection?.account_key || detailConnection?.connection_id)}
          </strong>
        </div>
      </Dialog>
    </div>
  );
}
