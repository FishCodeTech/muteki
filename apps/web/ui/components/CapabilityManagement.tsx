"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { Button, Checkbox, ListBox, ListBoxItem, Modal, Select } from "@heroui/react";
import { EngineLogo } from "@/components/EngineLogo";
import { Icon } from "@/components/Icon";
import { apiFetch } from "@/lib/useRun";

// ── Types ───────────────────────────────────────────────────────────────────

export type ToolRow = {
  name: string;
  description: string;
  target_kind: string;
  command_type?: string | null;
  query_type?: string | null;
  aggregate_type?: string;
  source: string;
  default_modes: string[];
};

export type Binding = {
  binding_id: string;
  binding_version: number;
  thread_id: string;
  principal_id: string;
  mode: string;
  tool_set: string[];
  allowed_commands: string[];
  allowed_queries: string[];
  resource_scopes: string[];
  policy_version?: number;
  created_at: string;
  revoked_at?: string | null;
};

export type Grant = {
  grant_id: string;
  binding_id?: string;
  agent_session_id: string;
  runtime_instance_id?: string;
  injection_kind: string;
  audience?: string;
  credential_reference: string;
  credential_status: string;
  state: "active" | "expired" | "revoked";
  issued_at: string;
  expires_at?: string | null;
  revoked_at?: string | null;
  last_touched_at?: string | null;
};

export type ThreadBindingRow = {
  thread: {
    thread_id: string;
    title: string;
    mode: string;
    updated_at: string;
  };
  principal_id: string;
  active_binding: Binding | null;
  initial_tool_set: string[];
  selection_source: "binding" | "mode_template";
  versions: Binding[];
  grants: Grant[];
};

export type InjectionSupport = {
  kind: string;
  supported: boolean;
  source: string;
};

export type AdapterRow = {
  engine: string;
  adapter_id: string;
  instance_id: string;
  configured: boolean;
  enabled: boolean;
  transport_kind: string;
  health_state: "healthy" | "unhealthy" | "unprobed" | string;
  health_detail: string;
  probed_at?: string | null;
  runtime_version: string;
  injection_support: InjectionSupport[];
  source: string[];
  instances: Array<{
    adapter_id: string;
    instance_id: string;
    configured: boolean;
    enabled: boolean;
    transport_kind: string;
    health_state: string;
  }>;
  last_session_injection: (Grant & {
    adapter_id: string;
    runtime_instance_id: string;
    thread_id?: string | null;
    session_closed?: boolean;
  }) | null;
};

export type ExtensionProvide = {
  type: string;
  id: string;
  api_version: number;
  section: string;
  registry_state: string;
  registry_detail: string;
};

export type ExtensionSource = {
  extension_id: string;
  version?: string;
  origin: string;
  state: string;
  enabled: boolean;
  health: string;
  manifest_error?: string;
  provides: ExtensionProvide[];
  secret_references: Array<{ reference: string; status: string }>;
};

export type McpServer = {
  id: string;
  name: string;
  enabled: boolean;
  health: string;
  source: string;
  scope: string;
  endpoint: string;
  protocol_version: string;
  tools: number;
  active_grants: number;
  lifecycle: string;
};

export type SkillItem = {
  id: string;
  name: string;
  enabled: boolean;
  mutable: boolean;
  health: string;
  source: string;
  scope: string;
  engines: Record<string, string[]>;
  legacy_copies: Array<{ path: string; state: string }>;
  lifecycle: string;
};

export type Overview = {
  mcp: {
    status: string;
    endpoint: string;
    implementation: string;
    active_grants: number;
    servers: McpServer[];
    extension_sources: ExtensionSource[];
  };
  skills: {
    status: string;
    id: string;
    implementation: string;
    active_grants: number;
    items: SkillItem[];
    extension_sources: ExtensionSource[];
  };
  tools: ToolRow[];
  adapters: AdapterRow[];
  extensions: ExtensionSource[];
  threads: ThreadBindingRow[];
  mode_templates: Record<string, string[]>;
};

type Feedback = { kind: "ok" | "bad"; text: string } | null;
type TabKey = "threads" | "services" | "adapters" | "catalog";

// ── Constants & Helpers ─────────────────────────────────────────────────────

const ENGINE_NAMES: Record<string, string> = {
  claude: "Claude Code",
  codex: "Codex",
  cursor: "Cursor",
  pi: "Pi",
  omp: "OMP",
  kimi: "Kimi Code",
  grok: "Grok",
  opencode: "OpenCode",
  devin: "Devin CLI",
  dsh: "DeepSeek Harness",
};

const MODE_NAMES: Record<string, string> = {
  conversation: "通用对话",
  single_task: "单题模式",
  competition: "比赛模式",
  management: "管理模式",
};

const INJECTION_NAMES: Record<string, string> = {
  mcp: "MCP",
  native_tool: "Native Tool",
  acp_mcp_config: "ACP MCP",
  agent_plugin: "Agent Plugin 1.0",
  runtime_plugin: "Runtime Plugin",
  http_jsonrpc: "HTTP JSON-RPC",
  cli_skill: "CLI Skill",
};

const TARGET_NAMES: Record<string, string> = {
  command: "写操作",
  query: "查询",
  read_events: "事件流",
  wait: "有界等待",
  receipt: "回执",
};

function fmt(value?: string | null): string {
  if (!value) return "未记录";
  const time = Date.parse(value);
  return Number.isNaN(time)
    ? value
    : new Date(time).toLocaleString("zh-CN", { hour12: false });
}

function rowKey(row: ThreadBindingRow): string {
  return `${row.thread.thread_id}\u0000${row.principal_id}`;
}

function sameSet(left: string[], right: string[]): boolean {
  return [...left].sort().join("\u0000") === [...right].sort().join("\u0000");
}

async function responseError(response: Response): Promise<string> {
  const payload = await response.json().catch(() => ({}));
  return String(payload.detail || payload.error?.message || `HTTP ${response.status}`);
}

