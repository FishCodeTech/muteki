"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  Button,
  Checkbox,
  Dialog,
  IconButton,
  Input,
  Menu,
  MenuItem,
  MenuSeparator,
  Select,
  useContextMenu,
} from "@muteki/ui";
import type { CSSProperties } from "react";
import { apiFetch } from "@/lib/useRun";
import type {
  BoardContribution,
  StatusLabelContribution,
  UiContributions,
} from "@/components/ui-contributions/types";
import {
  ArtifactViewerView,
  BoardView,
  CommandFormView,
  NavigationItems,
  StatusLabel,
} from "@/components/ui-contributions";
import { SchemaForm } from "@/components/ui-contributions/SchemaForm";
import { SettingsSection } from "@/components/settings/primitives";


type ExtensionRecord = {
  extension_id: string;
  origin: string;
  state: string;
  enabled: boolean;
  active_version: string;
  installed_versions: string[];
  config: Record<string, unknown>;
  health: string;
  last_error: string;
  capabilities: Record<string, unknown>;
  installations: Record<string, { verification?: Record<string, unknown>; installed_at?: string; confirmations?: string[] }>;
  registry_entries: { type: string; id: string; state: string; origin?: string; detail?: string; error?: unknown }[];
  isolation: { backend?: string; enforcement?: string; subprocess_policy?: string; resource_limits?: Record<string, number> };
  secret_usage: { reference: string; scope: string; injection: string; expires_at: string }[];
  start_count: number;
  last_exit_code: number | null;
  last_health_at: string | null;
  restart_backoff_seconds: number;
  manual_stop_reason: string;
  created_at: string;
  updated_at: string;
};

type ManifestSummary = {
  id?: string;
  version?: string;
  plugin_version?: string;
  description?: string;
  author?: Record<string, string>;
  homepage?: string;
  plugin_schema?: string;
  client_namespace?: string;
  portable_components?: {
    skills?: string[];
    skills_state?: string;
    mcp_state?: string;
  };
  origin?: string;
  requires_core?: string;
  provides?: { type: string; id: string; api_version: number }[];
  requires?: { capability: string; version: number }[];
  permissions?: {
    filesystem: string[];
    network: string[];
    secrets: string[];
    events_write: string[];
  };
  has_ui?: boolean;
  has_config_schema?: boolean;
  error?: string;
  verification?: Record<string, unknown>;
  registry_state?: string;
};

type OverviewCard = {
  record: ExtensionRecord;
  manifest: ManifestSummary;
  live_health: { status?: string; detail?: string } | null;
  migrations: MigrationReceipt[];
};

type MigrationReceipt = {
  from_version: string | null;
  to_version: string;
  migrated_at: string;
  files: string[];
};

type Detail = {
  record: ExtensionRecord;
  manifest: (ManifestSummary & { entrypoints?: Record<string, unknown> }) | null;
  manifest_error: string;
  config_schema: Record<string, unknown> | null;
  config_schema_error: string;
  migrations: MigrationReceipt[];
  live_health: { status?: string; detail?: string } | null;
};

type Receipt = {
  command_id: string;
  state: string;
  error?: { code: string; message: string } | null;
};

type LogPage = {
  lines: string[];
  total: number;
  offset: number;
  limit: number;
  has_more: boolean;
};

type SourceDraft = {
  kind: string;
  path: string;
  url: string;
  ref: string;
  catalog_root: string;
  extension_id: string;
  version: string;
};

type InstallPreview = {
  preview_id: string;
  extension_id: string;
  version: string;
  manifest: ManifestSummary & { entrypoints?: { command?: string[] } };
  verification: {
    signature?: string;
    publisher?: string;
    immutable?: boolean;
    source?: Record<string, string>;
    phases?: string[];
  };
  changes: {
    source_changed?: boolean;
    publisher_changed?: boolean;
    permission_expansion?: string[];
    ui_changed?: boolean;
    dependencies_changed?: boolean;
  };
  confirmations_required: string[];
  expires_at: string;
  state: string;
};

const EMPTY_SOURCE: SourceDraft = {
  kind: "local-dir",
  path: "",
  url: "",
  ref: "",
  catalog_root: "",
  extension_id: "",
  version: "",
};

const SOURCE_KINDS: [string, string][] = [
  ["local-dir", "本地目录"],
  ["archive", "本地归档"],
  ["git", "Git 仓库"],
  ["http", "HTTP(S) 归档"],
  ["catalog", "Catalog"],
];

const STATE_LABELS: Record<string, { label: string; color: string }> = {
  installed: { label: "已安装", color: "var(--muted)" },
  ready: { label: "就绪", color: "var(--green)" },
  degraded: { label: "降级", color: "var(--amber)" },
  unavailable: { label: "不可用", color: "var(--red)" },
  disabled: { label: "已停用", color: "var(--muted)" },
};

const ORIGIN_LABELS: Record<string, string> = {
  builtin: "内置",
  verified: "已验证",
  community: "社区",
  installed: "本地安装",
};


