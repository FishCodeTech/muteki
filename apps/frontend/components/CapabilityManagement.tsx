"use client";

import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useVirtualizer } from "@muteki/ui/engines/virtual";
import {
  Button,
  SearchInput,
  Select,
  Switch as CxSwitch,
  TabBar,
  toast,
} from "@muteki/ui";
import { EngineLogo } from "@/components/EngineLogo";
import { Icon } from "@muteki/ui";
import { SettingsDisclosure, SettingsNote, SettingsRow, SettingsSection } from "@/components/settings/primitives";
import { apiFetch } from "@/lib/useRun";
import { engineDisplayName, readyCatalog, useProviderDescriptors } from "@/lib/providerDescriptors";


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

type TabKey = "services" | "adapters" | "catalog";


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

const diagnosticDateFormat = new Intl.DateTimeFormat("zh-CN", {
  year: "numeric",
  month: "numeric",
  day: "numeric",
  hour: "numeric",
  minute: "numeric",
  second: "numeric",
  hour12: false,
});

function fmt(value?: string | null): string {
  if (!value) return "未记录";
  const time = Date.parse(value);
  return Number.isNaN(time) ? value : diagnosticDateFormat.format(time);
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


const RESOURCE_NAMES: Record<string, string> = {
  "muteki-browser": "浏览器",
  "computer-use": "电脑操控",
  "muteki-platform": "安全平台与工作台入口",
  "muteki-blackboard": "共享黑板",
  "agent-browser": "容器浏览器",
  "show-skills-in-slash": "/ 菜单显示 Skills",
};

function VirtualizedToolGrid({
  tools,
  rowEstimatePx,
  resetKey,
  renderCard,
}: {
  tools: ToolRow[];
  rowEstimatePx: number;
  resetKey: string;
  renderCard: (tool: ToolRow) => ReactNode;
}) {
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const [columns, setColumns] = useState<1 | 2>(2);

  useEffect(() => {
    const el = scrollRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const target = el.closest(".cx-settings-capabilities") ?? el;
    const update = (width: number) => {
      const next: 1 | 2 = width > 0 && width <= 620 ? 1 : 2;
      setColumns((prev) => (prev === next ? prev : next));
    };
    update(target.getBoundingClientRect().width);
    const observer = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (entry) update(entry.contentRect.width);
    });
    observer.observe(target);
    return () => observer.disconnect();
  }, []);

  const rowCount = Math.ceil(tools.length / columns);
  const virtualizer = useVirtualizer({
    count: rowCount,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowEstimatePx,
    overscan: 4,
    getItemKey: (rowIndex) =>
      `${columns}:${tools[rowIndex * columns]?.name ?? rowIndex}:${tools[rowIndex * columns + columns - 1]?.name ?? ""}`,
  });

  useEffect(() => {
    virtualizer.measure();
  }, [columns, virtualizer]);

  useEffect(() => {
    if (scrollRef.current) scrollRef.current.scrollTop = 0;
  }, [resetKey]);

  const virtualRows = virtualizer.getVirtualItems();

  return (
    <div ref={scrollRef} className="cap-tools-grid cap-virtual-scroll">
      {!tools.length ? (
        <div className="cap-empty-state" style={{ gridColumn: "1 / -1", padding: 24 }}>
          <p>没有匹配的工具</p>
        </div>
      ) : (
        <div style={{ height: virtualizer.getTotalSize(), position: "relative", width: "100%" }}>
          {virtualRows.map((virtualRow) => {
            const start = virtualRow.index * columns;
            const rowTools = tools.slice(start, start + columns);
            return (
              <div
                key={virtualRow.key}
                data-index={virtualRow.index}
                ref={virtualizer.measureElement}
                className="cap-tools-virtual-row"
                style={{
                  position: "absolute",
                  top: 0,
                  left: 0,
                  width: "100%",
                  transform: `translateY(${virtualRow.start}px)`,
                  gridTemplateColumns: `repeat(${columns}, minmax(0, 1fr))`,
                }}
              >
                {rowTools.map(renderCard)}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

export function CapabilityManagement({ hideIntro = false }: { hideIntro?: boolean }) {
  const descriptors = readyCatalog(useProviderDescriptors());
  const [data, setData] = useState<Overview | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [activeTab, setActiveTab] = useState<TabKey>("services");
  const [catalogSearch, setCatalogSearch] = useState("");
  const [catalogModeFilter, setCatalogModeFilter] = useState("all");
  const [error, setError] = useState("");

  const reload = useCallback(async (quiet = false) => {
    if (!quiet) setLoading(true);
    try {
      const response = await apiFetch("/api/capability-management", { cache: "no-store" });
      if (!response.ok) throw new Error(await responseError(response));
      const next = (await response.json()) as Overview;
      setData(next);
      setError("");
    } catch (cause) {
      setError(`读取能力状态失败：${String(cause)}`);
    } finally {
      if (!quiet) setLoading(false);
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

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

  const healthyAdapters = (data?.adapters || []).filter(
    (a) => a.health_state === "healthy" || a.health_state === "ready",
  ).length;
  const totalAdapters = data?.adapters.length || 0;

  const toggleResource = async (kind: "mcp" | "skills", resourceId: string, enabled: boolean) => {
    setSaving(true);
    setError("");
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
      toast({
        title: `已${enabled ? "启用" : "停用"}${RESOURCE_NAMES[resourceId] ?? resourceId}`,
        description: result.applies_to === "new_sessions" ? "新会话生效" : "新 Worker 生效",
        tone: "success",
      });
      await reload(true);
      window.dispatchEvent(new Event("muteki:chat-plugins-changed"));
    } catch (cause) {
      setError(`更新失败：${String(cause)}`);
    } finally {
      setSaving(false);
    }
  };

  if (loading && !data) {
    return (
      <main className="capability-center" aria-busy="true">
        <div className="cap-loading-view">
          <span className="cap-loading-spinner">
            <Icon name="refresh" size={24} />
          </span>
          <strong>正在读取能力状态</strong>
        </div>
      </main>
    );
  }

  const mcpMap = new Map((data?.mcp.servers || []).map((s) => [s.id, s]));
  const skillMap = new Map((data?.skills.items || []).map((s) => [s.id, s]));
  const browserOn = mcpMap.get("muteki-browser")?.enabled ?? true;
  const computerOn = (mcpMap.get("computer-use") ?? mcpMap.get("muteki-computer"))?.enabled ?? true;
  const platformOn = mcpMap.get("muteki-platform")?.enabled ?? true;
  const blackboardOn = skillMap.get("muteki-blackboard")?.enabled ?? true;
  const agentBrowserOn = skillMap.get("agent-browser")?.enabled ?? true;
  const showSkillsInSlash = skillMap.get("show-skills-in-slash")?.enabled ?? true;
  const switchDisabled = loading || saving;

  const resourceSwitch = (kind: "mcp" | "skills", id: string, checked: boolean) => (
    <CxSwitch
      ariaLabel={`启用${RESOURCE_NAMES[id] ?? id}`}
      checked={checked}
      disabled={switchDisabled}
      onCheckedChange={(on) => void toggleResource(kind, id, on)}
    />
  );

  return (
    <main className="capability-center cx-root" aria-busy={loading || saving}>
      {hideIntro ? null : (
        <header className="cap-topbar">
          <div>
            <h2 className="cap-topbar-title">内置能力</h2>
            <p className="cap-topbar-desc">管理内置浏览器、电脑操控、安全平台入口与 Worker 技能。</p>
          </div>
        </header>
      )}

      {error ? <SettingsNote tone="danger">{error}</SettingsNote> : null}

      <SettingsSection
        anchor="builtin-environment"
        title="对话工具"
        description="聊天中可用的内置工具，从下一轮对话生效。"
      >
        <SettingsRow
          title="浏览器"
          description="打开网页、读取内容、点击输入和截图。"
          control={resourceSwitch("mcp", "muteki-browser", browserOn)}
        />
        <SettingsRow
          title="电脑操控"
          description="在你授权的范围内操作 macOS 桌面应用。"
          control={resourceSwitch("mcp", "computer-use", computerOn)}
        />
      </SettingsSection>

      <SettingsSection anchor="builtin-platform" title="安全平台与工作台">
        <SettingsRow
          title="常驻平台入口"
          description="开启后聊天常驻 2 个轻量入口；关闭后只在使用 /ctf、/pentest、/workbench、@Run 等时加载。"
          control={resourceSwitch("mcp", "muteki-platform", platformOn)}
        />
      </SettingsSection>

      <SettingsSection anchor="builtin-worker-composer" title="Worker 与输入框">
        <SettingsRow
          title="共享黑板"
          description="新的 CTF 与渗透 Worker 通过黑板同步证据和进展。"
          control={resourceSwitch("skills", "muteki-blackboard", blackboardOn)}
        />
        <SettingsRow
          title="容器浏览器"
          description="新的渗透 Worker 可使用 agent-browser 访问目标并截图。"
          control={resourceSwitch("skills", "agent-browser", agentBrowserOn)}
        />
        <SettingsRow
          title="在 / 菜单中显示 Skills"
          description="默认开启，把 $ 菜单可见的全部 Skill 一并加入 / 菜单；关闭后仅在 $ 中显示。"
          control={resourceSwitch("skills", "show-skills-in-slash", showSkillsInSlash)}
        />
        <SettingsRow
          title="CTF 本地 Pi 单会话"
          description="单会话模式不使用这里的全局写权限 MCP，只加载 7 项 Pi 内置工作区工具、当前 Run 只读上下文和所选 CTF Skills。"
          control={<a href="/ctf/workers?section=local_pi" className="text-cx-sm text-cx-accent hover:underline">管理工具与 Skills →</a>}
        />
      </SettingsSection>

      <section className="flex flex-col gap-2">
        <h2 className="px-1 text-cx-base font-semibold text-cx-fg">高级</h2>
        <SettingsDisclosure
          anchor="builtin-diagnostics"
          icon="sliders"
          title="运行时诊断与工具目录"
          meta={`${healthyAdapters}/${totalAdapters} 引擎就绪`}
        >
          <div className="flex items-center justify-between gap-3 border-b border-cx-border-subtle px-3 pr-4">
            <TabBar<TabKey>
              ariaLabel="诊断视图"
              className="min-w-0 overflow-x-auto border-b-0"
              value={activeTab}
              onChange={setActiveTab}
              items={[
                { value: "services", label: "服务", count: (data?.mcp.servers.length || 0) + (data?.skills.items.length || 0) },
                { value: "adapters", label: "引擎", count: totalAdapters },
                { value: "catalog", label: "工具目录", count: data?.tools.length || 0 },
              ]}
            />
            <Button
              variant="ghost"
              size="sm"
              icon="refresh"
              loading={loading}
              disabled={loading || saving}
              onClick={() => void reload()}
            >
              刷新
            </Button>
          </div>

          <div className="flex flex-col gap-4 p-4">
            {activeTab === "services" && (
              <section className="cap-services-container" aria-label="MCP 与 Skills">
                <div className="cap-services-block">
                  <h3 className="cap-block-title">MCP 服务</h3>
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
                          <dt>端点</dt>
                          <dd><code>{server.endpoint}</code></dd>
                          <dt>协议</dt>
                          <dd>{server.protocol_version}</dd>
                          <dt>范围</dt>
                          <dd>{server.scope}</dd>
                          <dt>工具</dt>
                          <dd>{server.tools} 个</dd>
                        </dl>

                        <div className="cap-service-card-actions">
                          <span className="cap-service-lifecycle-note">
                            {server.lifecycle === "new_sessions" ? "新会话生效" : server.lifecycle}
                          </span>
                          <Button
                            variant="secondary"
                            size="sm"
                            disabled={saving}
                            onClick={() => void toggleResource("mcp", server.id, !server.enabled)}
                          >
                            {server.enabled ? "停用" : "启用"}
                          </Button>
                        </div>
                      </article>
                    ))}
                  </div>
                </div>

                <div className="cap-services-block">
                  <h3 className="cap-block-title">Skills</h3>
                  <div className="cap-services-grid">
                    {data?.skills.items.map((skill) => {
                      const copies = skill.legacy_copies.filter((c) => c.state !== "absent").length;
                      return (
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
                            <dt>来源</dt>
                            <dd data-tooltip={skill.source}><code>{skill.source}</code></dd>
                            <dt>范围</dt>
                            <dd>{skill.scope}</dd>
                            {copies > 0 ? (
                              <>
                                <dt>旧副本</dt>
                                <dd>{copies} 处</dd>
                              </>
                            ) : null}
                            {Object.keys(skill.engines).length > 0 && (
                              <>
                                <dt>引擎</dt>
                                <dd>{Object.keys(skill.engines).map((e) => engineDisplayName(descriptors, e)).join("、")}</dd>
                              </>
                            )}
                          </dl>

                          <div className="cap-service-card-actions">
                            <span className="cap-service-lifecycle-note">
                              {skill.lifecycle === "new_workers"
                                ? "新 Worker 生效"
                                : skill.lifecycle === "binding_managed"
                                  ? "由平台按需生效"
                                  : skill.lifecycle}
                            </span>
                            {skill.mutable ? (
                              <Button
                                variant="secondary"
                                size="sm"
                                disabled={saving}
                                onClick={() => void toggleResource("skills", skill.id, !skill.enabled)}
                              >
                                {skill.enabled ? "停用" : "启用"}
                              </Button>
                            ) : null}
                          </div>
                        </article>
                      );
                    })}
                  </div>
                </div>
              </section>
            )}

            {activeTab === "adapters" && (
              <section className="cap-adapters-grid" aria-label="引擎 Adapter">
                {data?.adapters.map((adapter) => (
                  <article key={adapter.engine} className="cap-adapter-card">
                    <header className="cap-adapter-head">
                      <div className="cap-adapter-brand">
                        <span className="cap-adapter-brand-logo">
                          <EngineLogo engine={adapter.engine} size={20} />
                        </span>
                        <div className="cap-adapter-titles">
                          <h4 className="cap-adapter-title">{engineDisplayName(descriptors, adapter.engine)}</h4>
                          <span className="cap-adapter-id">
                            {adapter.adapter_id}:{adapter.instance_id}
                          </span>
                        </div>
                      </div>
                      <StatePill state={adapter.health_state} />
                    </header>

                    <div className="cap-injection-matrix" aria-label="注入通道">
                      {adapter.injection_support.map((item) => (
                        <span
                          key={item.kind}
                          className={`cap-injection-chip ${item.supported ? "supported" : ""}`}
                          data-tooltip={`来源：${item.source}`}
                        >
                          <Icon name={item.supported ? "check" : "x"} size={10} />
                          <span>{INJECTION_NAMES[item.kind] || item.kind}</span>
                        </span>
                      ))}
                    </div>

                    <dl className="cap-adapter-props">
                      <dt>Transport</dt>
                      <dd>{adapter.transport_kind}</dd>
                      <dt>版本</dt>
                      <dd>{adapter.runtime_version || "未探测"}</dd>
                      <dt>最近注入</dt>
                      <dd>
                        {adapter.last_session_injection
                          ? `${INJECTION_NAMES[adapter.last_session_injection.injection_kind] || adapter.last_session_injection.injection_kind} · ${fmt(adapter.last_session_injection.issued_at)}`
                          : "无"}
                      </dd>
                    </dl>
                  </article>
                ))}
              </section>
            )}

            {activeTab === "catalog" && (
              <section className="flex flex-col gap-6">
                {data?.extensions.length ? (
                  <div className="cap-services-block">
                    <h3 className="cap-block-title">扩展</h3>
                    <div className="cap-extensions-grid">
                      {data.extensions.map((ext) => (
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
                          {ext.manifest_error ? (
                            <p className="rounded-lg bg-cx-danger-soft px-3 py-2 text-cx-xs text-cx-danger">{ext.manifest_error}</p>
                          ) : null}
                          <div className="cap-extension-provides-list">
                            {ext.provides.map((prov) => (
                              <span key={`${prov.type}:${prov.id}`} className="cap-extension-provide-pill">
                                {prov.type} · {prov.id}
                              </span>
                            ))}
                          </div>
                          {ext.secret_references.length > 0 ? (
                            <div className="cap-extension-secrets">
                              {ext.secret_references.map((sec) => (
                                <span key={sec.reference} className="inline-flex items-center gap-1.5">
                                  <code>{sec.reference}</code>
                                  <StatePill state={sec.status} />
                                </span>
                              ))}
                            </div>
                          ) : null}
                        </article>
                      ))}
                    </div>
                  </div>
                ) : null}

                <div className="cap-services-block">
                  <div className="flex flex-wrap items-center justify-between gap-3">
                    <h3 className="cap-block-title">工具目录</h3>
                    <div className="flex flex-wrap items-center gap-2 max-sm:w-full">
                      <Select
                        size="sm"
                        ariaLabel="适用模式"
                        className="h-8 w-[124px] max-sm:flex-1"
                        value={catalogModeFilter}
                        onChange={setCatalogModeFilter}
                        options={[
                          { value: "all", label: "全部模式" },
                          ...Object.entries(MODE_NAMES).map(([value, label]) => ({ value, label })),
                        ]}
                      />
                      <SearchInput
                        size="md"
                        className="w-[200px] max-sm:min-w-[160px] max-sm:flex-1"
                        value={catalogSearch}
                        onValueChange={setCatalogSearch}
                        placeholder="搜索工具"
                      />
                    </div>
                  </div>

                  <VirtualizedToolGrid
                    tools={filteredCatalogTools}
                    rowEstimatePx={114}
                    resetKey={`${catalogModeFilter}:${catalogSearch}`}
                    renderCard={(tool) => (
                      <div key={tool.name} className="cap-tool-card" style={{ cursor: "default" }}>
                        <div className="cap-tool-card-body">
                          <div className="cap-tool-card-head">
                            <span className="cap-tool-card-name">{tool.name}</span>
                            <span className="cap-badge">{TARGET_NAMES[tool.target_kind] || tool.target_kind}</span>
                          </div>
                          <p className="cap-tool-card-desc line-clamp-2" title={tool.description}>{tool.description}</p>
                          <div className="cap-tool-card-footer">
                            <span>{tool.default_modes.map((m) => MODE_NAMES[m] || m).join("、") || "无默认模式"}</span>
                            {tool.command_type || tool.query_type ? <code>{tool.command_type || tool.query_type}</code> : null}
                          </div>
                        </div>
                      </div>
                    )}
                  />
                </div>
              </section>
            )}
          </div>
        </SettingsDisclosure>
      </section>
    </main>
  );
}