function StatePill({ state }: { state: string }) {
  const ok = ["ready", "healthy", "active", "referenced", "injected", "linked"].includes(state);
  const bad = ["unhealthy", "unavailable", "revoked", "missing", "disabled"].includes(state);
  const warn = ["unprobed", "expired", "declared", "copied"].includes(state);

  const labels: Record<string, string> = {
    ready: "可用",
    healthy: "健康",
    unhealthy: "异常",
    unavailable: "不可用",
    unprobed: "未探测",
    active: "活动",
    expired: "已到期",
    revoked: "已撤销",
    declared: "已声明",
    injected: "已注入",
    referenced: "引用有效",
    disabled: "已停用",
    missing: "缺失",
    linked: "符号链接",
    copied: "目录副本",
    absent: "未安装",
  };

  const className = `cap-state${ok ? " ok" : bad ? " bad" : warn ? " warn" : ""}`;
  return (
    <span className={className}>
      <i />
      {labels[state] || state}
    </span>
  );
}

// ── Component ───────────────────────────────────────────────────────────────

export function CapabilityManagement({ hideIntro = false }: { hideIntro?: boolean }) {
  const [data, setData] = useState<Overview | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [activeTab, setActiveTab] = useState<TabKey>("threads");
  const [selectedKey, setSelectedKey] = useState("");
  const [selectedTools, setSelectedTools] = useState<string[]>([]);
  const [threadSearch, setThreadSearch] = useState("");
  const [toolSearch, setToolSearch] = useState("");
  const [targetFilter, setTargetFilter] = useState("all");
  const [catalogSearch, setCatalogSearch] = useState("");
  const [catalogModeFilter, setCatalogModeFilter] = useState("all");
  const [feedback, setFeedback] = useState<Feedback>(null);
  const [revokeOpen, setRevokeOpen] = useState(false);

  const reload = useCallback(async (quiet = false) => {
    if (!quiet) setLoading(true);
    try {
      const response = await apiFetch("/api/capability-management", { cache: "no-store" });
      if (!response.ok) throw new Error(await responseError(response));
      const next = (await response.json()) as Overview;
      setData(next);
      setSelectedKey((current) => {
        if (next.threads.some((row) => rowKey(row) === current)) {
          return current;
        }
        return next.threads[0] ? rowKey(next.threads[0]) : "";
      });
    } catch (error) {
      setFeedback({ kind: "bad", text: `读取能力状态失败：${String(error)}` });
    } finally {
      if (!quiet) setLoading(false);
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  const selectedRow = useMemo(
    () => data?.threads.find((row) => rowKey(row) === selectedKey) || null,
    [data, selectedKey],
  );

  useEffect(() => {
    setSelectedTools(selectedRow ? [...selectedRow.initial_tool_set] : []);
  }, [selectedKey, selectedRow]);

  useEffect(() => {
    setFeedback(null);
  }, [selectedKey]);

  // Filters for Thread Binding tools
  const filteredTools = useMemo(() => {
    const needle = toolSearch.trim().toLowerCase();
    return (data?.tools || []).filter((tool) => {
      const matchTarget = targetFilter === "all" || tool.target_kind === targetFilter;
      const matchSearch =
        !needle ||
        `${tool.name} ${tool.description} ${tool.command_type || ""} ${tool.query_type || ""}`
          .toLowerCase()
          .includes(needle);
      return matchTarget && matchSearch;
    });
  }, [data?.tools, toolSearch, targetFilter]);

  // Filters for Threads list
  const filteredThreads = useMemo(() => {
    const needle = threadSearch.trim().toLowerCase();
    return (data?.threads || []).filter((row) => {
      if (!needle) return true;
      return `${row.thread.title} ${row.thread.thread_id} ${row.principal_id} ${row.thread.mode}`
        .toLowerCase()
        .includes(needle);
    });
  }, [data?.threads, threadSearch]);

  // Filters for Catalog tab
  const filteredCatalogTools = useMemo(() => {
    const needle = catalogSearch.trim().toLowerCase();
    return (data?.tools || []).filter((tool) => {
      const matchMode =
        catalogModeFilter === "all" || tool.default_modes.includes(catalogModeFilter);
      const matchSearch =
        !needle ||
        `${tool.name} ${tool.description} ${tool.command_type || ""} ${tool.query_type || ""} ${tool.target_kind}`
          .toLowerCase()
          .includes(needle);
      return matchMode && matchSearch;
    });
  }, [data?.tools, catalogSearch, catalogModeFilter]);

  const dirty = selectedRow ? !sameSet(selectedTools, selectedRow.initial_tool_set) : false;
  const activeGrantCount =
    selectedRow?.grants.filter((grant) => grant.state === "active").length || 0;

  // Stats for the top summary cards
  const stats = useMemo(() => {
    const healthyAdapters = (data?.adapters || []).filter(
      (a) => a.health_state === "healthy" || a.health_state === "ready",
    ).length;
    const totalGrants = (data?.threads || []).reduce(
      (acc, t) => acc + t.grants.filter((g) => g.state === "active").length,
      0,
    );
    const activeBindings = (data?.threads || []).filter((t) => t.active_binding !== null).length;

    return {
      mcpStatus: data?.mcp.status || "unavailable",
      mcpGrants: data?.mcp.active_grants || 0,
      skillsStatus: data?.skills.status || "unavailable",
      skillsGrants: data?.skills.active_grants || 0,
      healthyAdapters,
      totalAdapters: data?.adapters.length || 0,
      totalTools: data?.tools.length || 0,
      totalThreads: data?.threads.length || 0,
      activeBindings,
      totalGrants,
    };
  }, [data]);

  // Action: Save Thread Binding
  const saveBinding = async () => {
    if (!selectedRow) return;
    setSaving(true);
    setFeedback(null);
    try {
      const response = await apiFetch(
        `/api/capability-management/threads/${encodeURIComponent(selectedRow.thread.thread_id)}/binding`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            principal_id: selectedRow.principal_id,
            tool_set: selectedTools,
          }),
        },
      );
      if (!response.ok) throw new Error(await responseError(response));
      const result = (await response.json()) as {
        changed: boolean;
        binding: Binding;
        revoked_grants: number;
      };
      setFeedback({
        kind: "ok",
        text: result.changed
          ? `已创建 Binding v${result.binding.binding_version}，撤销 ${result.revoked_grants} 个旧 Grant。`
          : `配置未变化，继续使用 Binding v${result.binding.binding_version}。`,
      });
      await reload(true);
    } catch (error) {
      setFeedback({ kind: "bad", text: `保存失败：${String(error)}` });
    } finally {
      setSaving(false);
    }
  };

  // Action: Revoke Thread Binding
  const revokeBinding = async () => {
    if (!selectedRow) return;
    setSaving(true);
    try {
      const response = await apiFetch(
        `/api/capability-management/threads/${encodeURIComponent(selectedRow.thread.thread_id)}/revoke`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ principal_id: selectedRow.principal_id }),
        },
      );
      if (!response.ok) throw new Error(await responseError(response));
      const result = (await response.json()) as { binding: Binding; revoked_grants: number };
      setRevokeOpen(false);
      setFeedback({
        kind: "ok",
        text: `已撤销 Binding v${result.binding.binding_version} 及 ${result.revoked_grants} 个活动 Grant。`,
      });
      await reload(true);
    } catch (error) {
      setFeedback({ kind: "bad", text: `撤销失败：${String(error)}` });
    } finally {
      setSaving(false);
    }
  };

  // Action: Toggle MCP Server or Skill Resource
  const toggleResource = async (kind: "mcp" | "skills", resourceId: string, enabled: boolean) => {
    setSaving(true);
    setFeedback(null);
    try {
      const response = await apiFetch(
        `/api/capability-management/resources/${kind}/${encodeURIComponent(resourceId)}`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ enabled }),
        },
      );
      if (!response.ok) throw new Error(await responseError(response));
      const result = (await response.json()) as { command_id: string; applies_to: string };
      setFeedback({
        kind: "ok",
        text: `${resourceId} 已${enabled ? "启用" : "停用"}，作用于${
          result.applies_to === "new_sessions" ? "新会话" : "新 Worker"
        } · ${result.command_id}`,
      });
      await reload(true);
    } catch (error) {
      setFeedback({ kind: "bad", text: `更新资源失败：${String(error)}` });
    } finally {
      setSaving(false);
    }
  };

  // Quick tool selection actions
  const handleSelectAllVisibleTools = () => {
    const visibleNames = filteredTools.map((t) => t.name);
    setSelectedTools((prev) => Array.from(new Set([...prev, ...visibleNames])));
  };

  const handleClearVisibleTools = () => {
    const visibleNames = new Set(filteredTools.map((t) => t.name));
    setSelectedTools((prev) => prev.filter((name) => !visibleNames.has(name)));
  };

  const handleResetToModeTemplate = () => {
    if (!selectedRow || !data) return;
    const templateTools = data.mode_templates[selectedRow.thread.mode] || [];
    setSelectedTools([...templateTools]);
  };

  if (loading && !data) {
    return (
      <main className="capability-center">
        <div className="cap-loading-view">
          <span className="cap-loading-spinner">
            <Icon name="refresh" size={24} />
          </span>
          <strong>正在读取真实能力目录与运行时状态</strong>
        </div>
      </main>
    );
  }

  return (
    <main className="capability-center">
      {/* Top Toolbar — page title/intro owned by SettingsHub when hideIntro */}
      <header className={`cap-topbar${hideIntro ? " is-actions-only" : ""}`}>
        {hideIntro ? null : (
          <div>
            <h2 className="cap-topbar-title">能力</h2>
            <p className="cap-topbar-desc">Conversation Thread 授权与全局 MCP/Skills。这里不是 CTF Fact 图白名单。全局 Blackboard Skill 开关会影响新启动的 Worker。</p>
          </div>
        )}
        <div className="cap-topbar-actions">
          <Button
            type="button"
            className="cap-btn"
            isDisabled={loading || saving}
            onClick={() => void reload()}
            data-tooltip="刷新能力目录与运行时状态"
          >
            <Icon name="refresh" size={13} className={loading ? "cap-loading-spinner" : undefined} />
            <span>{loading ? "刷新中…" : "刷新状态"}</span>
          </Button>
        </div>
      </header>

      {/* Global Feedback Banner */}
      {feedback ? (
        <div className={`cap-feedback ${feedback.kind}`} role="status">
          <div className="cap-feedback-body">
            <Icon name={feedback.kind === "ok" ? "checkCircle" : "alert"} size={16} />
            <span>{feedback.text}</span>
          </div>
          <Button
            type="button"
            className="cap-feedback-close"
            onClick={() => setFeedback(null)}
            aria-label="关闭提示"
          >
            <Icon name="x" size={14} />
          </Button>
        </div>
      ) : null}

      {/* Summary Stats Deck */}
      <section className="cap-stats-deck" aria-label="能力运行态势概览">
        <article className="cap-stat-card">
          <div className="cap-stat-head">
            <span className="cap-stat-label">
              <Icon name="network" size={14} />
              MCP 网关
            </span>
            <StatePill state={stats.mcpStatus} />
          </div>
          <div className="cap-stat-value">
            {data?.mcp.servers.length || 0}
            <small style={{ fontSize: 11, color: "var(--muted)", fontWeight: "normal" }}>
              个服务
            </small>
          </div>
          <p className="cap-stat-sub">{stats.mcpGrants} 个活动 Grant · /api/capability</p>
        </article>

        <article className="cap-stat-card">
          <div className="cap-stat-head">
            <span className="cap-stat-label">
              <Icon name="terminal" size={14} />
              Skills 注入
            </span>
            <StatePill state={stats.skillsStatus} />
          </div>
          <div className="cap-stat-value">
            {data?.skills.items.length || 0}
            <small style={{ fontSize: 11, color: "var(--muted)", fontWeight: "normal" }}>
              个能力项
            </small>
          </div>
          <p className="cap-stat-sub">{stats.skillsGrants} 个活动 Grant · 跨 9 类引擎</p>
        </article>

        <article className="cap-stat-card">
          <div className="cap-stat-head">
            <span className="cap-stat-label">
              <Icon name="cpu" size={14} />
              引擎 Adapter
            </span>
            <span
              className={`cap-badge ${
                stats.healthyAdapters === stats.totalAdapters ? "success" : "primary"
              }`}
            >
              {stats.healthyAdapters}/{stats.totalAdapters} 正常
            </span>
          </div>
          <div className="cap-stat-value">
            {stats.totalAdapters}
            <small style={{ fontSize: 11, color: "var(--muted)", fontWeight: "normal" }}>
              类引擎
            </small>
          </div>
          <p className="cap-stat-sub">动态通道探活与会话注入</p>
        </article>

        <article className="cap-stat-card">
          <div className="cap-stat-head">
            <span className="cap-stat-label">
              <Icon name="layers" size={14} />
              Catalog 工具
            </span>
            <span className="cap-badge">已注册</span>
          </div>
          <div className="cap-stat-value">
            {stats.totalTools}
            <small style={{ fontSize: 11, color: "var(--muted)", fontWeight: "normal" }}>
              个工具
            </small>
          </div>
          <p className="cap-stat-sub">4 类执行目标 · 模式安全模板</p>
        </article>

        <article className="cap-stat-card">
          <div className="cap-stat-head">
            <span className="cap-stat-label">
              <Icon name="lock" size={14} />
              Thread 授权
            </span>
            <span className="cap-badge primary">{stats.activeBindings} 个已绑定</span>
          </div>
          <div className="cap-stat-value">
            {stats.totalThreads}
            <small style={{ fontSize: 11, color: "var(--muted)", fontWeight: "normal" }}>
              个会话
            </small>
          </div>
          <p className="cap-stat-sub">共 {stats.totalGrants} 个活动 Grant</p>
        </article>
      </section>

      {/* View Tabs */}
      <nav className="cap-tab-bar" aria-label="能力视图切换">
        <Button
          type="button"
          className={`cap-tab-btn ${activeTab === "threads" ? "active" : ""}`}
          onClick={() => setActiveTab("threads")}
        >
          <Icon name="lock" size={14} />
          <span>Thread 授权与 Binding</span>
          <span className="cap-tab-count">{data?.threads.length || 0}</span>
        </Button>

        <Button
          type="button"
          className={`cap-tab-btn ${activeTab === "services" ? "active" : ""}`}
          onClick={() => setActiveTab("services")}
        >
          <Icon name="network" size={14} />
          <span>协议与服务（MCP & Skills）</span>
          <span className="cap-tab-count">
            {(data?.mcp.servers.length || 0) + (data?.skills.items.length || 0)}
          </span>
        </Button>

        <Button
          type="button"
          className={`cap-tab-btn ${activeTab === "adapters" ? "active" : ""}`}
          onClick={() => setActiveTab("adapters")}
        >
          <Icon name="cpu" size={14} />
          <span>引擎 Adapter 接入</span>
          <span className="cap-tab-count">{data?.adapters.length || 0}</span>
        </Button>

        <Button
          type="button"
          className={`cap-tab-btn ${activeTab === "catalog" ? "active" : ""}`}
          onClick={() => setActiveTab("catalog")}
        >
          <Icon name="layers" size={14} />
          <span>工具清单与扩展能力</span>
          <span className="cap-tab-count">{data?.tools.length || 0}</span>
        </Button>
      </nav>

      {/* Tab Content 1: Thread Binding Workspace */}
      {activeTab === "threads" && (
        <section className="cap-threads-layout" aria-label="Thread 工具授权与绑定工作区">
          {/* Left: Thread Picker List */}
          <aside className="cap-thread-picker">
            <div className="cap-thread-picker-head">
              <span>选择 Thread / Principal</span>
              <span className="cap-badge">{filteredThreads.length} 个</span>
            </div>

            <div className="cap-search-input-wrap">
              <Icon name="search" size={13} />
              <input
                className="cap-search-input"
                value={threadSearch}
                onChange={(e) => setThreadSearch(e.target.value)}
                placeholder="搜索 Thread 标题或 ID…"
              />
            </div>

            <div className="cap-thread-list">
              {filteredThreads.map((row) => {
                const key = rowKey(row);
                const active = key === selectedKey;
                const grantCount = row.grants.filter((g) => g.state === "active").length;
                return (
                  <Button
                    key={key}
                    type="button"
                    className={`cap-thread-card ${active ? "active" : ""}`}
                    onClick={() => setSelectedKey(key)}
                  >
                    <div className="cap-thread-card-top">
                      <span className="cap-thread-card-title">{row.thread.title}</span>
                      <span className="cap-badge primary">{MODE_NAMES[row.thread.mode] || row.thread.mode}</span>
                    </div>
                    <div className="cap-thread-card-sub">
                      <span>{row.principal_id}</span>
                      <span>·</span>
                      <span>{row.active_binding ? `Binding v${row.active_binding.binding_version}` : "模式默认"}</span>
                    </div>
                    <div className="cap-thread-card-meta">
                      <span>{grantCount > 0 ? `${grantCount} 个活动 Grant` : "无活动 Grant"}</span>
                      <span>{fmt(row.thread.updated_at)}</span>
                    </div>
                  </Button>
                );
              })}

              {!filteredThreads.length && (
                <div className="cap-empty-state" style={{ padding: 20 }}>
                  <Icon name="search" size={18} />
                  <p>未找到匹配的 Thread</p>
                </div>
              )}
            </div>
          </aside>

          {/* Right: Binding & Tool Editor */}
          {selectedRow ? (
            <div className="cap-binding-editor">
              <header className="cap-binding-editor-head">
                <div className="cap-binding-editor-info">
                  <h3 className="cap-binding-editor-title">
                    <span>{selectedRow.thread.title}</span>
                    <span className="cap-badge primary">{MODE_NAMES[selectedRow.thread.mode] || selectedRow.thread.mode}</span>
                    <span className="cap-badge">
                      {selectedRow.active_binding
                        ? `v${selectedRow.active_binding.binding_version}`
                        : "模式默认模板"}
                    </span>
                    {activeGrantCount > 0 && (
                      <span className="cap-badge success">{activeGrantCount} 个活动 Grant</span>
                    )}
                  </h3>
                  <div className="cap-binding-editor-badges">
                    <span style={{ fontSize: 11, color: "var(--muted)", fontFamily: "var(--font-mono)" }}>
                      Principal: {selectedRow.principal_id} · Thread ID: {selectedRow.thread.thread_id}
                    </span>
                  </div>
                </div>

                <div className="cap-binding-editor-actions">
                  <Button
                    type="button"
                    className="cap-btn sm"
                    isDisabled={saving}
                    onClick={handleResetToModeTemplate}
                    aria-label="重置为当前模式预设的工具集合"
                  >
                    <Icon name="retry" size={12} />
                    <span>恢复模式模板</span>
                  </Button>
                  <Button
                    type="button"
                    className="cap-btn sm danger"
                    isDisabled={!selectedRow.active_binding || saving}
                    onClick={() => setRevokeOpen(true)}
                    data-tooltip="撤销当前 Thread 的整组 CapabilityBinding 与所有活动 Grants"
                  >
                    <Icon name="trash" size={12} />
                    <span>撤销整组</span>
                  </Button>
                </div>
              </header>

              {/* Tools Filter Bar */}
              <div className="cap-tools-toolbar">
                <div className="cap-target-filters">
                  <Button
                    type="button"
                    className={`cap-filter-pill ${targetFilter === "all" ? "active" : ""}`}
                    onClick={() => setTargetFilter("all")}
                  >
                    全部目标 ({data?.tools.length || 0})
                  </Button>
                  {["command", "query", "read_events", "wait", "receipt"].map((target) => (
                    <Button
                      key={target}
                      type="button"
                      className={`cap-filter-pill ${targetFilter === target ? "active" : ""}`}
                      onClick={() => setTargetFilter(target)}
                    >
                      {TARGET_NAMES[target] || target}
                    </Button>
                  ))}
                </div>

                <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                  <Button
                    type="button"
                    className="cap-btn sm"
                    onClick={handleSelectAllVisibleTools}
                    aria-label="选中当前筛选条件下的全部工具"
                  >
                    全选当前
                  </Button>
                  <Button
                    type="button"
                    className="cap-btn sm"
                    onClick={handleClearVisibleTools}
                    aria-label="取消选中当前筛选条件下的工具"
                  >
                    清空当前
                  </Button>
                  <div className="cap-search-input-wrap" style={{ width: 180 }}>
                    <Icon name="search" size={12} />
                    <input
                      className="cap-search-input"
                      style={{ minHeight: 28, fontSize: 10.5 }}
                      value={toolSearch}
                      onChange={(e) => setToolSearch(e.target.value)}
                      placeholder="搜索工具名或描述…"
                    />
                  </div>
                </div>
              </div>

              {/* Tool Cards Grid */}
              <div className="cap-tools-grid">
                {filteredTools.map((tool) => {
                  const isChecked = selectedTools.includes(tool.name);
                  return (
                    <label
                      key={tool.name}
                      className={`cap-tool-card ${isChecked ? "selected" : ""}`}
                    >
                      <Checkbox
                        aria-label={`选择 ${tool.name}`}
                        isSelected={isChecked}
                        isDisabled={saving}
                        onChange={(checked) => {
                          setSelectedTools((current) =>
                            checked
                              ? [...current, tool.name]
                              : current.filter((name) => name !== tool.name),
                          );
                        }}
                      ><Checkbox.Content><Checkbox.Control><Checkbox.Indicator /></Checkbox.Control></Checkbox.Content></Checkbox>
                      <div className="cap-tool-card-body">
                        <div className="cap-tool-card-head">
                          <span className="cap-tool-card-name">{tool.name}</span>
                          <span className="cap-badge">
                            {TARGET_NAMES[tool.target_kind] || tool.target_kind}
                          </span>
                        </div>
                        <p className="cap-tool-card-desc">{tool.description}</p>
                        <div className="cap-tool-card-footer">
                          {tool.command_type || tool.query_type ? (
                            <span>
                              映射: <code>{tool.command_type || tool.query_type}</code>
                            </span>
                          ) : null}
                          <span>来源: {tool.source}</span>
                          {tool.default_modes.length > 0 ? (
                            <span>
                              默认:{" "}
                              {tool.default_modes.map((m) => MODE_NAMES[m] || m).join(", ")}
                            </span>
                          ) : null}
                        </div>
                      </div>
                    </label>
                  );
                })}

                {!filteredTools.length && (
                  <div className="cap-empty-state" style={{ gridColumn: "1 / -1", padding: 24 }}>
                    <Icon name="search" size={20} />
                    <strong>未找到匹配的工具</strong>
                    <p>请尝试更换关键词或清除目标分类筛选</p>
                  </div>
                )}
              </div>

              {/* Save / Change Status Footer */}
              <footer className="cap-binding-footer">
                <div className={`cap-dirty-notice ${dirty ? "changed" : ""}`}>
                  <Icon name={dirty ? "pencil" : "check"} size={14} />
                  <span>
                    {dirty
                      ? `已修改工具列表（选中 ${selectedTools.length}/${data?.tools.length || 0} 个）· 未保存`
                      : `配置已同步（已选择 ${selectedTools.length} 个工具）`}
                  </span>
                </div>

                <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                  <Button
                    type="button"
                    className="cap-btn primary"
                    isDisabled={!dirty || saving}
                    onClick={() => void saveBinding()}
                  >
                    <Icon name="check" size={13} />
                    <span>{saving ? "写入中…" : "创建新 Binding 版本"}</span>
                  </Button>
                </div>
              </footer>

              {/* Grants and Versions History Disclosures */}
              <div style={{ display: "flex", flexDirection: "column", gap: 8, marginTop: 8 }}>
                <details className="cap-disclosure">
                  <summary>
                    <span>
                      活动与历史 Grants 授权凭据（{selectedRow.grants.length}）
                    </span>
                    <Icon name="chevronDown" size={14} />
                  </summary>
                  <div className="cap-disclosure-body">
                    {selectedRow.grants.map((grant) => (
                      <div key={grant.grant_id} className="cap-grant-item">
                        <div className="cap-grant-item-main">
                          <div className="cap-grant-item-title">
                            <code>{grant.grant_id}</code>
                            <span className="cap-badge">
                              {INJECTION_NAMES[grant.injection_kind] || grant.injection_kind}
                            </span>
                            <span style={{ fontSize: 10, color: "var(--dim)" }}>
                              会话: {grant.agent_session_id}
                            </span>
                          </div>
                          <div className="cap-grant-item-sub">
                            凭据: {grant.credential_reference || "无凭据引用"} · 签发于{" "}
                            {fmt(grant.issued_at)}
                            {grant.expires_at ? ` · 到期: ${fmt(grant.expires_at)}` : ""}
                          </div>
                        </div>
                        <StatePill state={grant.state} />
                      </div>
                    ))}
                    {!selectedRow.grants.length && (
                      <p style={{ margin: 0, padding: 8, color: "var(--dim)", fontSize: 11 }}>
                        当前没有已记录的 Grant。
                      </p>
                    )}
                  </div>
                </details>

                <details className="cap-disclosure">
                  <summary>
                    <span>
                      Binding 版本演进历史（{selectedRow.versions.length}）
                    </span>
                    <Icon name="chevronDown" size={14} />
                  </summary>
                  <div className="cap-disclosure-body">
                    {selectedRow.versions.map((ver) => (
                      <div key={ver.binding_id} className="cap-grant-item">
                        <div className="cap-grant-item-main">
                          <div className="cap-grant-item-title">
                            <span>Binding v{ver.binding_version}</span>
                            <code>{ver.binding_id}</code>
                          </div>
                          <div className="cap-grant-item-sub">
                            包含 {ver.tool_set.length} 个工具 · 创建于 {fmt(ver.created_at)}
                            {ver.revoked_at ? ` · 撤销于 ${fmt(ver.revoked_at)}` : ""}
                          </div>
                        </div>
                        <StatePill state={ver.revoked_at ? "revoked" : "active"} />
                      </div>
                    ))}
                  </div>
                </details>
              </div>
            </div>
          ) : (
            <div className="cap-empty-state">
              <Icon name="terminal" size={24} />
              <strong>尚未选择 Thread</strong>
              <p>请从左侧选择一个 Thread 查看或调整其工具授权 Binding。</p>
            </div>
          )}
        </section>
      )}

      {/* Tab Content 2: Services & Protocols (MCP & Skills) */}
      {activeTab === "services" && (
        <section className="cap-services-container" aria-label="服务与协议独立管理">
          {/* MCP Servers Section */}
          <div className="cap-services-block">
            <div>
              <h3 className="cap-block-title">
                <Icon name="network" size={16} />
                MCP Server 协议网关
              </h3>
              <p className="cap-block-desc">
                独立持久化启停状态。MCP 配置变更将在新建 Agent 会话时自动生效。
              </p>
            </div>

            <div className="cap-services-grid">
              {data?.mcp.servers.map((server) => (
                <article key={`mcp:${server.id}`} className="cap-service-card">
                  <div className="cap-service-card-head">
                    <div className="cap-service-card-brand">
                      <span className="cap-service-card-icon">
                        <Icon name="network" size={18} />
                      </span>
                      <div>
                        <h4 className="cap-service-card-title">{server.name}</h4>
                        <span className="cap-service-card-id">{server.id}</span>
                      </div>
                    </div>
                    <StatePill state={server.health} />
                  </div>

                  <dl className="cap-service-card-props">
                    <dt>接入端点</dt>
                    <dd><code>{server.endpoint}</code></dd>
                    <dt>协议版本</dt>
                    <dd>{server.protocol_version}</dd>
                    <dt>作用范围</dt>
                    <dd>{server.scope}</dd>
                    <dt>工具 / Grant</dt>
                    <dd>{server.tools} 个工具 · {server.active_grants} 个活动 Grant</dd>
                    <dt>生效周期</dt>
                    <dd>{server.lifecycle === "new_sessions" ? "新 Agent 会话" : server.lifecycle}</dd>
                  </dl>

                  <div className="cap-service-card-actions">
                    <span className="cap-service-lifecycle-note">
                      状态变更持久化到平台配置
                    </span>
                    <Button
                      type="button"
                      className={`cap-btn sm ${server.enabled ? "danger" : "primary"}`}
                      isDisabled={saving}
                      onClick={() => void toggleResource("mcp", server.id, !server.enabled)}
                    >
                      <Icon name={server.enabled ? "pause" : "play"} size={12} />
                      <span>{server.enabled ? "停用 MCP Server" : "启用 MCP Server"}</span>
                    </Button>
                  </div>
                </article>
              ))}
            </div>
          </div>

          {/* Skills Section */}
          <div className="cap-services-block">
            <div>
              <h3 className="cap-block-title">
                <Icon name="terminal" size={16} />
                Skills 注入能力
              </h3>
              <p className="cap-block-desc">
                Blackboard Skill 变更作用于新启动的 Worker 工作区；Capability Binding 技能由当前
                Thread Binding 规则动态派发。
              </p>
            </div>

            <div className="cap-services-grid">
              {data?.skills.items.map((skill) => (
                <article key={`skill:${skill.id}`} className="cap-service-card">
                  <div className="cap-service-card-head">
                    <div className="cap-service-card-brand">
                      <span className="cap-service-card-icon">
                        <Icon name="terminal" size={18} />
                      </span>
                      <div>
                        <h4 className="cap-service-card-title">{skill.name}</h4>
                        <span className="cap-service-card-id">{skill.id}</span>
                      </div>
                    </div>
                    <StatePill state={skill.health} />
                  </div>

                  <dl className="cap-service-card-props">
                    <dt>技能来源</dt>
                    <dd data-tooltip={skill.source}><code>{skill.source}</code></dd>
                    <dt>作用域</dt>
                    <dd>{skill.scope}</dd>
                    <dt>生命周期</dt>
                    <dd>{skill.lifecycle === "new_workers" ? "新 Worker" : skill.lifecycle === "binding_managed" ? "Thread Binding 托管" : skill.lifecycle}</dd>
                    <dt>旧用户副本</dt>
                    <dd>
                      {skill.legacy_copies.filter((c) => c.state !== "absent").length > 0
                        ? `${skill.legacy_copies.filter((c) => c.state !== "absent").length} 处已安装 (${skill.legacy_copies.map((c) => `${c.state}`).join(", ")})`
                        : "无残留副本"}
                    </dd>
                    {Object.keys(skill.engines).length > 0 && (
                      <>
                        <dt>适配引擎</dt>
                        <dd>{Object.keys(skill.engines).map((e) => ENGINE_NAMES[e] || e).join(", ")}</dd>
                      </>
                    )}
                  </dl>

                  <div className="cap-service-card-actions">
                    <span className="cap-service-lifecycle-note">
                      {skill.mutable ? "支持独立启停开关" : "由系统及 Thread Binding 自动管理"}
                    </span>
                    {skill.mutable ? (
                      <Button
                        type="button"
                        className={`cap-btn sm ${skill.enabled ? "danger" : "primary"}`}
                        isDisabled={saving}
                        onClick={() => void toggleResource("skills", skill.id, !skill.enabled)}
                      >
                        <Icon name={skill.enabled ? "pause" : "play"} size={12} />
                        <span>{skill.enabled ? "停用 Skill" : "启用 Skill"}</span>
                      </Button>
                    ) : (
                      <span className="cap-badge">由 Thread Binding 托管</span>
                    )}
                  </div>
                </article>
              ))}
            </div>
          </div>
        </section>
      )}

      {/* Tab Content 3: Engine Adapters Matrix */}
      {activeTab === "adapters" && (
        <section className="cap-adapters-grid" aria-label="九类引擎 Adapter 与注入探测">
          {data?.adapters.map((adapter) => (
            <article key={adapter.engine} className="cap-adapter-card">
              <header className="cap-adapter-head">
                <div className="cap-adapter-brand">
                  <span className="cap-adapter-brand-logo">
                    <EngineLogo
                      engine={adapter.engine}
                      size={20}
                      data-tooltip={ENGINE_NAMES[adapter.engine] || adapter.engine}
                    />
                  </span>
                  <div className="cap-adapter-titles">
                    <h4 className="cap-adapter-title">
                      {ENGINE_NAMES[adapter.engine] || adapter.engine}
                    </h4>
                    <span className="cap-adapter-id">
                      {adapter.adapter_id}:{adapter.instance_id}
                    </span>
                  </div>
                </div>
                <StatePill state={adapter.health_state} />
              </header>

              {/* Injection Channels Matrix */}
              <div className="cap-injection-matrix" aria-label="注入通道支持状态">
                {adapter.injection_support.map((item) => (
                  <span
                    key={item.kind}
                    className={`cap-injection-chip ${item.supported ? "supported" : ""}`}
                    data-tooltip={`来源: ${item.source}`}
                  >
                    <Icon name={item.supported ? "check" : "x"} size={10} />
                    <span>{INJECTION_NAMES[item.kind] || item.kind}</span>
                  </span>
                ))}
              </div>

              {/* Engine Parameters */}
              <dl className="cap-adapter-props">
                <dt>Transport</dt>
                <dd>{adapter.transport_kind}</dd>
                <dt>版本</dt>
                <dd>{adapter.runtime_version || "未探测"}</dd>
                <dt>来源</dt>
                <dd data-tooltip={adapter.source.join(" · ")}>{adapter.source.join(" · ") || "内置"}</dd>
              </dl>

              {/* Recent Injection Record */}
              <div className="cap-recent-injection">
                <div className="cap-recent-injection-head">
                  <span>最近会话注入</span>
                  {adapter.last_session_injection ? (
                    <StatePill state={adapter.last_session_injection.state} />
                  ) : (
                    <span style={{ fontSize: 9, color: "var(--dim)" }}>无记录</span>
                  )}
                </div>
                {adapter.last_session_injection ? (
                  <>
                    <div style={{ fontWeight: 600, color: "var(--bright)", fontSize: 10 }}>
                      {INJECTION_NAMES[adapter.last_session_injection.injection_kind] ||
                        adapter.last_session_injection.injection_kind}
                    </div>
                    <div className="cap-recent-injection-desc">
                      {fmt(adapter.last_session_injection.issued_at)} · {adapter.last_session_injection.agent_session_id}
                    </div>
                  </>
                ) : (
                  <span style={{ color: "var(--dim)", fontSize: 9.5 }}>尚无会话注入记录</span>
                )}
              </div>

              {/* Instance Probe Details Disclosure */}
              <details className="cap-disclosure">
                <summary>
                  <span>Runtime 实例明细（{adapter.instances.length}）</span>
                  <Icon name="chevronDown" size={12} />
                </summary>
                <div className="cap-disclosure-body">
                  {adapter.instances.map((instance) => (
                    <div
                      key={`${instance.adapter_id}:${instance.instance_id}`}
                      style={{
                        padding: "6px 8px",
                        border: "1px solid var(--line)",
                        borderRadius: 6,
                        background: "var(--panel)",
                        fontSize: 10,
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                        gap: 6,
                      }}
                    >
                      <div style={{ display: "flex", flexDirection: "column", gap: 1, minWidth: 0 }}>
                        <code style={{ fontSize: 9.5, color: "var(--bright)" }}>
                          {instance.adapter_id}:{instance.instance_id}
                        </code>
                        <span style={{ fontSize: 9, color: "var(--dim)" }}>
                          {instance.configured ? "已配置" : "自动发现"} · {instance.transport_kind}
                        </span>
                      </div>
                      <StatePill state={instance.health_state} />
                    </div>
                  ))}
                  {!adapter.instances.length && (
                    <p style={{ margin: 0, padding: 6, color: "var(--dim)", fontSize: 10 }}>
                      没有发现运行实例。
                    </p>
                  )}
                </div>
              </details>
            </article>
          ))}
        </section>
      )}

      {/* Tab Content 4: Catalog & Extensions */}
      {activeTab === "catalog" && (
        <section style={{ display: "flex", flexDirection: "column", gap: 16 }}>
          {/* Extension Sources */}
          <div className="cap-services-block">
            <div>
              <h3 className="cap-block-title">
                <Icon name="layers" size={16} />
                扩展能力来源（Extensions）
              </h3>
              <p className="cap-block-desc">
                展示已安装扩展在 Manifest 中声明的能力输出、Registry 状态与 Secret 引用安全审计。
              </p>
            </div>

            <div className="cap-extensions-grid">
              {data?.extensions.map((ext) => (
                <article key={ext.extension_id} className="cap-extension-card">
                  <div className="cap-extension-head">
                    <div>
                      <h4 className="cap-extension-id">{ext.extension_id}</h4>
                      <span className="cap-extension-sub">
                        {ext.version || "manifest unavailable"} · {ext.origin}
                      </span>
                    </div>
                    <StatePill state={ext.health || ext.state} />
                  </div>

                  {ext.manifest_error && (
                    <div
                      style={{
                        padding: "6px 8px",
                        borderRadius: 6,
                        background: "color-mix(in srgb, var(--red) 10%, var(--panel2))",
                        color: "var(--red)",
                        fontSize: 10.5,
                      }}
                    >
                      {ext.manifest_error}
                    </div>
                  )}

                  <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
                    <span style={{ fontSize: 10, color: "var(--muted)", fontWeight: 600 }}>
                      声明的能力输出
                    </span>
                    <div className="cap-extension-provides-list">
                      {ext.provides.map((prov) => (
                        <span key={`${prov.type}:${prov.id}`} className="cap-extension-provide-pill">
                          {prov.section} · {prov.type} · {prov.id} ({prov.registry_state})
                        </span>
                      ))}
                      {!ext.provides.length && (
                        <span style={{ color: "var(--dim)", fontSize: 10 }}>未声明能力输出</span>
                      )}
                    </div>
                  </div>

                  <div style={{ display: "flex", flexDirection: "column", gap: 4, marginTop: 4 }}>
                    <span style={{ fontSize: 10, color: "var(--muted)", fontWeight: 600 }}>
                      Secret 权限引用
                    </span>
                    <div className="cap-extension-secrets">
                      {ext.secret_references.map((sec) => (
                        <code
                          key={sec.reference}
                          style={{
                            padding: "2px 6px",
                            border: "1px solid var(--line)",
                            borderRadius: 4,
                            background: "var(--panel2)",
                            display: "inline-flex",
                            alignItems: "center",
                            gap: 6,
                          }}
                        >
                          <span>{sec.reference}</span>
                          <StatePill state={sec.status} />
                        </code>
                      ))}
                      {!ext.secret_references.length && (
                        <span style={{ color: "var(--dim)", fontSize: 10 }}>无 Secret 引用</span>
                      )}
                    </div>
                  </div>
                </article>
              ))}

              {!data?.extensions.length && (
                <div className="cap-empty-state" style={{ gridColumn: "1 / -1", padding: 24 }}>
                  <Icon name="layers" size={20} />
                  <strong>没有安装外部扩展</strong>
                  <p>当前运行环境下仅包含内置 MCP Server、Blackboard Skill 与标准 Catalog 工具。</p>
                </div>
              )}
            </div>
          </div>

          {/* Full Catalog Tools List */}
          <div className="cap-services-block">
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", flexWrap: "wrap", gap: 10 }}>
              <div>
                <h3 className="cap-block-title">
                  <Icon name="list" size={16} />
                  标准能力工具库（CapabilityCatalog）
                </h3>
                <p className="cap-block-desc">
                  所有受控工具必须在 Catalog 中注册。工具绑定与执行受 Thread Mode 模板和运行时策略保护。
                </p>
              </div>

              <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                <Select
                  aria-label="适用模式"
                  selectedKey={catalogModeFilter}
                  onSelectionChange={(key) => setCatalogModeFilter(String(key))}
                  className="cap-mode-select"
                >
                  <Select.Trigger><Select.Value /></Select.Trigger>
                  <Select.Popover><ListBox>
                    <ListBoxItem id="all">所有适用模式</ListBoxItem>
                    <ListBoxItem id="conversation">通用对话模式</ListBoxItem>
                    <ListBoxItem id="single_task">单题模式</ListBoxItem>
                    <ListBoxItem id="competition">比赛模式</ListBoxItem>
                    <ListBoxItem id="management">管理模式</ListBoxItem>
                  </ListBox></Select.Popover>
                </Select>

                <div className="cap-search-input-wrap" style={{ width: 220 }}>
                  <Icon name="search" size={13} />
                  <input
                    className="cap-search-input"
                    value={catalogSearch}
                    onChange={(e) => setCatalogSearch(e.target.value)}
                    placeholder="按名称、目标类型或命令搜索…"
                  />
                </div>
              </div>
            </div>

            <div className="cap-tools-grid">
              {filteredCatalogTools.map((tool) => (
                <div key={tool.name} className="cap-tool-card" style={{ cursor: "default" }}>
                  <div className="cap-tool-card-body">
                    <div className="cap-tool-card-head">
                      <span className="cap-tool-card-name">{tool.name}</span>
                      <span className="cap-badge primary">
                        {TARGET_NAMES[tool.target_kind] || tool.target_kind}
                      </span>
                    </div>
                    <p className="cap-tool-card-desc">{tool.description}</p>
                    <div className="cap-tool-card-footer">
                      {tool.command_type && (
                        <span>
                          写操作: <code>{tool.command_type}</code>
                        </span>
                      )}
                      {tool.query_type && (
                        <span>
                          查询: <code>{tool.query_type}</code>
                        </span>
                      )}
                      {tool.aggregate_type && (
                        <span>
                          聚合: <code>{tool.aggregate_type}</code>
                        </span>
                      )}
                      <span>来源: {tool.source}</span>
                      <span>
                        适用模式:{" "}
                        {tool.default_modes.map((m) => MODE_NAMES[m] || m).join(", ")}
                      </span>
                    </div>
                  </div>
                </div>
              ))}

              {!filteredCatalogTools.length && (
                <div className="cap-empty-state" style={{ gridColumn: "1 / -1", padding: 24 }}>
                  <Icon name="search" size={20} />
                  <strong>未找到匹配的 Catalog 工具</strong>
                </div>
              )}
            </div>
          </div>
        </section>
      )}

      {/* Revoke Whole Binding Group Confirmation Modal */}
      <Modal isOpen={revokeOpen} onOpenChange={setRevokeOpen}>
        <Modal.Backdrop isDismissable={!saving}>
          <Modal.Container><Modal.Dialog>
          <Modal.Header className="flex flex-col gap-1">
            <Modal.Heading>撤销当前 Thread 的整组能力？</Modal.Heading>
            {selectedRow ? <small className="font-normal text-foreground-500">{selectedRow.thread.title} · Principal: {selectedRow.principal_id}</small> : null}
          </Modal.Header>
          <Modal.Body><p>该操作将立即撤销当前 Thread 的活动 CapabilityBinding，所有由该 Binding 签发的未到期活动 Grant 将同步失效并从运行时解绑。后续如需恢复，可随时重新分配工具并创建新版本。</p></Modal.Body>
          <Modal.Footer>
            <Button variant="ghost" onPress={() => setRevokeOpen(false)} isDisabled={saving}>取消</Button>
            <Button variant="danger" isPending={saving} onPress={() => void revokeBinding()}>确认撤销 Binding 与 Grants</Button>
          </Modal.Footer>
          </Modal.Dialog></Modal.Container>
        </Modal.Backdrop>
      </Modal>
    </main>
  );
}