const page: CSSProperties = {
  minHeight: 0,
  background: "transparent",
  color: "var(--cx-fg)",
  fontFamily: "var(--cx-font-sans)",
};
const inner: CSSProperties = { display: "grid", gap: 32 };
const row: CSSProperties = { display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" };
const muted: CSSProperties = { color: "var(--cx-fg-3)", fontSize: 12 };
const mono: CSSProperties = { fontFamily: "var(--cx-font-mono)", fontSize: 12, color: "var(--cx-fg-2)" };
const meta: CSSProperties = { ...row, gap: "4px 16px", color: "var(--cx-fg-3)", fontSize: 12, lineHeight: 1.5 };
const btn = "cx-ext-btn";
const btnPrimary = "cx-ext-btn cx-ext-btn-primary";
const btnDanger = "cx-ext-btn cx-ext-btn-danger";
const input: CSSProperties = { minWidth: 0 };

const TONES: Record<string, string> = {
  "var(--green)": "success",
  "var(--amber)": "warning",
  "var(--red)": "danger",
};

function badge(color: string, label: string, title?: string) {
  return (
    <span key={label} className="cx-ext-chip" data-tone={TONES[color] ?? "neutral"} data-tooltip={title ?? label}>
      {label}
    </span>
  );
}

function sourcePayload(draft: SourceDraft): Record<string, string> {
  const out: Record<string, string> = { kind: draft.kind };
  for (const key of ["path", "url", "ref", "catalog_root", "extension_id", "version"] as const) {
    if (draft[key].trim()) out[key] = draft[key].trim();
  }
  return out;
}

function receiptError(receipt: Receipt | undefined): string {
  return receipt?.error ? `${receipt.error.code}: ${receipt.error.message}` : "";
}

const CONFIRMATION_LABELS: Record<string, string> = {
  unsigned: "我确认该扩展未提供可验证签名",
  source_change: "我确认本次来源与已安装版本不同",
  publisher_change: "我确认发布者身份发生变化",
  permission_expansion: "我确认本次新增权限",
};

function PreviewPanel({
  value,
  confirmations,
  onConfirmations,
}: {
  value: InstallPreview;
  confirmations: string[];
  onConfirmations: (items: string[]) => void;
}) {
  const permissions = value.manifest.permissions;
  return (
    <div className="cx-ext-preview">
      <div style={row}>
        <strong style={{ ...mono, color: "var(--cx-fg)", fontWeight: 500 }}>{value.extension_id}@{value.version}</strong>
        {badge(value.verification.signature === "unsigned" ? "var(--amber)" : "var(--green)", value.verification.signature || "unknown")}
        {badge(value.verification.immutable ? "var(--green)" : "var(--amber)", value.verification.immutable ? "来源已固定" : "可变来源")}
        {badge("var(--blue)", value.state)}
      </div>
      <dl className="extension-preview-grid">
        <dt>包标准</dt><dd>{value.manifest.plugin_schema ? "Agent Plugins 1.0.0" : "未识别"}</dd>
        <dt>客户端扩展</dt><dd style={mono}>{value.manifest.client_namespace || "无"}</dd>
        <dt>来源</dt><dd style={mono}>{JSON.stringify(value.verification.source || {})}</dd>
        <dt>发布者</dt><dd>{value.verification.publisher || "unknown"}</dd>
        <dt>待启动进程</dt><dd style={mono}>{value.manifest.entrypoints?.command?.join(" ") || "无"}</dd>
        <dt>安装阶段</dt><dd>{value.verification.phases?.join(" → ") || "—"}</dd>
        <dt>有效期</dt><dd>{new Date(value.expires_at).toLocaleString()}</dd>
      </dl>
      {permissions ? (
        <div style={{ ...muted, display: "grid", gap: 3 }}>
          <span>文件系统：{permissions.filesystem.join(", ") || "无"}</span>
          <span>网络：{permissions.network.join(", ") || "无"}</span>
          <span>Secret：{permissions.secrets.join(", ") || "无"}</span>
          <span>事件写入：{permissions.events_write.join(", ") || "无"}</span>
        </div>
      ) : null}
      {value.changes.permission_expansion?.length ? <div style={{ color: "var(--cx-warning)", fontSize: 12 }}>新增权限：{value.changes.permission_expansion.join(", ")}</div> : null}
      {value.confirmations_required.map((item) => (
        <Checkbox
          key={item}
          className="action-dialog-option"
          checked={confirmations.includes(item)}
          onCheckedChange={(checked) => onConfirmations(checked ? [...confirmations, item] : confirmations.filter((value) => value !== item))}
          label={CONFIRMATION_LABELS[item] || `确认 ${item}`}
        />
      ))}
    </div>
  );
}


function SourceForm({
  draft,
  onChange,
}: {
  draft: SourceDraft;
  onChange: (d: SourceDraft) => void;
}) {
  const set = (key: keyof SourceDraft) => (e: React.ChangeEvent<HTMLInputElement | HTMLSelectElement>) =>
    onChange({ ...draft, [key]: e.target.value });
  return (
    <div className="extension-source-form" style={{ display: "grid", gap: 8 }}>
      <div className="extension-form-row cx-ext-fields">
        <div className="cx-ext-field cx-ext-field-kind">
          <span>来源类型</span>
          <Select
            ariaLabel="来源类型"
            size="sm"
            value={draft.kind}
            onChange={(key) => onChange({ ...draft, kind: key as SourceDraft["kind"] })}
            options={SOURCE_KINDS.map(([kind, label]) => ({ value: kind, label }))}
          />
        </div>
        {draft.kind === "local-dir" || draft.kind === "archive" ? (
          <label className="cx-ext-field cx-ext-field-grow">
            <span>路径</span>
            <Input
              size="sm"
              style={input}
              placeholder={draft.kind === "local-dir" ? "如 /path/to/extension" : "如 /tmp/hello-1.1.0.tar.gz"}
              value={draft.path}
              onChange={set("path")}
            />
          </label>
        ) : null}
        {draft.kind === "git" || draft.kind === "http" ? (
          <label className="cx-ext-field cx-ext-field-grow">
            <span>URL</span>
            <Input size="sm" style={input} placeholder="https://…" value={draft.url} onChange={set("url")} />
          </label>
        ) : null}
        {draft.kind === "git" ? (
          <label className="cx-ext-field" style={{ width: 160 }}>
            <span>ref</span>
            <Input size="sm" style={input} placeholder="分支 / tag / commit" value={draft.ref} onChange={set("ref")} />
          </label>
        ) : null}
        {draft.kind === "catalog" ? (
          <>
            <label className="cx-ext-field cx-ext-field-grow">
              <span>catalog 根目录</span>
              <Input size="sm" style={input} value={draft.catalog_root} onChange={set("catalog_root")} />
            </label>
            <label className="cx-ext-field cx-ext-field-grow">
              <span>插件名称</span>
              <Input size="sm" style={input} value={draft.extension_id} onChange={set("extension_id")} />
            </label>
            <label className="cx-ext-field" style={{ width: 120 }}>
              <span>版本</span>
              <Input size="sm" style={input} value={draft.version} onChange={set("version")} />
            </label>
          </>
        ) : null}
      </div>
    </div>
  );
}


export function ExtensionSettings() {
  const [cards, setCards] = useState<OverviewCard[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState("");
  const [installDraft, setInstallDraft] = useState<SourceDraft>(EMPTY_SOURCE);
  const [installPreview, setInstallPreview] = useState<InstallPreview | null>(null);
  const [installConfirmations, setInstallConfirmations] = useState<string[]>([]);
  const [upgradeDraft, setUpgradeDraft] = useState<Record<string, SourceDraft>>({});
  const [upgradePreviews, setUpgradePreviews] = useState<Record<string, InstallPreview>>({});
  const [upgradeConfirmations, setUpgradeConfirmations] = useState<Record<string, string[]>>({});
  const [showUpgrade, setShowUpgrade] = useState("");
  const [selected, setSelected] = useState("");
  const contextMenu = useContextMenu();
  const [contextExtensionId, setContextExtensionId] = useState("");
  const [detail, setDetail] = useState<Detail | null>(null);
  const [contributions, setContributions] = useState<UiContributions>({});
  const [projection, setProjection] = useState<unknown>(null);
  const [viewerProjections, setViewerProjections] = useState<Record<string, unknown>>({});
  const [logs, setLogs] = useState<LogPage | null>(null);
  const [invokeResults, setInvokeResults] = useState<Record<string, unknown>>({});
  const [catalogRoot, setCatalogRoot] = useState("");
  const [catalogQuery, setCatalogQuery] = useState("");
  const [catalogEntries, setCatalogEntries] = useState<{ id: string; version: string; description: string; source: SourceDraft; publisher?: string; trust_status?: string; revoked?: boolean; revoke_reason?: string }[]>([]);
  const [uninstallDraft, setUninstallDraft] = useState<{
    id: string;
    preserveState: boolean;
    preserveArtifacts: boolean;
  } | null>(null);

  const load = useCallback(async () => {
    setError("");
    try {
      const res = await apiFetch("/api/extensions/overview");
      if (!res.ok) throw new Error(`扩展列表加载失败（HTTP ${res.status}）`);
      const body = await res.json();
      setCards(Array.isArray(body.extensions) ? body.extensions : []);
    } catch (exc) {
      setError(exc instanceof Error ? exc.message : String(exc));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const detailGen = useRef(0);
  const loadDetail = useCallback(async (extensionId: string) => {
    const gen = ++detailGen.current;
    const base = `/api/extensions/${encodeURIComponent(extensionId)}`;
    const [rDetail, rUi, rLogs] = await Promise.all([
      apiFetch(`${base}/detail`),
      apiFetch(`${base}/ui`),
      apiFetch(`${base}/logs/page?limit=50`),
    ]);
    if (gen !== detailGen.current) return;
    if (rDetail.ok) setDetail(await rDetail.json());
    let nextContributions: UiContributions = {};
    if (rUi.ok) {
      const body = await rUi.json();
      nextContributions = (body.contributions as UiContributions) ?? {};
    }
    if (gen !== detailGen.current) return;
    setContributions(nextContributions);
    setProjection(null);
    setViewerProjections({});
    if (nextContributions.board) {
      const query = nextContributions.board.projection
        ? `?name=${encodeURIComponent(nextContributions.board.projection)}` : "";
      const rProj = await apiFetch(`${base}/projection${query}`);
      if (gen !== detailGen.current) return;
      if (rProj.ok) {
        const body = await rProj.json();
        setProjection(body.status === "ok" ? body.data ?? body : null);
      }
    }
    const viewers = nextContributions.artifact_viewers ?? [];
    const viewerData: Record<string, unknown> = {};
    await Promise.all(viewers.map(async (viewer) => {
      const query = viewer.projection ? `?name=${encodeURIComponent(viewer.projection)}` : "";
      const r = await apiFetch(`${base}/projection${query}`);
      if (r.ok) {
        const body = await r.json();
        if (body.status === "ok") viewerData[viewer.id] = body.data ?? body;
      }
    }));
    if (gen !== detailGen.current) return;
    setViewerProjections(viewerData);
    if (rLogs.ok) setLogs(await rLogs.json());
  }, []);

  const select = useCallback(
    async (extensionId: string) => {
      if (selected === extensionId) {
        detailGen.current += 1;
        setSelected("");
        setDetail(null);
        return;
      }
      setSelected(extensionId);
      setDetail(null);
      setInvokeResults({});
      try {
        await loadDetail(extensionId);
      } catch (exc) {
        setError(exc instanceof Error ? exc.message : String(exc));
      }
    },
    [selected, loadDetail],
  );

  const runAction = useCallback(
    async (label: string, path: string, init?: RequestInit) => {
      setBusy(label);
      setError("");
      setNotice("");
      try {
        const res = await apiFetch(path, init);
        const body = await res.json().catch(() => ({}));
        const receipt: Receipt | undefined = body.receipt ?? body.detail ?? body;
        if (!res.ok || (receipt && receipt.state && receipt.state !== "completed")) {
          setError(`${label} 失败：${receiptError(receipt) || `HTTP ${res.status}`}`);
          return false;
        }
        setNotice(`${label} 完成（命令 ${receipt?.command_id ?? "—"}，状态 ${receipt?.state ?? "ok"}）`);
        try {
          await load();
          if (selected) await loadDetail(selected);
        } catch (refreshExc) {
          setError(`详情刷新失败（命令已完成）：${refreshExc instanceof Error ? refreshExc.message : String(refreshExc)}`);
        }
        return true;
      } catch (exc) {
        setError(`${label} 失败：${exc instanceof Error ? exc.message : String(exc)}`);
        return false;
      } finally {
        setBusy("");
      }
    },
    [load, loadDetail, selected],
  );

  const install = useCallback(async () => {
    if (!installPreview) {
      setError("请先生成安装预览并核对来源、摘要和权限");
      return;
    }
    const ok = await runAction("安装", "/api/extensions/install", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        source: sourcePayload(installDraft),
        preview_id: installPreview.preview_id,
        confirmations: installConfirmations,
      }),
    });
    if (ok) {
      setInstallDraft(EMPTY_SOURCE);
      setInstallPreview(null);
      setInstallConfirmations([]);
    }
  }, [installConfirmations, installDraft, installPreview, runAction]);

  const preview = useCallback(async (draft: SourceDraft, extensionId = "") => {
    setBusy(extensionId ? `预览升级 ${extensionId}` : "生成安装预览");
    setError("");
    try {
      const res = await apiFetch("/api/extensions/preview", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ source: sourcePayload(draft), extension_id: extensionId }),
      });
      const body = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(body.error?.message || body.detail?.message || body.error?.code || body.detail?.code || `HTTP ${res.status}`);
      const value = body as InstallPreview;
      if (extensionId) {
        setUpgradePreviews((items) => ({ ...items, [extensionId]: value }));
        setUpgradeConfirmations((items) => ({ ...items, [extensionId]: [] }));
      } else {
        setInstallPreview(value);
        setInstallConfirmations([]);
      }
      setNotice(`预览已生成：${value.extension_id}@${value.version}，待确认后激活`);
    } catch (exc) {
      setError(`预览失败：${exc instanceof Error ? exc.message : String(exc)}`);
    } finally {
      setBusy("");
    }
  }, []);

  const enable = useCallback(
    (id: string, config?: Record<string, unknown>) =>
      runAction(`启用 ${id}`, `/api/extensions/${encodeURIComponent(id)}/enable`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(config ? { config } : {}),
      }),
    [runAction],
  );

  const disable = useCallback(
    (id: string) =>
      runAction(`停用 ${id}`, `/api/extensions/${encodeURIComponent(id)}/disable`, { method: "POST" }),
    [runAction],
  );

  const upgrade = useCallback(
    async (id: string) => {
      const draft = upgradeDraft[id] ?? EMPTY_SOURCE;
      const install = upgradePreviews[id];
      if (!install) {
        setError("请先生成升级预览并核对变更");
        return;
      }
      const ok = await runAction(`升级 ${id}`, `/api/extensions/${encodeURIComponent(id)}/upgrade`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          source: sourcePayload(draft),
          preview_id: install.preview_id,
          confirmations: upgradeConfirmations[id] || [],
        }),
      });
      if (ok) setShowUpgrade("");
    },
    [upgradeConfirmations, upgradeDraft, upgradePreviews, runAction],
  );

  const loadCatalog = useCallback(async () => {
    setBusy("加载 Catalog");
    setError("");
    try {
      const params = new URLSearchParams({ root: catalogRoot, q: catalogQuery });
      const res = await apiFetch(`/api/extensions/catalog?${params.toString()}`);
      const body = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(body.error?.message || body.detail?.message || `HTTP ${res.status}`);
      setCatalogEntries(Array.isArray(body.entries) ? body.entries : []);
      if (!body.entries?.length) setNotice(body.detail || "Catalog 中没有匹配项");
    } catch (exc) {
      setError(`Catalog 加载失败：${exc instanceof Error ? exc.message : String(exc)}`);
    } finally {
      setBusy("");
    }
  }, [catalogQuery, catalogRoot]);

  const rollback = useCallback(
    (id: string) =>
      runAction(`回滚 ${id}`, `/api/extensions/${encodeURIComponent(id)}/rollback`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "{}",
      }),
    [runAction],
  );

  const uninstall = useCallback(
    async () => {
      if (!uninstallDraft) return;
      const { id, preserveState, preserveArtifacts } = uninstallDraft;
      const ok = await runAction(`卸载 ${id}`, `/api/extensions/${encodeURIComponent(id)}`, {
        method: "DELETE",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          confirm: true,
          preserve_state: preserveState,
          preserve_logs: preserveState,
          preserve_artifacts: preserveArtifacts,
        }),
      });
      if (ok) {
        if (selected === id) {
          setSelected("");
          setDetail(null);
        }
        setUninstallDraft(null);
      }
    },
    [runAction, selected, uninstallDraft],
  );

  const refreshHealth = useCallback(
    async (id: string) => {
      setBusy("健康探测");
      setError("");
      try {
        const res = await apiFetch(`/api/extensions/${encodeURIComponent(id)}/health/refresh`, { method: "POST" });
        const body = await res.json();
        const status = body.health?.status ?? "unknown";
        setNotice(`${id} 实时健康：${status}${body.health?.detail ? `（${body.health.detail}）` : ""}`);
        await load();
      } catch (exc) {
        setError(exc instanceof Error ? exc.message : String(exc));
      } finally {
        setBusy("");
      }
    },
    [load],
  );

  const invoke = useCallback(
    async (id: string, commandType: string, params: Record<string, unknown>) => {
      setBusy(`调用 ${commandType}`);
      setError("");
      try {
        const res = await apiFetch(`/api/extensions/${encodeURIComponent(id)}/invoke`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ command_type: commandType, params }),
        });
        const body = await res.json().catch(() => ({}));
        const receipt: Receipt | undefined = body.receipt ?? body.detail ?? body;
        if (!res.ok || !receipt || receipt.state !== "completed") {
          setError(`调用失败：${receiptError(receipt) || `HTTP ${res.status}`}`);
          return;
        }
        setInvokeResults((prev) => ({ ...prev, [commandType]: body.result }));
        setNotice(`调用完成（命令 ${receipt.command_id}）`);
        try {
          await loadDetail(id);
        } catch (refreshExc) {
          setError(`详情刷新失败（命令已完成）：${refreshExc instanceof Error ? refreshExc.message : String(refreshExc)}`);
        }
      } catch (exc) {
        setError(`调用失败：${exc instanceof Error ? exc.message : String(exc)}`);
      } finally {
        setBusy("");
      }
    },
    [loadDetail],
  );

  const loadMoreLogs = useCallback(async () => {
    if (!selected || !logs) return;
    const res = await apiFetch(
      `/api/extensions/${encodeURIComponent(selected)}/logs/page?offset=${logs.offset + logs.limit}&limit=${logs.limit}`,
    );
    if (!res.ok) return;
    const earlier: LogPage = await res.json();
    setLogs({
      ...earlier,
      lines: [...earlier.lines, ...logs.lines],
      offset: logs.offset,
      has_more: earlier.has_more,
    });
  }, [selected, logs]);

  const selectedStatusLabels: StatusLabelContribution | undefined = contributions.status_labels;
  const openUpgrade = (id: string) => {
    setShowUpgrade(showUpgrade === id ? "" : id);
    setUpgradeDraft(prev => ({ ...prev, [id]: prev[id] ?? EMPTY_SOURCE }));
  };
  const requestUninstall = (id: string) => {
    setError("");
    setUninstallDraft({ id, preserveState: true, preserveArtifacts: true });
  };
  const extensionActions = (record: ExtensionRecord) => <>
    <MenuItem icon="info" onSelect={() => void select(record.extension_id)}>{selected === record.extension_id ? "收起详情" : "查看详情"}</MenuItem>
    <MenuItem icon={record.enabled ? "pause" : "play"} disabled={Boolean(busy) || (!record.enabled && !record.installed_versions.length)}
      onSelect={() => { if (record.enabled) void disable(record.extension_id); else void enable(record.extension_id); }}>{record.enabled ? "停用" : "启用"}</MenuItem>
    <MenuItem icon="upload" disabled={Boolean(busy)} onSelect={() => openUpgrade(record.extension_id)}>{showUpgrade === record.extension_id ? "收起升级设置" : "升级…"}</MenuItem>
    <MenuItem icon="undo" disabled={Boolean(busy) || !record.installed_versions.some(version => version !== record.active_version)}
      onSelect={() => void rollback(record.extension_id)}>恢复上一版本</MenuItem>
    <MenuItem icon="plug" disabled={Boolean(busy) || !record.enabled} onSelect={() => void refreshHealth(record.extension_id)}>健康探测</MenuItem>
    <MenuSeparator />
    <MenuItem icon="trash" danger disabled={Boolean(busy)} onSelect={() => requestUninstall(record.extension_id)}>卸载…</MenuItem>
  </>;
  const contextExtension = cards.find(card => card.record.extension_id === contextExtensionId)?.record;

  return (
    <div className="extension-settings" style={page}>
      <div style={inner}>
        {error ? (
          <div role="alert" className="cx-ext-note" data-tone="danger">
            {error}
          </div>
        ) : null}
        {notice ? (
          <div role="status" className="cx-ext-note" data-tone="success">
            {notice}
          </div>
        ) : null}

        <SettingsSection
          className="cx-ext-section"
          bodyClassName="extension-settings-card cx-ext-card"
          title="安装平台扩展包"
          description={<>根目录需包含 Agent Plugins 1.0.0 <code className="cx-ext-code">plugin.json</code>，并声明 Muteki 平台模块命名空间。</>}
        >
          <SourceForm draft={installDraft} onChange={(draft) => {
            setInstallDraft(draft);
            setInstallPreview(null);
            setInstallConfirmations([]);
          }} />
          {installPreview ? (
            <PreviewPanel value={installPreview} confirmations={installConfirmations} onConfirmations={setInstallConfirmations} />
          ) : null}
          <div className="extension-actions cx-ext-footer" style={row}>
            <span style={{ flex: 1 }} />
            <Button
              type="button"
              variant="secondary"
              size="sm"
              className={btn}
              disabled={Boolean(busy)}
              onClick={() => void preview(installDraft)}
            >
              {busy === "生成安装预览" ? "检查中…" : "生成安装预览"}
            </Button>
            <Button
              type="button"
              variant="primary"
              size="sm"
              className={btnPrimary}
              disabled={Boolean(busy) || !installPreview || installPreview.confirmations_required.some((item) => !installConfirmations.includes(item))}
              onClick={() => void install()}
            >
              {busy === "安装" ? "安装中…" : "安装"}
            </Button>
          </div>
        </SettingsSection>

        <SettingsSection
          className="cx-ext-section"
          bodyClassName="extension-settings-card cx-ext-card"
          title="平台扩展包 Catalog"
          description="浏览管理员提供的版本固定清单"
        >
          <div className="extension-catalog-row cx-ext-fields">
            <div className="cx-ext-field cx-ext-field-grow">
              <label htmlFor="extension-catalog-root">Catalog 路径</label>
              <Input size="sm" id="extension-catalog-root" style={input} value={catalogRoot} onChange={(event) => setCatalogRoot(event.target.value)} placeholder="目录或 YAML 文件" />
            </div>
            <div className="cx-ext-field" style={{ width: 240 }}>
              <label htmlFor="extension-catalog-query">搜索</label>
              <Input size="sm" id="extension-catalog-query" style={input} value={catalogQuery} onChange={(event) => setCatalogQuery(event.target.value)} placeholder="id、版本或说明" />
            </div>
            <Button type="button" variant="secondary" size="sm" className={btn} disabled={Boolean(busy)} onClick={() => void loadCatalog()}>加载</Button>
          </div>
          {catalogEntries.map((entry) => (
            <div key={`${entry.id}:${entry.version}`} className="cx-ext-list-row" style={row}>
              <strong style={mono}>{entry.id}@{entry.version}</strong>
              {badge(entry.revoked ? "var(--red)" : entry.trust_status === "verified" ? "var(--green)" : "var(--amber)", entry.revoked ? "revoked" : entry.trust_status || "unsigned", entry.revoke_reason)}
              <span style={muted}>发布者 {entry.publisher || "unknown"}</span>
              <span style={muted}>{entry.description || "无说明"}</span>
              <span style={{ flex: 1 }} />
              <Button type="button" variant="secondary" size="sm" className={btn} disabled={entry.revoked} onClick={() => {
                setInstallDraft({ ...EMPTY_SOURCE, kind: "catalog", catalog_root: catalogRoot, extension_id: entry.id, version: entry.version });
                setInstallPreview(null);
              }}>选择</Button>
            </div>
          ))}
        </SettingsSection>

        <SettingsSection
          className="cx-ext-section"
          bodyClassName="extension-settings-card cx-ext-card cx-ext-list"
          title="已安装平台扩展包"
          description={loading ? "加载中…" : `共 ${cards.length} 个`}
          actions={
            <Button type="button" variant="secondary" size="sm" className={btn} disabled={loading} onClick={() => void load()}>
              刷新
            </Button>
          }
        >
          {loading ? (
            <div className="cx-ext-empty">正在加载插件列表…</div>
          ) : cards.length === 0 ? (
            <div className="cx-ext-empty">尚未安装任何平台扩展包。</div>
          ) : (
            cards.map(({ record, manifest, live_health: liveHealth, migrations }) => {
              const state = STATE_LABELS[record.state] ?? { label: record.state, color: "var(--muted)" };
              const id = record.extension_id;
              const rollbackTargets = record.installed_versions.filter((v) => v !== record.active_version);
              return (
                <article
                  key={id}
                  className="cx-ext-plugin"
                  data-selected={selected === id || undefined}
                  onContextMenu={event => { contextMenu.onContextMenu(event); if (event.defaultPrevented) setContextExtensionId(id); }}
                  onKeyDown={event => { contextMenu.onKeyDown(event); if (event.defaultPrevented) setContextExtensionId(id); }}
                  style={{ opacity: record.enabled || record.state === "installed" ? 1 : 0.75 }}
                >
                  <div className="cx-ext-plugin-head" style={row}>
                    <strong className="cx-ext-plugin-name">{id}</strong>
                    {badge("var(--blue)", ORIGIN_LABELS[record.origin] ?? record.origin)}
                    {selected === id && selectedStatusLabels ? (
                      <StatusLabel value={record.state} contribution={selectedStatusLabels} fallback={badge(state.color, state.label)} />
                    ) : (
                      badge(state.color, state.label)
                    )}
                    {record.active_version
                      ? badge("var(--muted)", `v${record.active_version}`, `已装版本：${record.installed_versions.join(", ")}`)
                      : badge("var(--muted)", "未启用版本")}
                    {record.enabled ? (
                      liveHealth?.status === "healthy"
                        ? badge("var(--green)", "实时健康")
                        : badge("var(--amber)", `健康：${liveHealth?.status ?? "探测中"}`, liveHealth?.detail)
                    ) : (
                      badge("var(--muted)", "未启用")
                    )}
                    {manifest.has_ui ? badge("var(--blue)", "声明式 UI") : null}
                    {manifest.plugin_schema ? badge("var(--blue)", "Agent Plugins 1.0.0") : null}
                    {badge("var(--blue)", `Registry：${manifest.registry_state || "installed"}`)}
                    <span style={{ flex: 1 }} />
                    <Button type="button" variant="secondary" size="sm" className={btn} onClick={() => void select(id)}>
                      {selected === id ? "收起" : "详情"}
                    </Button>
                    <Menu trigger={<IconButton icon="more" label={`${id} 更多操作`} />} placement="bottom-end" ariaLabel={`${id} 操作`}>
                      {extensionActions(record)}
                    </Menu>
                  </div>

                  <div className="cx-ext-meta" style={meta}>
                    {manifest.plugin_version ? <span>plugin v{manifest.plugin_version}</span> : null}
                    {manifest.version && manifest.version !== manifest.plugin_version ? <span>host v{manifest.version}</span> : null}
                    {manifest.client_namespace ? <code className="cx-ext-code">{manifest.client_namespace}</code> : null}
                    {manifest.requires_core ? <span>core {manifest.requires_core}</span> : null}
                    {manifest.portable_components?.skills?.length ? <span>Skills：{manifest.portable_components.skills.join(", ")}</span> : null}
                    {manifest.portable_components?.mcp_state === "ready" ? <span>MCP</span> : null}
                    <span>已装版本：{record.installed_versions.join(" / ") || "—"}</span>
                    {migrations.length ? <span>状态迁移 {migrations.length} 次</span> : null}
                    <span>更新于 {new Date(record.updated_at).toLocaleString()}</span>
                    <span>启动 {record.start_count || 0} 次</span>
                    <span>上次健康：{record.last_health_at ? new Date(record.last_health_at).toLocaleString() : "未记录"}</span>
                  </div>

                  {manifest.verification ? (
                    <div className="cx-ext-meta" style={meta}>
                      <span>签名：{String(manifest.verification.signature || "unsigned")}</span>
                      <span>发布者：{String(manifest.verification.publisher || "unknown")}</span>
                    </div>
                  ) : null}

                  {record.isolation?.enforcement ? (
                    <div className="cx-ext-meta" style={{ ...meta, color: record.isolation.enforcement === "enforced" ? meta.color : "var(--cx-warning)" }}>
                      <span>权限执行：{record.isolation.enforcement}</span>
                      <span>隔离：{record.isolation.backend || "none"}</span>
                      <span>子进程：{record.isolation.subprocess_policy || "未声明"}</span>
                      <span>退出码：{record.last_exit_code ?? "运行中"}</span>
                    </div>
                  ) : null}

                  {record.registry_entries?.length ? (
                    <div className="cx-ext-meta" style={meta}>
                      {record.registry_entries.map((entry) => <span key={`${entry.type}:${entry.id}`}>{entry.type} · <code className="cx-ext-code">{entry.id}</code> · {entry.state}{entry.detail ? ` · ${entry.detail}` : ""}</span>)}
                    </div>
                  ) : null}

                  {manifest.permissions ? (
                    <div className="cx-ext-meta" style={meta}>
                      <span>文件系统：{manifest.permissions.filesystem.join(", ") || "无"}</span>
                      <span>网络：{manifest.permissions.network.join(", ") || "无"}</span>
                      <span>
                        Secret：
                        {manifest.permissions.secrets.join(", ") || "无"}
                      </span>
                      <span>事件写入：{manifest.permissions.events_write.join(", ") || "无"}</span>
                    </div>
                  ) : null}

                  {manifest.provides?.length ? (
                    <div style={row}>
                      {manifest.provides.map((p) => badge("var(--blue)", `${p.type} · ${p.id}`, `api v${p.api_version}`))}
                    </div>
                  ) : null}

                  {record.last_error ? (
                    <div style={{ color: "var(--cx-danger)", fontSize: 12 }}>{record.last_error}</div>
                  ) : null}

                  <div className="cx-ext-plugin-actions" style={row}>
                    {record.enabled ? (
                      <Button type="button" variant="secondary" size="sm" className={btn} agentConfirm="destructive" disabled={Boolean(busy)} onClick={() => void disable(id)}>
                        停用
                      </Button>
                    ) : (
                      <Button
                        type="button"
                        variant="primary"
                        size="sm"
                        className={btnPrimary}
                        disabled={Boolean(busy) || !record.installed_versions.length}
                        onClick={() => void enable(id)}
                      >
                        启用
                      </Button>
                    )}
                    <Button
                      type="button"
                      variant="secondary"
                      size="sm"
                      className={btn}
                      disabled={Boolean(busy)}
                      onClick={() => openUpgrade(id)}
                    >
                      升级
                    </Button>
                    <Button
                      type="button"
                      variant="secondary"
                      size="sm"
                      className={btn}
                      agentConfirm="destructive"
                      disabled={Boolean(busy) || !rollbackTargets.length}
                      aria-label={rollbackTargets.length ? `回滚到 v${rollbackTargets[rollbackTargets.length - 1]}` : "无其他已装版本"}
                      onClick={() => void rollback(id)}
                    >
                      回滚
                    </Button>
                    <Button
                      type="button"
                      variant="secondary"
                      size="sm"
                      className={btn}
                      disabled={Boolean(busy) || !record.enabled}
                      onClick={() => void refreshHealth(id)}
                    >
                      健康探测
                    </Button>
                    <span style={{ flex: 1 }} />
                    <Button
                      type="button"
                      variant="danger-soft"
                      size="sm"
                      className={btnDanger}
                      agentConfirm="destructive"
                      disabled={Boolean(busy)}
                      onClick={() => requestUninstall(id)}
                    >
                      卸载
                    </Button>
                  </div>

                  {showUpgrade === id ? (
                    <div className="cx-ext-upgrade">
                      <span className="cx-ext-detail-label">升级来源（旧版本按插件升级策略处理，状态迁移会写 receipt）</span>
                      <SourceForm
                        draft={upgradeDraft[id] ?? EMPTY_SOURCE}
                        onChange={(d) => {
                          setUpgradeDraft((prev) => ({ ...prev, [id]: d }));
                          setUpgradePreviews((prev) => {
                            const next = { ...prev };
                            delete next[id];
                            return next;
                          });
                        }}
                      />
                      {upgradePreviews[id] ? (
                        <PreviewPanel
                          value={upgradePreviews[id]}
                          confirmations={upgradeConfirmations[id] || []}
                          onConfirmations={(items) => setUpgradeConfirmations((prev) => ({ ...prev, [id]: items }))}
                        />
                      ) : null}
                      <div style={row}>
                        <span style={{ flex: 1 }} />
                        <Button type="button" variant="secondary" size="sm" className={btn} disabled={Boolean(busy)} onClick={() => void preview(upgradeDraft[id] ?? EMPTY_SOURCE, id)}>
                          {busy === `预览升级 ${id}` ? "检查中…" : "生成升级预览"}
                        </Button>
                        <Button
                          type="button"
                          variant="primary"
                          size="sm"
                          className={btnPrimary}
                          disabled={Boolean(busy) || !upgradePreviews[id] || upgradePreviews[id].confirmations_required.some((item) => !(upgradeConfirmations[id] || []).includes(item))}
                          onClick={() => void upgrade(id)}
                        >
                          {busy ? "升级中…" : "执行升级"}
                        </Button>
                      </div>
                    </div>
                  ) : null}
                </article>
              );
            })
          )}
        </SettingsSection>

        {selected && detail ? (
          <SettingsSection
            className="cx-ext-section"
            bodyClassName="cx-ext-card cx-ext-detail"
            title={<>插件详情 <code className="cx-ext-code">{selected}</code></>}
            actions={detail.live_health
              ? badge(
                  detail.live_health.status === "healthy" ? "var(--green)" : "var(--amber)",
                  `实时健康：${detail.live_health.status ?? "unknown"}`,
                  detail.live_health.detail,
                )
              : badge("var(--muted)", "未启用，无实时健康")}
          >

            {contributions.navigation?.length ? <NavigationItems items={contributions.navigation} /> : null}

            {detail.migrations.length ? (
              <div style={{ display: "grid", gap: 4 }}>
                <span className="cx-ext-detail-label">状态迁移 receipt</span>
                {detail.migrations.map((m) => (
                  <span key={`${m.from_version}-${m.to_version}`} style={muted}>
                    {m.from_version ?? "none"} → {m.to_version} · {new Date(m.migrated_at).toLocaleString()} ·{" "}
                    {m.files.length ? `迁移文件：${m.files.join(", ")}` : "无状态文件"}
                  </span>
                ))}
              </div>
            ) : null}

            {contributions.command_forms?.length && detail.record.enabled ? (
              <div style={{ display: "grid", gap: 8 }}>
                <span className="cx-ext-detail-label">命令表单（声明式 contribution）</span>
                {contributions.command_forms.map((form, index) => (
                  <CommandFormView
                    key={form.command_type || form.id || `form-${index}`}
                    form={form}
                    busy={busy === `调用 ${form.command_type}`}
                    result={invokeResults[form.command_type]}
                    onInvoke={(commandType, params) => void invoke(selected, commandType, params)}
                  />
                ))}
              </div>
            ) : null}

            {contributions.board && projection !== null ? (
              <div style={{ display: "grid", gap: 6 }}>
                <span className="cx-ext-detail-label">看板（projection：{contributions.board.projection ?? "默认"}）</span>
                <BoardView board={contributions.board as BoardContribution} data={projection} />
              </div>
            ) : null}

            {contributions.artifact_viewers?.length && (projection !== null || Object.keys(viewerProjections).length > 0) ? (
              <div style={{ display: "grid", gap: 8 }}>
                <span className="cx-ext-detail-label">Artifact viewer（声明式，受控渲染）</span>
                {contributions.artifact_viewers.map((viewer) => (
                  <ArtifactViewerView key={viewer.id} viewer={viewer} data={viewerProjections[viewer.id]} />
                ))}
              </div>
            ) : null}

            {detail.config_schema ? (
              <div style={{ display: "grid", gap: 6 }}>
                <span className="cx-ext-detail-label">设置 schema（config_schema 声明式渲染，提交即带配置重新启用）</span>
                <SchemaForm
                  key={`${detail.record.extension_id}:${JSON.stringify(detail.record.config ?? {})}`}
                  schema={detail.config_schema}
                  initial={detail.record.config}
                  submitLabel={detail.record.enabled ? "应用配置并重新启用" : "带配置启用"}
                  busy={Boolean(busy)}
                  onSubmit={(config) => void enable(detail.record.extension_id, config)}
                />
              </div>
            ) : detail.config_schema_error ? (
              <div style={{ color: "var(--cx-danger)", fontSize: 12 }}>config schema 读取失败：{detail.config_schema_error}</div>
            ) : null}

            <div style={{ display: "grid", gap: 6 }}>
              <div style={row}>
                <span className="cx-ext-detail-label">归档日志（完整内容，尾部共 {logs?.total ?? 0} 行）</span>
                {logs?.has_more ? (
                  <Button type="button" variant="secondary" size="sm" className={btn} onClick={() => void loadMoreLogs()}>
                    加载更早
                  </Button>
                ) : null}
              </div>
              {logs && logs.lines.length ? (
                <pre className="cx-ext-log" style={mono}>
                  {logs.lines.join("\n")}
                </pre>
              ) : (
                <span style={muted}>暂无日志。</span>
              )}
            </div>
          </SettingsSection>
        ) : null}

        {selected && !detail ? (
          <div className="cx-settings-card cx-ext-empty">正在加载 {selected} 的详情…</div>
        ) : null}
      </div>
      {contextExtension ? contextMenu.render(extensionActions(contextExtension), `${contextExtension.extension_id} 操作`) : null}
      <Dialog
        open={Boolean(uninstallDraft)}
        onOpenChange={(open) => !open && setUninstallDraft(null)}
        dismissable={!busy}
        className="cx-settings-dialog"
        tone="danger"
        title={uninstallDraft ? `卸载 ${uninstallDraft.id}` : "卸载扩展"}
        description="卸载会停止并移除扩展。请明确选择需要保留的数据，再确认执行。"
        footer={<>
          <Button variant="ghost" onClick={() => setUninstallDraft(null)} disabled={Boolean(busy)}>取消</Button>
          <Button variant="danger" agentConfirm="destructive" loading={Boolean(uninstallDraft && busy === `卸载 ${uninstallDraft.id}`)} onClick={() => void uninstall()}>卸载</Button>
        </>}
      >
        {uninstallDraft ? (
          <div className="action-dialog-options">
            <Checkbox
              className="rounded-xl border border-cx-border bg-cx-elevated px-3.5 py-3"
              checked={uninstallDraft.preserveState}
              onCheckedChange={(checked) => setUninstallDraft((current) => current ? { ...current, preserveState: checked } : current)}
              label="保留扩展状态和运行日志"
              description="取消勾选后，卸载时会删除状态与归档日志。"
            />
            <Checkbox
              className="rounded-xl border border-cx-border bg-cx-elevated px-3.5 py-3"
              checked={uninstallDraft.preserveArtifacts}
              onCheckedChange={(checked) => setUninstallDraft((current) => current ? { ...current, preserveArtifacts: checked } : current)}
              label="保留关联 Artifact"
              description="取消勾选后将删除关联 Artifact；当前没有 Artifact 时也会记录选择。"
            />
            <div className="action-dialog-summary">
              <span>状态与日志：<strong>{uninstallDraft.preserveState ? "保留" : "删除"}</strong></span>
              <span>Artifact：<strong>{uninstallDraft.preserveArtifacts ? "保留" : "删除"}</strong></span>
            </div>
            {error ? <p role="alert" className="cx-settings-dialog-error">{error}</p> : null}
          </div>
        ) : null}
      </Dialog>
    </div>
  );
}
