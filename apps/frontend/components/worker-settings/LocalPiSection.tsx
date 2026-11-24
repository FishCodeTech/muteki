"use client";

import Link from "next/link";
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Badge,
  Button,
  Callout,
  Collapse,
  IconButton,
  Input,
  Select,
  StatusDot,
  Switch,
  Tooltip,
  type ListOption,
  type Tone,
} from "@muteki/ui";
import { Icon } from "@muteki/ui";
import { SettingsRow, SettingsSection } from "@/components/settings/primitives";
import { SaveBar } from "@/components/worker-roster/SaveBar";
import {
  fetchLocalPiCatalog,
  fetchLocalPiProviderModels,
  probeLocalPiCompatibility,
  saveLocalPiSettings,
  type LocalPiCatalogResponse,
  type LocalPiProbeResult,
  type LocalPiProviderSave,
  type LocalPiSettings,
} from "@/lib/useRun";

const EXTENSIONS_HREF = "/settings/agent-extensions?mode=ctf&engine=pi";

const ACCESS_MODE_OPTIONS: ListOption<LocalPiSettings["default_access_mode"]>[] = [
  { value: "full-access", label: "自动执行" },
  { value: "auto-accept-edits", label: "自动允许文件编辑" },
  { value: "supervised", label: "每次命令审批" },
];

const PROVIDER_API_OPTIONS: ListOption<LocalPiProviderSave["api"]>[] = [
  { value: "openai-completions", label: "OpenAI 兼容" },
  { value: "anthropic-messages", label: "Anthropic" },
];

interface ProviderDraft extends LocalPiProviderSave {
  _key: string;
  _keyInput?: string;
  _clearKey?: boolean;
  _fetching?: boolean;
  _fetchError?: string;
  _fetched?: Array<{ id: string; name?: string }>;
  _manualModel?: string;
}

let providerDraftSeq = 0;
const nextDraftKey = () => `draft-${++providerDraftSeq}`;

const draftFromServer = (p: LocalPiProviderSave): ProviderDraft => ({ ...p, _key: p.id || nextDraftKey() });
const blankProvider = (): ProviderDraft => ({
  _key: nextDraftKey(),
  id: "",
  name: "",
  api: "openai-completions",
  base_url: "",
  models: [],
});

const providerDraftToPatch = (d: ProviderDraft): LocalPiProviderSave => {
  const out: LocalPiProviderSave = {
    ...(d.id ? { id: d.id } : {}),
    name: d.name,
    api: d.api,
    base_url: d.base_url,
    models: d.models,
  };
  if (d._keyInput?.trim()) out.api_key = d._keyInput.trim();
  else if (d._clearKey) out.clear_api_key = true;
  return out;
};

export function probeCheckTone(status?: string): Tone {
  if (status === "supported" || status === "reported" || status === "declared" || status === "verified") {
    return "success";
  }
  if (status === "failed") return "danger";
  return "warning";
}

export function probeCheckLabel(status?: string): string {
  switch (status) {
    case "supported":
      return "已支持";
    case "reported":
      return "已上报";
    case "declared":
      return "已声明";
    case "not_observed":
      return "未观测到分片";
    case "not_used":
      return "未触发调用";
    case "unknown":
      return "未知";
    case "failed":
      return "失败";
    default:
      return status || "未知";
  }
}

export function envCheckBadge(status?: string): { tone: Tone; label: string } {
  if (status === "verified") return { tone: "success", label: "已启用" };
  if (status === "disabled") return { tone: "neutral", label: "已关闭" };
  return { tone: "warning", label: "未验证" };
}

