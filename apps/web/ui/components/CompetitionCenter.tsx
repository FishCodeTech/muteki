"use client";

import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { useRouter } from "next/navigation";
import {
  Button,
  Card,
  Chip,
  Input,
  ListBox,
  ListBoxItem,
  Modal,
  Select,
  TextArea,
} from "@heroui/react";
import { Icon, type IconName } from "@/components/Icon";
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
  const [connections, setConnections] = useState<ConnectionView[]>([]);
  const [competitions, setCompetitions] = useState<CompetitionInfo[]>([]);
  const [platforms, setPlatforms] = useState(FALLBACK_PLATFORMS);
  const [secrets, setSecrets] = useState<PlatformSecretMetadata[]>([]);
  const [browserSession, setBrowserSession] = useState<BrowserSessionStatus | null>(null);
  const [section, setSection] = useState<Section>("competitions");
  const [modal, setModal] = useState<ModalKind>(null);
  const [detailConnectionId, setDetailConnectionId] = useState("");
  const [loading, setLoading] = useState(true);
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
  }, [detailConnectionId, modal]);
  const [expiresAt, setExpiresAt] = useState("");
  const [storageState, setStorageState] = useState(EMPTY_STORAGE);

  const load = useCallback(async () => {
    try {
      const results = await Promise.allSettled([
        fetchConnections(), fetchCompetitions(), fetchPlatformSecrets(), fetchPlatformKinds(),
      ]);
      const [connectionsResult, competitionsResult, secretsResult, kindsResult] = results;
      if (connectionsResult.status === "fulfilled") {
        setConnections(connectionsResult.value);
        setRegisterConnectionId((current) => current || connectionsResult.value[0]?.connection_id || "");
      }
      if (competitionsResult.status === "fulfilled") setCompetitions(competitionsResult.value);
      if (secretsResult.status === "fulfilled") setSecrets(secretsResult.value);
      setSecretsUnavailable(secretsResult.status === "rejected");
      if (kindsResult.status === "fulfilled") setPlatforms(kindsToPlatforms(kindsResult.value));
      const labels = ["平台连接", "比赛列表", "凭据状态", "平台类型"];
      setLoadError(results.flatMap((result, index) => result.status === "rejected"
        ? [`${labels[index]}读取失败：${result.reason instanceof Error ? result.reason.message : String(result.reason)}`]
        : []).join("；"));
    } catch (exc) {
      setFeedback({ kind: "bad", detail: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const focus = params.get("focus");
    if (focus === "credentials" || focus === "connections") {
      focusCredentials.current = true;
      setSection("connections");
    }
    void load();
  }, [load]);

  useEffect(() => {
    if (!focusCredentials.current || loading || !connections.length) return;
    focusCredentials.current = false;
    openConnectionDetail(connections[0].connection_id);
  }, [loading, connections]);

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
          <Button variant="ghost" onPress={() => void load()} isDisabled={loading}>
            <Icon name="refresh" size={14} />
            刷新
          </Button>
          {section === "connections" ? (
            <Button className="primary" onPress={beginNewConnection}>
              <Icon name="plus" size={14} />
              添加连接
            </Button>
          ) : (
            <Button
              className="primary"
              onPress={() => beginNewCompetition()}
              isDisabled={!connections.length}
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
        <Button size="sm" onPress={() => void load()}>重试读取</Button>
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
                <Chip size="sm" variant="secondary">{competitions.length}</Chip>
              </div>
              <p>每场比赛对应一个远端赛事，进入工作区后可同步题目并调度 Run。</p>
            </div>
            {!connections.length ? (
              <Button variant="outline" onPress={() => setSection("connections")}>
                先去添加连接
              </Button>
            ) : null}
          </div>

          {loading ? (
            <div className="competition-hub-empty">
              <Icon name="loader" size={18} />
              <strong>正在读取比赛列表</strong>
            </div>
          ) : null}

          {!loading && competitions.length ? (
            <div className="competition-card-grid">
              {competitions.map((item) => {
                const scheduler = SCHEDULER_LABELS[item.scheduler_state] ?? {
                  label: item.scheduler_state,
                  tone: "muted" as const,
                };
                const connection = connections.find((row) => row.connection_id === item.connection_id);
                const meta = connection ? platformMeta(connection.platform_kind, platforms) : null;
                return (
                  <Card key={item.competition_id} className="competition-card">
                    <Card.Header className="competition-card-header">
                      <div className="competition-card-icon">
                        <Icon name="trophy" size={18} />
                      </div>
                      <div className="competition-card-copy">
                        <Card.Title>{item.title || item.external_competition_id}</Card.Title>
                        <Card.Description>
                          {meta ? `${meta.label} · ${connection?.account_key}` : item.external_competition_id}
                        </Card.Description>
                      </div>
                      <Chip
                        size="sm"
                        variant="secondary"
                        className={`competition-chip-${scheduler.tone}`}
                      >
                        <StatusDot tone={scheduler.tone} />
                        {scheduler.label}
                      </Chip>
                    </Card.Header>
                    <Card.Content className="competition-card-meta">
                      <div>
                        <span>外部 ID</span>
                        <strong>{item.external_competition_id}</strong>
                      </div>
                      <div>
                        <span>平台地址</span>
                        <strong>{connection?.canonical_base_url || "—"}</strong>
                      </div>
                    </Card.Content>
                    <Card.Footer className="competition-card-actions">
                      <Button
                        variant="ghost"
                        className="danger"
                        onPress={() => {
                          setPendingCompetitionId(item.competition_id);
                          setConfirmAction("competition");
                        }}
                      >
                        移除
                      </Button>
                      <Button
                        className="primary"
                        onPress={() => router.push(`/competitions/${encodeURIComponent(item.competition_id)}`)}
                      >
                        打开工作区
                        <Icon name="arrowRight" size={14} />
                      </Button>
                    </Card.Footer>
                  </Card>
                );
              })}
            </div>
          ) : null}

          {!loading && !competitions.length ? (
            <div className="competition-hub-empty">
              <Icon name="trophy" size={22} />
              <strong>还没有登记比赛</strong>
              <span>
                {connections.length
                  ? "选择一条平台连接，填写远端比赛 ID 后即可开始同步与调度。"
                  : "请先在「平台连接」中添加 CTFd、rCTF、GZCTF 或本地模拟连接。"}
              </span>
              {connections.length ? (
                <Button className="primary" onPress={() => beginNewCompetition()}>
                  登记比赛
                </Button>
              ) : (
                <Button className="primary" onPress={() => setSection("connections")}>
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
                <Chip size="sm" variant="secondary">{connections.length}</Chip>
              </div>
              <p>
                保存平台地址与账户标识；凭据以 Secret 引用存储，页面不回显原文。
                已配置 {credentialCount} / {connections.length} 条。
              </p>
            </div>
          </div>

          {loading ? (
            <div className="competition-hub-empty">
              <Icon name="loader" size={18} />
              <strong>正在读取平台连接</strong>
            </div>
          ) : null}

          {!loading && connections.length ? (
            <div className="competition-card-grid">
              {connections.map((item) => {
                const meta = platformMeta(item.platform_kind, platforms);
                const status = STATUS_LABELS[item.status] ?? { label: item.status, tone: "muted" as const };
                const bound = competitions.filter((row) => row.connection_id === item.connection_id);
                return (
                  <Card key={item.connection_id} className="competition-card">
                    <Card.Header className="competition-card-header">
                      <div className="competition-card-icon">
                        <Icon name={meta.icon} size={18} />
                      </div>
                      <div className="competition-card-copy">
                        <Card.Title>{meta.label} · {item.account_key}</Card.Title>
                        <Card.Description>{item.canonical_base_url || "未填写地址"}</Card.Description>
                      </div>
                    </Card.Header>
                    <Card.Content className="competition-card-tags">
                      <Chip size="sm" variant="secondary" className={`competition-chip-${status.tone}`}>
                        <StatusDot tone={status.tone} />
                        {status.label}
                      </Chip>
                      <Chip size="sm" variant="secondary" className={item.has_credential ? "competition-chip-ok" : "competition-chip-warn"}>
                        <StatusDot tone={item.has_credential ? "ok" : "warn"} />
                        {item.has_credential ? "已配凭据" : "未配凭据"}
                      </Chip>
                      {bound.length ? (
                        <Chip size="sm" variant="secondary">
                          {bound.length} 场比赛
                        </Chip>
                      ) : null}
                    </Card.Content>
                    {item.platform_kind === "tsecbench" ? (
                      <Card.Content className="competition-vpn-command">
                        <span>先在终端连接当前测评批次的 VPN</span>
                        <code>{TSEC_VPN_COMMAND}</code>
                      </Card.Content>
                    ) : null}
                    <Card.Footer className="competition-card-actions">
                      <Button
                        variant="ghost"
                        isDisabled={probing === item.connection_id}
                        onPress={() => void probe(item.connection_id)}
                      >
                        {probing === item.connection_id
                          ? (item.platform_kind === "tsecbench" ? "检查中…" : "测试中…")
                          : (item.platform_kind === "tsecbench" ? "检查 VPN 连接" : "测试")}
                      </Button>
                      <Button variant="outline" onPress={() => openConnectionDetail(item.connection_id)}>
                        凭据与管理
                      </Button>
                      <Button className="primary" onPress={() => beginNewCompetition(item.connection_id)}>
                        登记比赛
                      </Button>
                    </Card.Footer>
                  </Card>
                );
              })}
            </div>
          ) : null}

          {!loading && !connections.length ? (
            <div className="competition-hub-empty">
              <Icon name="plug" size={22} />
              <strong>还没有平台连接</strong>
              <span>添加 CTFd、rCTF、GZCTF、浏览器会话或本地模拟连接。本地模拟可不填凭据。</span>
              <Button className="primary" onPress={beginNewConnection}>
                添加连接
              </Button>
            </div>
          ) : null}
        </section>
      )}

      {/* 新建连接 */}
      <Modal isOpen={modal === "new-connection"} onOpenChange={(open) => { if (!open) closeModal(); }}>
        <Modal.Backdrop isDismissable={!busy}>
          <Modal.Container size="lg">
            <Modal.Dialog>
              <form noValidate onSubmit={(event) => void submitConnection(event)}>
                <Modal.Header>
                  <Modal.Heading>添加平台连接</Modal.Heading>
                  <p className="competition-modal-lead">
                    保存平台地址和账户标识。一次性凭据提交后转为 Secret 引用。
                  </p>
                </Modal.Header>
                <Modal.Body className="competition-form-grid">
                  <label>平台类型
                    <Select aria-label="平台类型" selectedKey={newKind} onSelectionChange={(key) => setNewKind(String(key))}>
                      <Select.Trigger><Select.Value /></Select.Trigger>
                      <Select.Popover>
                        <ListBox>
                          {platforms.map((item) => (
                            <ListBoxItem key={item.value} id={item.value} textValue={item.label}>{item.label}</ListBoxItem>
                          ))}
                        </ListBox>
                      </Select.Popover>
                    </Select>
                  </label>
                  <label>平台地址
                    <Input
                      required
                      value={newUrl}
                      placeholder={newKind === "mock" ? "mock://local" : "https://ctf.example.com"}
                      onChange={(event) => setNewUrl(event.target.value)}
                    />
                  </label>
                  <label>账户标识
                    <Input
                      required
                      value={newAccount}
                      placeholder="用户名或本地团队名"
                      onChange={(event) => setNewAccount(event.target.value)}
                    />
                  </label>
                  <label>一次性凭据（可选）
                    <Input
                      type="password"
                      autoComplete="new-password"
                      value={newCredential}
                      placeholder={newKind === "tsecbench" ? "BENCHMARK_TOKEN" : "提交后转换为 Secret 引用"}
                      onChange={(event) => setNewCredential(event.target.value)}
                    />
                  </label>
                  {newKind === "tsecbench" ? (
                    <div className="competition-vpn-command wide">
                      <span>下载当前测评批次的 VPN 配置后，在终端执行</span>
                      <code>{TSEC_VPN_COMMAND}</code>
                    </div>
                  ) : null}
                  <label className="wide">已有 Secret 引用（可选）
                    <Input
                      value={newCredentialRef}
                      placeholder="secret://…"
                      autoComplete="off"
                      onChange={(event) => setNewCredentialRef(event.target.value)}
                    />
                  </label>
                </Modal.Body>
                <Modal.Footer>
                  <Button variant="ghost" isDisabled={busy} onPress={closeModal}>取消</Button>
                  <Button type="submit" className="primary" isDisabled={busy || !newUrl.trim() || !newAccount.trim()}>
                    {busy ? "提交中…" : "创建连接"}
                  </Button>
                </Modal.Footer>
              </form>
            </Modal.Dialog>
          </Modal.Container>
        </Modal.Backdrop>
      </Modal>

      {/* 登记比赛 */}
      <Modal isOpen={modal === "new-competition"} onOpenChange={(open) => { if (!open) closeModal(); }}>
        <Modal.Backdrop isDismissable={!busy}>
          <Modal.Container size="lg">
            <Modal.Dialog>
              <form noValidate onSubmit={(event) => void submitCompetition(event)}>
                <Modal.Header>
                  <Modal.Heading>登记并首次同步</Modal.Heading>
                  <p className="competition-modal-lead">所选连接需要处于活动状态。策略会在首次同步受理后立即保存。</p>
                </Modal.Header>
                <Modal.Body className="competition-form-grid">
                  <label className="wide">平台连接
                    <Select
                      aria-label="平台连接"
                      isRequired
                      selectedKey={registerConnectionId}
                      onSelectionChange={(key) => setRegisterConnectionId(String(key ?? ""))}
                    >
                      <Select.Trigger><Select.Value /></Select.Trigger>
                      <Select.Popover>
                        <ListBox>
                          <ListBoxItem id="" textValue="选择平台连接" isDisabled>选择平台连接</ListBoxItem>
                          {connections.map((item) => (
                            <ListBoxItem
                              key={item.connection_id}
                              id={item.connection_id}
                              textValue={`${platformMeta(item.platform_kind, platforms).label} · ${item.canonical_base_url} · ${item.account_key}`}
                            >
                              {platformMeta(item.platform_kind, platforms).label} · {item.canonical_base_url} · {item.account_key}
                            </ListBoxItem>
                          ))}
                        </ListBox>
                      </Select.Popover>
                    </Select>
                  </label>
                  <label>远端比赛 ID
                    <Input
                      required
                      value={externalId}
                      placeholder="远端比赛 id"
                      onChange={(event) => setExternalId(event.target.value)}
                    />
                  </label>
                  <label>标题（可选）
                    <Input
                      value={title}
                      placeholder="本机显示名"
                      onChange={(event) => setTitle(event.target.value)}
                    />
                  </label>
                  <label>自动化策略
                    <Select aria-label="自动化策略" selectedKey={automationMode} onSelectionChange={(key) => setAutomationMode(String(key))}>
                      <Select.Trigger><Select.Value /></Select.Trigger>
                      <Select.Popover>
                        <ListBox>
                          <ListBoxItem id="observe">仅观察，不提交</ListBoxItem>
                          <ListBoxItem id="assisted">审批后提交</ListBoxItem>
                          <ListBoxItem id="autonomous">符合 Gate 和预算时自动提交</ListBoxItem>
                        </ListBox>
                      </Select.Popover>
                    </Select>
                  </label>
                  <label>最大并发 Run
                    <Input
                      type="number"
                      min={1}
                      max={64}
                      value={maxConcurrentRuns}
                      onChange={(event) => setMaxConcurrentRuns(Number(event.target.value) || 1)}
                    />
                  </label>
                </Modal.Body>
                {registerConnectionId && connections.find((item) => item.connection_id === registerConnectionId)?.status !== "active" ? (
                  <p className="competition-editor-hint warn">请先测试所选连接，等状态变为活动后再登记。</p>
                ) : null}
                <Modal.Footer>
                  <Button variant="ghost" isDisabled={busy} onPress={closeModal}>取消</Button>
                  <Button type="submit" className="primary" isDisabled={busy || !registerReady}>
                    {busy ? "提交中…" : "登记并同步"}
                  </Button>
                </Modal.Footer>
              </form>
            </Modal.Dialog>
          </Modal.Container>
        </Modal.Backdrop>
      </Modal>

      {/* 连接详情 / 凭据 */}
      <Modal isOpen={modal === "connection-detail" && Boolean(detailConnection)} onOpenChange={(open) => { if (!open) closeModal(); }}>
        <Modal.Backdrop isDismissable={!busy}>
          <Modal.Container size="lg">
            <Modal.Dialog>
              {detailConnection ? (
                <>
                  <Modal.Header>
                    <Modal.Heading>
                      {platformMeta(detailConnection.platform_kind, platforms).label} · {detailConnection.account_key}
                    </Modal.Heading>
                    <p className="competition-modal-lead">{detailConnection.canonical_base_url || "尚未填写平台地址"}</p>
                  </Modal.Header>
                  <Modal.Body className="competition-detail-body">
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
                        {secretsUnavailable ? <Chip size="sm" className="competition-chip-warn">读取失败</Chip> : selectedSecret ? (
                          <Chip size="sm" className="competition-chip-ok">有效引用</Chip>
                        ) : (
                          <Chip size="sm" className="competition-chip-warn">未配置</Chip>
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
                      <div className="competition-form-grid compact">
                        <label>操作
                          <Select aria-label="凭据操作" selectedKey={secretMode} onSelectionChange={(key) => setSecretMode(key as "rotate" | "update")}>
                            <Select.Trigger><Select.Value /></Select.Trigger>
                            <Select.Popover>
                              <ListBox>
                                <ListBoxItem id="rotate">轮换（旧引用失效）</ListBoxItem>
                                <ListBoxItem id="update">更新（引用保持）</ListBoxItem>
                              </ListBox>
                            </Select.Popover>
                          </Select>
                        </label>
                        <label>到期时间（可选）
                          <Input type="datetime-local" value={expiresAt} onChange={(event) => setExpiresAt(event.target.value)} />
                        </label>
                        <label className="wide">一次性凭据
                          <Input
                            type={showSecret ? "text" : "password"}
                            aria-label="一次性凭据"
                            value={secretValue}
                            autoComplete="new-password"
                            placeholder="Token、Cookie 或平台密码"
                            onChange={(event) => setSecretValue(event.target.value)}
                          />
                          <Button size="sm" variant="ghost" aria-pressed={showSecret} onPress={() => setShowSecret((value) => !value)}>
                            {showSecret ? "隐藏凭据" : "显示凭据"}
                          </Button>
                        </label>
                      </div>
                      <div className="competition-editor-actions">
                        <Button className="primary" isDisabled={busy || !secretValue} onPress={() => void submitSecret()}>
                          {secretMode === "rotate" ? "轮换凭据" : "更新凭据"}
                        </Button>
                        <Button
                          variant="ghost"
                          className="danger"
                          isDisabled={busy || !selectedSecret}
                          onPress={() => setConfirmAction("secret")}
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
                          <Chip
                            size="sm"
                            className={browserSession?.present ? (browserSession.expired ? "competition-chip-warn" : "competition-chip-ok") : ""}
                          >
                            {browserSession?.present ? (browserSession.expired ? "会话已过期" : "会话有效") : "尚未导入"}
                          </Chip>
                        </header>
                        <label className="competition-storage-label">storage state JSON
                          <TextArea
                            className="resize-none"
                            value={storageState}
                            onChange={(event) => setStorageState(event.target.value)}
                          />
                        </label>
                        <div className="competition-editor-actions">
                          <Button className="primary" isDisabled={busy} onPress={() => void importBrowser(false)}>导入</Button>
                          <Button isDisabled={busy || !browserSession?.present} onPress={() => void importBrowser(true)}>续期</Button>
                          <Button variant="ghost" className="danger" isDisabled={busy || !browserSession?.present} onPress={() => setConfirmAction("browser")}>撤销会话</Button>
                        </div>
                      </section>
                    ) : null}

                    {boundCompetitions.length ? (
                      <section className="competition-bound-list">
                        <h3>使用此连接的比赛</h3>
                        {boundCompetitions.map((row) => (
                          <Button
                            key={row.competition_id}
                            variant="ghost"
                            onPress={() => {
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
                  </Modal.Body>
                  <Modal.Footer>
                    <Button
                      variant="ghost"
                      className="danger"
                      isDisabled={busy || boundCompetitions.length > 0}
                      onPress={() => setConfirmAction("connection")}
                    >
                      移除连接
                    </Button>
                    <Button variant="ghost" onPress={closeModal}>关闭</Button>
                  </Modal.Footer>
                  {boundCompetitions.length ? (
                    <p className="competition-editor-hint warn competition-detail-hint">
                      此连接仍绑定 {boundCompetitions.length} 场比赛，请先从「我的比赛」移除这些比赛。
                    </p>
                  ) : null}
                </>
              ) : null}
            </Modal.Dialog>
          </Modal.Container>
        </Modal.Backdrop>
      </Modal>

      {/* 确认对话框 */}
      <Modal isOpen={confirmAction !== null} onOpenChange={(open) => { if (!open && !busy) setConfirmAction(null); }}>
        <Modal.Backdrop isDismissable={!busy}>
          <Modal.Container>
            <Modal.Dialog>
              <Modal.Header className="flex-col items-start gap-1">
                <Modal.Heading>
                  {confirmAction === "browser"
                    ? "撤销浏览器会话"
                    : confirmAction === "competition"
                      ? "从清单移除比赛"
                      : confirmAction === "connection"
                        ? "从清单移除连接"
                        : "撤销当前连接凭据"}
                </Modal.Heading>
                <small className="font-normal text-muted">
                  {confirmAction === "browser"
                    ? "浏览器 storage state 将从本地安全存储中删除。"
                    : confirmAction === "competition"
                      ? "只从本地清单隐藏，不会删除远端平台比赛。"
                      : confirmAction === "connection"
                        ? "只从本地清单隐藏；关联凭据与浏览器会话将一并清理。"
                        : "撤销后，依赖该引用的同步和提交将无法继续认证。"}
                </small>
              </Modal.Header>
              <Modal.Body>
                <div className="action-dialog-summary">
                  <strong>
                    {confirmAction === "competition"
                      ? (pendingCompetition?.title || pendingCompetition?.external_competition_id)
                      : confirmAction === "connection"
                        ? `${platformMeta(detailConnection?.platform_kind || "", platforms).label} · ${detailConnection?.account_key || ""}`
                        : (detailConnection?.account_key || detailConnection?.connection_id)}
                  </strong>
                </div>
              </Modal.Body>
              <Modal.Footer>
                <Button variant="ghost" isDisabled={busy} onPress={() => setConfirmAction(null)}>取消</Button>
                <Button
                  variant="danger"
                  isPending={busy}
                  onPress={() => void (
                    confirmAction === "browser"
                      ? confirmRevokeBrowser()
                      : confirmAction === "competition"
                        ? confirmUnregisterCompetition()
                        : confirmAction === "connection"
                          ? confirmUnregisterConnection()
                          : confirmRevokeSecret()
                  )}
                >
                  确认
                </Button>
              </Modal.Footer>
            </Modal.Dialog>
          </Modal.Container>
        </Modal.Backdrop>
      </Modal>
    </div>
  );
}