export function LocalPiSection({
  onNotice,
}: {
  onNotice?: (msg: string) => void;
}) {
  const [catalog, setCatalog] = useState<LocalPiCatalogResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [probing, setProbing] = useState(false);
  const [probeResult, setProbeResult] = useState<LocalPiProbeResult | null>(null);
  const [probeOpen, setProbeOpen] = useState(false);

  const [enabled, setEnabled] = useState(true);
  const [useAgentCredentials, setUseAgentCredentials] = useState(false);
  const [modelKey, setModelKey] = useState("");
  const [endpoint, setEndpoint] = useState("");
  const [accessMode, setAccessMode] = useState<LocalPiSettings["default_access_mode"]>("full-access");
  const [selectedSkills, setSelectedSkills] = useState<string[]>([]);
  const [offlineEnv, setOfflineEnv] = useState(true);
  const [providers, setProviders] = useState<ProviderDraft[]>([]);
  const [dirty, setDirty] = useState(false);

  const applySettings = useCallback((data: LocalPiCatalogResponse) => {
    if (!data.settings) return;
    setEnabled(data.settings.enabled ?? true);
    setUseAgentCredentials(data.settings.use_agent_credentials ?? false);
    const models = data.models ?? [];
    const savedModel = data.settings.default_model || "";
    const savedSource = data.settings.default_account_id || "";
    const match = models.find((m) => m.id === savedModel && (m.source_ref || "system_login") === (savedSource || m.source_ref || "system_login"))
      ?? models.find((m) => m.id === savedModel);
    setModelKey(match?.key ?? "");
    setEndpoint(data.settings.default_endpoint || "");
    setAccessMode(data.settings.default_access_mode || "full-access");
    setSelectedSkills(
      Array.isArray(data.settings.default_skills)
        ? data.settings.default_skills
        : data.skills.map((s) => s.id),
    );
    setOfflineEnv(data.settings.offline_env ?? true);
    setProviders((data.settings.providers ?? []).map(draftFromServer));
    setDirty(false);
  }, []);

  const loadCatalog = useCallback(async (preserveDraft = false) => {
    setLoading(true);
    try {
      const data = await fetchLocalPiCatalog();
      setCatalog(data);
      if (data.latest_probe) setProbeResult(data.latest_probe);
      if (!preserveDraft) applySettings(data);
    } catch {
      setCatalog(null);
    } finally {
      setLoading(false);
    }
  }, [applySettings]);

  useEffect(() => {
    void loadCatalog(false);
  }, [loadCatalog]);

  const discoveredModels = useMemo(() => catalog?.models ?? [], [catalog?.models]);
  const systemModels = useMemo(
    () => discoveredModels.filter((m) => (m.source_ref || "system_login") === "system_login"),
    [discoveredModels],
  );

  const selectedModel = useMemo(
    () => discoveredModels.find((m) => m.key === modelKey) ?? null,
    [discoveredModels, modelKey],
  );
  const defaultModelInvalid = Boolean(modelKey) && !selectedModel;

  const modelOptions: ListOption<string>[] = useMemo(
    () => discoveredModels.map((m) => ({
      value: m.key ?? m.id,
      label: `${m.label} · ${m.source_label || m.source_ref || m.source}`,
    })),
    [discoveredModels],
  );

  const modelMetaLine = useMemo(() => {
    const parts: string[] = [];
    const contextWindow = selectedModel?.context_window ?? probeResult?.checks?.context_window?.context_window;
    if (contextWindow != null) parts.push(`上下文 ${contextWindow.toLocaleString()} tokens`);
    const maxTokens = selectedModel?.max_tokens ?? probeResult?.checks?.context_window?.max_tokens;
    if (maxTokens != null) parts.push(`最大输出 ${maxTokens.toLocaleString()} tokens`);
    if (selectedModel?.parameter_size) {
      parts.push(`${selectedModel.parameter_size}${selectedModel.quantization_level ? ` · ${selectedModel.quantization_level}` : ""}`);
    }
    return parts.join(" · ");
  }, [selectedModel, probeResult]);

  const isSystemOllamaModel = (selectedModel?.source_ref || "system_login") === "system_login"
    && Boolean(selectedModel?.id.startsWith("ollama/"));

  const patchProvider = useCallback((key: string, patch: Partial<ProviderDraft>) => {
    setDirty(true);
    setProviders((prev) => prev.map((p) => (p._key === key ? { ...p, ...patch } : p)));
  }, []);

  const fetchProviderModels = useCallback(async (d: ProviderDraft) => {
    patchProvider(d._key, { _fetching: true, _fetchError: undefined });
    const res = await fetchLocalPiProviderModels({
      ...(d.id ? { provider_id: d.id } : {}),
      api: d.api,
      base_url: d.base_url.trim(),
      ...(d._keyInput?.trim() ? { api_key: d._keyInput.trim() } : {}),
    });
    if (res.ok) {
      patchProvider(d._key, { _fetching: false, _fetched: res.models ?? [] });
    } else {
      patchProvider(d._key, {
        _fetching: false,
        _fetched: undefined,
        _fetchError: res.code ? `${res.code}: ${res.detail || ""}` : res.detail || "获取失败",
      });
    }
  }, [patchProvider]);

  const handleSave = async () => {
    setSaving(true);
    try {
      const res = await saveLocalPiSettings({
        enabled,
        use_agent_credentials: useAgentCredentials,
        default_account_id: selectedModel?.source_ref || "system_login",
        default_model: selectedModel?.id || "",
        default_endpoint: endpoint.trim(),
        default_access_mode: accessMode,
        default_skills: selectedSkills,
        offline_env: offlineEnv,
        providers: providers.map(providerDraftToPatch),
      });
      if (res.ok) {
        setDirty(false);
        onNotice?.("本地 Pi 单会话默认配置已保存，新建本地 Pi 任务时自动应用。");
        await loadCatalog(false);
      } else {
        onNotice?.(`保存失败：${res.detail || "服务未返回原因"}`);
      }
    } finally {
      setSaving(false);
    }
  };

  const handleProbe = async () => {
    setProbing(true);
    try {
      const res = await probeLocalPiCompatibility({
        account_id: selectedModel?.source_ref || "system_login",
        model: selectedModel?.id || undefined,
        endpoint: endpoint.trim() || undefined,
      });
      if (res.probe) {
        setProbeResult(res.probe);
        setProbeOpen(true);
      }
      onNotice?.(res.ok ? res.detail || "本地 Pi 兼容性检查通过" : `兼容性检查未通过：${res.detail || "服务未返回原因"}`);
    } finally {
      setProbing(false);
    }
  };

  const runtime = catalog?.runtime;
  const probe = probeResult;
  const skills = catalog?.skills ?? [];

  return (
    <div className="flex min-h-full flex-col gap-8" data-testid="local-pi-settings-section">
      {!loading && !runtime ? (
        <Callout tone="danger" icon="alert" title="无法读取本地 Pi 状态，请确认服务在线后刷新" />
      ) : null}

      <SettingsSection title="本地 Pi 单会话">
        <SettingsRow
          title="启用本地 Pi 单会话"
          description="关闭后出题只使用蜂群模式。"
          control={
            <Switch
              checked={enabled}
              ariaLabel="启用本地 Pi 单会话"
              onCheckedChange={(next) => {
                setEnabled(next);
                setDirty(true);
              }}
            />
          }
        />
      </SettingsSection>

      {enabled ? (
        <>
          <SettingsSection
            title="运行状态"
            actions={
              <div className="flex items-center gap-2">
                <IconButton icon="refresh" label="刷新本地 Pi 状态" disabled={loading} onClick={() => void loadCatalog(true)} />
                <Button size="sm" variant="secondary" icon="activity" loading={probing} disabled={probing || !runtime?.ready} onClick={() => void handleProbe()}>
                  {probing ? "正在检查…" : "兼容性检查"}
                </Button>
              </div>
            }
          >
            <div className="px-5 py-3.5">
              <div className="flex flex-wrap items-center gap-x-6 gap-y-2 text-cx-xs">
                <span className="inline-flex items-center gap-1.5">
                  <StatusDot tone={runtime?.ready ? "success" : "danger"} />
                  <span className="text-cx-fg-3">Pi CLI</span>
                  <span className="font-medium text-cx-fg">
                    {runtime?.ready ? `已就绪${runtime.pi_version ? ` ${runtime.pi_version}` : ""}` : "未检测到 pi CLI"}
                  </span>
                </span>
                <Tooltip
                  content={
                    <span className="block max-w-[300px] whitespace-pre-wrap break-words">
                      {runtime?.ollama_status?.detail || "Ollama 状态未知"}
                      {runtime?.ollama_status?.endpoint ? `（${runtime.ollama_status.endpoint}）` : ""}
                    </span>
                  }
                >
                  <span tabIndex={0} className="inline-flex items-center gap-1.5 outline-none focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]">
                    <StatusDot tone={runtime?.ollama_status?.reachable ? "success" : "neutral"} />
                    <span className="text-cx-fg-3">Ollama</span>
                    <span className="font-medium text-cx-fg">
                      {runtime?.ollama_status?.reachable ? `在线 · ${runtime.ollama_status.model_count} 个模型` : "未连接"}
                    </span>
                  </span>
                </Tooltip>
                <button
                  type="button"
                  className="inline-flex items-center gap-1.5 text-left"
                  onClick={() => setProbeOpen((open) => !open)}
                  aria-expanded={probeOpen}
                >
                  <StatusDot tone={probe ? (probe.ok ? "success" : "danger") : "neutral"} />
                  <span className="text-cx-fg-3">兼容性检查</span>
                  <span className="font-medium text-cx-fg">
                    {probe
                      ? `${probe.ok ? "通过" : "未通过"}（${probe.served_model || probe.requested_model || "default"}）`
                      : "尚未检查"}
                  </span>
                  {probe ? <Icon name="chevronDown" size={11} className={probeOpen ? "rotate-180 transition-transform" : "transition-transform"} /> : null}
                </button>
              </div>
              <Collapse open={probeOpen && Boolean(probe)}>
                <div className="mt-3">{probe ? <ProbeResultPanel probe={probe} compact /> : null}</div>
              </Collapse>
            </div>
          </SettingsSection>

          <SettingsSection
            title="模型来源"
            description="本地 Pi 可选用的模型来自以下接入点。"
          >
            <div className="px-5 py-3.5">
              <div className="mb-2 text-cx-xs font-medium text-cx-fg-2">本机 Pi 配置</div>
              {systemModels.length ? (
                <ul className="flex flex-col gap-1 text-cx-xs">
                  {systemModels.map((m) => (
                    <li key={m.key ?? m.id} className="flex min-w-0 items-baseline gap-2">
                      <span className="truncate font-cx-mono text-cx-fg">{m.label}</span>
                      <span className="shrink-0 text-cx-fg-3">{m.source_label || "本机 Pi 配置"}</span>
                    </li>
                  ))}
                </ul>
              ) : (
                <div className="text-cx-xs text-cx-fg-3">本机未发现可用模型</div>
              )}
            </div>

            <SettingsRow
              title="读取 Agent 凭据中的 Pi 账号"
              description="开启后，Agent 凭据里配置的 Pi 账号及其模型也会出现在本地 Pi 的模型列表中。"
              control={
                <Switch
                  checked={useAgentCredentials}
                  ariaLabel="读取 Agent 凭据中的 Pi 账号"
                  onCheckedChange={(next) => {
                    setUseAgentCredentials(next);
                    setDirty(true);
                  }}
                />
              }
            />

            {providers.map((p, idx) => {
              const addedIds = new Set(p.models.map((m) => m.id));
              const newFetched = (p._fetched ?? []).filter((m) => !addedIds.has(m.id));
              return (
                <div key={p._key} className="border-t border-cx-border-subtle px-5 py-4">
                  <div className="mb-3 flex items-center justify-between gap-2">
                    <span className="text-cx-xs font-medium text-cx-fg-2">接入点 {idx + 1}</span>
                    <IconButton
                      icon="trash"
                      label="删除接入点"
                      onClick={() => {
                        setDirty(true);
                        setProviders((prev) => prev.filter((x) => x._key !== p._key));
                      }}
                    />
                  </div>
                  <div className="grid gap-3 @[640px]:grid-cols-2">
                    <Input
                      value={p.name}
                      placeholder="名称（如 Ollama 本机）"
                      aria-label="接入点名称"
                      onChange={(e) => patchProvider(p._key, { name: e.target.value })}
                    />
                    <Select<LocalPiProviderSave["api"]>
                      ariaLabel="接入点协议"
                      value={p.api}
                      options={PROVIDER_API_OPTIONS}
                      onChange={(next) => patchProvider(p._key, { api: next, _fetched: undefined, _fetchError: undefined })}
                    />
                  </div>
                  <div className="mt-3 grid gap-3 @[640px]:grid-cols-2">
                    <Input
                      value={p.base_url}
                      placeholder="Base URL（如 http://127.0.0.1:11434/v1）"
                      aria-label="接入点 Base URL"
                      className="font-cx-mono"
                      onChange={(e) => patchProvider(p._key, { base_url: e.target.value, _fetched: undefined, _fetchError: undefined })}
                    />
                    <div className="flex items-center gap-2">
                      <Input
                        type="password"
                        value={p._keyInput ?? ""}
                        placeholder={p.has_api_key ? "已保存，留空不修改" : "API Key（可留空）"}
                        aria-label="接入点 API Key"
                        className="font-cx-mono"
                        autoComplete="off"
                        onChange={(e) => patchProvider(p._key, { _keyInput: e.target.value, _clearKey: false })}
                      />
                      {p.has_api_key && !p._clearKey ? (
                        <Button size="sm" variant="ghost" onClick={() => patchProvider(p._key, { _clearKey: true, _keyInput: "" })}>
                          清除
                        </Button>
                      ) : null}
                      {p._clearKey ? (
                        <Badge tone="warning">保存后清除 Key</Badge>
                      ) : null}
                    </div>
                  </div>
                  <div className="mt-3 flex flex-wrap items-center gap-2">
                    <Button
                      size="md"
                      variant="secondary"
                      loading={p._fetching}
                      disabled={p._fetching || !p.base_url.trim()}
                      onClick={() => void fetchProviderModels(p)}
                    >
                      获取模型
                    </Button>
                    <div className="flex min-w-0 flex-1 items-center gap-2 @[480px]:max-w-72">
                      <Input
                        size="md"
                        value={p._manualModel ?? ""}
                        placeholder="手动添加模型 ID"
                        aria-label="手动添加模型 ID"
                        className="font-cx-mono"
                        onChange={(e) => patchProvider(p._key, { _manualModel: e.target.value })}
                      />
                      <Button
                        size="md"
                        variant="ghost"
                        disabled={!p._manualModel?.trim() || addedIds.has(p._manualModel.trim())}
                        onClick={() => {
                          const id = p._manualModel!.trim();
                          patchProvider(p._key, {
                            models: [...p.models, { id }],
                            _manualModel: "",
                          });
                        }}
                      >
                        添加
                      </Button>
                    </div>
                  </div>
                  {p._fetchError ? (
                    <div className="mt-2 text-cx-xs text-cx-danger">{p._fetchError}</div>
                  ) : null}
                  {newFetched.length ? (
                    <div className="mt-2 flex flex-col gap-1">
                      {newFetched.map((m) => (
                        <div key={m.id} className="flex items-center justify-between gap-2 rounded-lg border border-cx-border-subtle px-3 py-1.5 text-cx-xs">
                          <span className="min-w-0 truncate font-cx-mono text-cx-fg">{m.name && m.name !== m.id ? `${m.name}（${m.id}）` : m.id}</span>
                          <Button
                            size="sm"
                            variant="ghost"
                            onClick={() =>
                              patchProvider(p._key, {
                                models: [...p.models, { id: m.id, ...(m.name ? { name: m.name } : {}) }],
                                _fetched: (p._fetched ?? []).filter((x) => x.id !== m.id),
                              })
                            }
                          >
                            添加
                          </Button>
                        </div>
                      ))}
                    </div>
                  ) : null}
                  {p.models.length ? (
                    <div className="mt-2 flex flex-col gap-1">
                      {p.models.map((m) => (
                        <div key={m.id} className="flex items-center justify-between gap-2 rounded-lg bg-cx-bg px-3 py-1.5 text-cx-xs">
                          <span className="min-w-0 truncate font-cx-mono text-cx-fg">{m.name && m.name !== m.id ? `${m.name}（${m.id}）` : m.id}</span>
                          <IconButton
                            icon="x"
                            label={`移除模型 ${m.id}`}
                            onClick={() =>
                              patchProvider(p._key, { models: p.models.filter((x) => x.id !== m.id) })
                            }
                          />
                        </div>
                      ))}
                    </div>
                  ) : null}
                </div>
              );
            })}
            <div className="border-t border-cx-border-subtle px-5 py-3">
              <Button
                size="sm"
                variant="ghost"
                icon="plus"
                onClick={() => {
                  setDirty(true);
                  setProviders((prev) => [...prev, blankProvider()]);
                }}
              >
                添加接入点
              </Button>
            </div>
          </SettingsSection>

          <SettingsSection
            title="默认运行配置"
            description="新建“本地 Pi 单会话”任务时默认带入的配置。"
          >
            <SettingsRow
              title="默认模型"
              description="新建任务默认使用的模型与来源。"
              stacked
            >
              <div className="flex flex-col gap-2">
                <Select<string>
                  ariaLabel="默认 Pi 模型"
                  className="w-full max-w-72"
                  searchable
                  value={modelKey}
                  options={modelOptions}
                  onChange={(next) => {
                    setModelKey(next);
                    setDirty(true);
                  }}
                />
                {defaultModelInvalid ? (
                  <div className="text-cx-2xs text-cx-warning">保存的默认模型已不在模型列表中，请重新选择。</div>
                ) : null}
                {modelMetaLine ? <div className="text-cx-2xs text-cx-fg-3">{modelMetaLine}</div> : null}
              </div>
            </SettingsRow>

            {isSystemOllamaModel ? (
              <SettingsRow
                title="本地模型端点"
                description="仅对 ollama/ 模型生效，留空使用 Pi 默认地址。"
                control={
                  <Input
                    value={endpoint}
                    placeholder="例如 http://127.0.0.1:11434"
                    aria-label="本地模型端点"
                    className="w-full max-w-72 font-cx-mono"
                    onChange={(e) => {
                      setEndpoint(e.target.value);
                      setDirty(true);
                    }}
                  />
                }
              />
            ) : null}

            <SettingsRow
              title="执行权限"
              description="控制 Pi 在题目工作区内执行命令与编辑文件时的审批级别。"
              control={
                <Select<LocalPiSettings["default_access_mode"]>
                  ariaLabel="默认执行权限模式"
                  className="w-full max-w-72"
                  value={accessMode}
                  options={ACCESS_MODE_OPTIONS}
                  onChange={(next) => {
                    setAccessMode(next);
                    setDirty(true);
                  }}
                />
              }
            />

            <SettingsRow
              title="离线启动 (PI_OFFLINE=1)"
              description="启动时不刷新模型目录、不检查更新；不限制工具联网。"
              control={
                <Switch
                  checked={offlineEnv}
                  ariaLabel="注入 PI_OFFLINE=1"
                  onCheckedChange={(next) => {
                    setOfflineEnv(next);
                    setDirty(true);
                  }}
                />
              }
            />
          </SettingsSection>

          <SettingsSection
            title="默认 Skills"
            actions={
              <div className="flex items-center gap-2">
                <Badge tone="accent">已启用 {selectedSkills.length}/{skills.length}</Badge>
                <Button
                  size="sm"
                  variant="ghost"
                  disabled={!skills.length}
                  onClick={() => {
                    setDirty(true);
                    setSelectedSkills(selectedSkills.length === skills.length ? [] : skills.map((s) => s.id));
                  }}
                >
                  {selectedSkills.length === skills.length ? "全部取消" : "全部启用"}
                </Button>
              </div>
            }
          >
            <div className="flex flex-col">
              {skills.map((skill) => {
                const enabled = selectedSkills.includes(skill.id);
                const missing = skill.missing_dependencies ?? [];
                return (
                  <SettingsRow
                    key={skill.id}
                    title={skill.name}
                    description={skill.summary}
                    control={
                      <div className="flex items-center gap-2">
                        {missing.length ? (
                          <Tooltip
                            content={
                              <span className="block max-w-[300px] whitespace-pre-wrap break-words">
                                缺少命令：{missing.join(", ")}——安装后该 Skill 才可用；仍可开启，运行时调用会失败
                              </span>
                            }
                          >
                            <span tabIndex={0} aria-label={`缺少命令：${missing.join(", ")}`} className="inline-flex outline-none focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]">
                              <Badge tone="warning" icon="alert">缺少命令</Badge>
                            </span>
                          </Tooltip>
                        ) : null}
                        <Switch
                          checked={enabled}
                          ariaLabel={`默认启用 ${skill.name}`}
                          onCheckedChange={() => {
                            setDirty(true);
                            setSelectedSkills((prev) => (prev.includes(skill.id) ? prev.filter((id) => id !== skill.id) : [...prev, skill.id]));
                          }}
                        />
                      </div>
                    }
                  />
                );
              })}
              <div className="px-5 py-3 text-cx-xs text-cx-fg-3">
                扩展中心启用的 Skills 也会一并加载。
                <Link href={EXTENSIONS_HREF} className="ml-1 text-cx-accent underline">
                  管理扩展中心 →
                </Link>
              </div>
            </div>
          </SettingsSection>
        </>
      ) : (
        <div className="px-5 py-3 text-cx-xs text-cx-fg-3">关闭后出题只使用蜂群模式。</div>
      )}

      <SaveBar
        dirty={dirty}
        saving={saving}
        failed={false}
        onSave={() => void handleSave()}
        onDiscard={() => { if (catalog) applySettings(catalog); }}
      />
    </div>
  );
}

export function ProbeResultPanel({ probe, compact = false }: { probe: LocalPiProbeResult; compact?: boolean }) {
  const items = [
    { key: "text_response", title: "普通文本响应", status: probe.checks?.text_response?.status, detail: probe.checks?.text_response?.detail || probe.detail },
    { key: "streaming", title: "流式增量输出", status: probe.checks?.streaming?.status, detail: probe.checks?.streaming?.detail || "未知" },
    { key: "tool_call", title: "工具调用表示", status: probe.checks?.tool_call?.status, detail: probe.checks?.tool_call?.detail || "未知" },
    { key: "usage_reporting", title: "Token 用量上报", status: probe.checks?.usage_reporting?.status, detail: probe.checks?.usage_reporting?.detail || "未知（不填零）" },
    { key: "context_window", title: "上下文窗口信息", status: probe.checks?.context_window?.status, detail: probe.checks?.context_window?.detail || "未知（不按名称猜测）" },
  ];
  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2 rounded-xl border border-cx-border-subtle bg-cx-bg px-3.5 py-2.5 text-cx-xs">
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <StatusDot tone={probe.ok ? "success" : "danger"} />
          <span className="font-medium text-cx-fg">{probe.ok ? "最近一次兼容性检查通过" : "最近一次兼容性检查未通过"}</span>
          <span className="font-cx-mono text-cx-fg-3">模型：{probe.served_model || probe.requested_model || "default"}</span>
          {probe.endpoint ? <span className="truncate font-cx-mono text-cx-fg-3">端点：{probe.endpoint}</span> : null}
        </div>
        <div className="flex items-center gap-3 text-cx-fg-3">
          {probe.first_token_ms != null ? <span>首包 {probe.first_token_ms} ms</span> : null}
          <span>总耗时 {probe.elapsed_ms} ms</span>
        </div>
      </div>
      {!probe.ok && probe.detail ? <div className="text-cx-xs text-cx-danger">{probe.detail}</div> : null}
      <div className={compact ? "grid gap-2" : "grid gap-2.5 @[560px]:grid-cols-5"}>
        {items.map((item) => (
          <div key={item.key} className="flex flex-col justify-between gap-1.5 rounded-xl border border-cx-border-subtle bg-cx-bg p-3 text-cx-xs">
            <div className="flex items-center justify-between gap-1">
              <span className="font-medium text-cx-fg-2">{item.title}</span>
              <Badge tone={probeCheckTone(item.status)}>{probeCheckLabel(item.status)}</Badge>
            </div>
            <div className="text-cx-2xs leading-4 text-cx-fg-3">{item.detail}</div>
          </div>
        ))}
      </div>
    </div>
  );
}
