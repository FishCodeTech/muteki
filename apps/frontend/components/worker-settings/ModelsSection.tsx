"use client";

import { useMemo, useState } from "react";
import { Badge, Button, Callout, Collapse, IconButton, Select, Switch, TextField, type ListOption } from "@muteki/ui";
import { Icon } from "@muteki/ui";
import { ConversationRouteLink } from "@/components/conversation/ConversationNavigation";
import { ModelTestTerminal } from "@/components/ModelTestTerminal";
import { NumberField } from "@/components/NumberField";
import { SettingsNote, SettingsRow, SettingsSection } from "@/components/settings/primitives";
import { priceKey, priceLabel, useModelPrices } from "@/lib/modelPricing";
import {
  createModelEndpoint,
  testLlmEndpoint,
  type LlmProfile,
  type LlmTemperatureMode,
  type ModelEndpoint,
  type WorkerModelTestResult,
  type WorkerSettings,
} from "@/lib/useRun";

type LlmProfileName = "planner" | "titler";
type Profiles = WorkerSettings["llm_profiles"];
type ProfilePatch = Partial<Profiles["planner"]>;

const CUSTOM_ENDPOINT = "__custom__";
const EMPTY_DRAFT = { id: "", provider: "", base_url: "", api_key: "", model: "" };

const PURPOSE: Record<LlmProfileName, { title: string; description: string; role: string; testNote: string }> = {
  planner: {
    title: "规划与决策",
    description: "协调决策使用 Pi 账号；请先在 Agent 凭据中配置 Pi 账号，再选择端点。",
    role: "Reason / Planner",
    testNote: "连接测试检查 HTTP 服务；协调决策通过 Pi 运行。",
  },
  titler: {
    title: "标题生成",
    description: "对话与 CTF / 渗透任务自动标题；翻译与摘要复用此模型名。",
    role: "Titler",
    testNote: "对话与任务自动命名通过此端点调用；翻译与摘要使用 Planner 端点。",
  },
};

export function llmTemperatureMode(profile: LlmProfile): LlmTemperatureMode {
  const mode = profile.temperature_mode;
  return mode === "custom" || mode === "omit" || mode === "default" ? mode : "default";
}

function ProfileSection({
  profile, endpoints, which, onUpdate, onEndpointSaved,
}: {
  profile: Profiles[LlmProfileName];
  endpoints: ModelEndpoint[];
  which: LlmProfileName;
  onUpdate: (patch: ProfilePatch) => void;
  onEndpointSaved: (endpoint: ModelEndpoint) => void;
}) {
  const purpose = PURPOSE[which];
  const temperatureMode = llmTemperatureMode(profile);
  const customTemperature = temperatureMode !== "default";
  const selectedEndpoint = endpoints.find((endpoint) => endpoint.id === (profile.endpoint_id || profile.credential_id)) || null;
  const testSignature = [profile.endpoint_id || profile.credential_id || "", profile.model || "", temperatureMode, profile.temperature ?? ""].join("\u0000");
  const [testing, setTesting] = useState(false);
  const [customEndpoint, setCustomEndpoint] = useState(false);
  const [editingEndpoint, setEditingEndpoint] = useState(false);
  const [savingCustom, setSavingCustom] = useState(false);
  const [showCustomKey, setShowCustomKey] = useState(false);
  const [customError, setCustomError] = useState("");
  const [draft, setDraft] = useState(EMPTY_DRAFT);
  const [testState, setTestState] = useState<{ signature: string; result: WorkerModelTestResult } | null>(null);
  const testResult = testState?.signature === testSignature ? testState.result : null;

  const modelRows = useMemo(() => {
    const rows = new Map<string, string>();
    if (selectedEndpoint?.default_model) rows.set(selectedEndpoint.default_model, "端点默认模型");
    for (const model of selectedEndpoint?.models || []) if (!rows.has(model)) rows.set(model, "端点目录模型");
    if (profile.model && !rows.has(profile.model)) rows.set(profile.model, "当前已保存模型");
    return [...rows.entries()].map(([id, label]) => ({ id, label }));
  }, [profile.model, selectedEndpoint?.default_model, selectedEndpoint?.models]);
  const priceIdentity = (id: string) => ({ engine: "api", model: id, credential_id: selectedEndpoint?.id, provider_id: selectedEndpoint?.provider || profile.provider });
  const pricing = useModelPrices(modelRows.map((row) => priceIdentity(row.id)));
  const priceOf = (id: string) => priceLabel(pricing.values.get(priceKey(priceIdentity(id))), pricing.error, pricing.loading);

  const closeCustomEndpoint = () => {
    setCustomEndpoint(false);
    setEditingEndpoint(false);
    setDraft(EMPTY_DRAFT);
    setShowCustomKey(false);
    setCustomError("");
  };
  const beginEditEndpoint = () => {
    if (!selectedEndpoint) return;
    setCustomEndpoint(true);
    setEditingEndpoint(true);
    setCustomError("");
    setShowCustomKey(false);
    setDraft({
      id: String(selectedEndpoint.label || selectedEndpoint.id.replace(/^endpoint:/, "")).trim(),
      provider: selectedEndpoint.provider || "",
      base_url: selectedEndpoint.base_url || "",
      api_key: "",
      model: profile.model || selectedEndpoint.default_model || selectedEndpoint.models[0] || "",
    });
  };
  const chooseEndpoint = (endpointId: string) => {
    if (endpointId === CUSTOM_ENDPOINT) {
      setCustomEndpoint(true);
      setEditingEndpoint(false);
      setDraft(EMPTY_DRAFT);
      setShowCustomKey(false);
      setCustomError("");
      onUpdate({ endpoint_id: "", credential_id: undefined, model: "" });
      return;
    }
    closeCustomEndpoint();
    const next = endpoints.find((endpoint) => endpoint.id === endpointId);
    if (!next) {
      onUpdate({ endpoint_id: "", credential_id: undefined, model: "" });
      return;
    }
    const allowed = new Set([next.default_model || "", ...next.models].filter(Boolean));
    onUpdate({
      endpoint_id: next.id,
      credential_id: undefined,
      model: allowed.has(profile.model) ? profile.model : next.default_model || next.models[0] || "",
    });
  };
  const saveCustomEndpoint = async () => {
    if (savingCustom) return;
    const accountId = draft.id.trim();
    const baseUrl = draft.base_url.trim();
    const model = draft.model.trim();
    const apiKey = draft.api_key.trim();
    if (!accountId || !baseUrl || !model) return;
    if (!editingEndpoint && !apiKey) return;
    setSavingCustom(true);
    setCustomError("");
    const result = await createModelEndpoint({
      id: accountId,
      provider: draft.provider.trim(),
      base_url: baseUrl,
      ...(apiKey ? { api_key: apiKey } : {}),
      model,
    });
    setSavingCustom(false);
    if (!result.ok || !result.endpoint) {
      setCustomError(result.detail || "模型端点保存失败");
      return;
    }
    onEndpointSaved(result.endpoint);
    onUpdate({ endpoint_id: result.endpoint.id, credential_id: undefined, model });
    closeCustomEndpoint();
  };
  const runTest = async () => {
    if (!selectedEndpoint?.present || !profile.model.trim() || testing) return;
    const signature = testSignature;
    const startedAt = performance.now();
    setTesting(true);
    setTestState(null);
    try {
      const response = await testLlmEndpoint(which, selectedEndpoint.id, profile.model.trim(), temperatureMode, profile.temperature);
      const elapsedMs = Math.max(0, Math.round(performance.now() - startedAt));
      const detail = response.detail || (response.ok ? "模型已返回有效响应" : "模型测试未通过");
      setTestState({
        signature,
        result: {
          ok: response.ok,
          detail,
          model: response.model || profile.model,
          engine: "HTTP 模型 API",
          elapsed_ms: elapsedMs,
          tested_at: Date.now() / 1000,
          logs: [
            { stream: "command", message: `${which} · ${profile.model.trim()}`, elapsed_ms: 0 },
            { stream: response.ok ? "success" : "error", message: detail, elapsed_ms: elapsedMs },
          ],
        },
      });
    } finally {
      setTesting(false);
    }
  };

  const endpointOptions: ListOption<string>[] = [
    ...endpoints.map((endpoint) => ({
      value: endpoint.id,
      label: endpoint.label || endpoint.id,
      description: [endpoint.provider, endpoint.present ? "" : "不可用", which === "planner" && endpoint.worker_engine !== "pi" && endpoint.engine !== "pi" ? "Planner 需要 Pi 账号" : ""].filter(Boolean).join(" · ") || undefined,
      disabled: !endpoint.present || (which === "planner" && endpoint.worker_engine !== "pi" && endpoint.engine !== "pi"),
    })),
    ...(which === "planner" ? [] : [{ value: CUSTOM_ENDPOINT, label: "自定义模型端点…", icon: "plus" as const }]),
  ];
  const modelOptions: ListOption<string>[] = modelRows.map((row) => ({
    value: row.id,
    label: row.id,
    description: `${row.label} · ${priceOf(row.id)}`,
  }));
  const setDraftField = (key: keyof typeof EMPTY_DRAFT) => (event: { target: { value: string } }) =>
    setDraft((value) => ({ ...value, [key]: event.target.value }));
  const draftReady = draft.id.trim() && draft.base_url.trim() && draft.model.trim() && (editingEndpoint || draft.api_key.trim());
  const testLabel = testing ? "正在与模型交互…" : !selectedEndpoint?.present ? "请先选择可用端点" : !profile.model.trim() ? "请先选择模型" : "测试模型连接";

  return (
    <SettingsSection
      title={purpose.title}
      description={purpose.description}
      actions={<Badge tone={selectedEndpoint?.present ? "neutral" : "warning"}>{selectedEndpoint?.label || "未选择模型端点"}</Badge>}
    >
      <SettingsRow
        title="模型端点"
        description="用于调用模型的 HTTP 服务。"
        control={
          <Select<string>
            ariaLabel={`${purpose.title}模型端点`}
            className="w-56"
            searchable={endpoints.length > 6}
            placeholder="选择模型端点"
            value={customEndpoint && !editingEndpoint ? CUSTOM_ENDPOINT : selectedEndpoint?.id || null}
            onChange={chooseEndpoint}
            options={endpointOptions}
          />
        }
      />

      <Collapse open={customEndpoint}>
        <div className="flex flex-col gap-3 border-b border-cx-border-subtle bg-cx-bg-subtle px-5 py-4" aria-label={editingEndpoint ? "编辑模型端点" : "自定义模型端点"}>
          <p className="flex items-start gap-1.5 text-cx-xs leading-5 text-cx-fg-3">
            <Icon name="info" size={13} className="mt-1 shrink-0" />
            {editingEndpoint ? "端点修改立即保存；API key 留空则保留原值。" : `填写后会保存为独立模型端点，并立即用于当前 ${purpose.role}。`}
          </p>
          <div className="grid gap-3 @[560px]:grid-cols-2">
            <TextField label="端点名称" autoComplete="off" placeholder="例如 reason-gateway" value={draft.id} disabled={editingEndpoint} onChange={setDraftField("id")} />
            <TextField label="服务名称" autoComplete="off" placeholder="可选，例如 OpenAI Compatible" value={draft.provider} onChange={setDraftField("provider")} />
            <TextField label="Base URL" autoComplete="url" placeholder="https://api.example.com/v1" value={draft.base_url} onChange={setDraftField("base_url")} />
            <TextField
              label="API key"
              type={showCustomKey ? "text" : "password"}
              autoComplete="new-password"
              placeholder={editingEndpoint ? "留空则保留原 key" : "输入 API key"}
              value={draft.api_key}
              onChange={setDraftField("api_key")}
              trailing={
                <IconButton size="xs" icon={showCustomKey ? "eyeOff" : "eye"} label={showCustomKey ? "隐藏 API key" : "显示 API key"} onClick={() => setShowCustomKey((value) => !value)} />
              }
            />
            <TextField containerClassName="@[560px]:col-span-2" label="模型 ID" autoComplete="off" placeholder="例如 deepseek-v4-flash" value={draft.model} onChange={setDraftField("model")} />
          </div>
          {customError ? <Callout tone="danger" role="alert">{customError}</Callout> : null}
          <div className="flex items-center gap-2">
            <Button size="sm" variant="primary" icon="check" loading={savingCustom} disabled={savingCustom || !draftReady} onClick={() => void saveCustomEndpoint()}>
              {savingCustom ? "正在保存端点…" : editingEndpoint ? "保存修改" : "保存并选择此端点"}
            </Button>
            <Button size="sm" variant="ghost" disabled={savingCustom} onClick={closeCustomEndpoint}>取消</Button>
          </div>
        </div>
      </Collapse>

      {!customEndpoint ? (
        <>
          {selectedEndpoint ? (
            <SettingsRow
              title="连接详情"
              description={
                <span className="block truncate" title={selectedEndpoint.base_url}>
                  {which === "planner" ? "HTTP 端点" : "后端 HTTP 直连"} · {selectedEndpoint.base_url} · {selectedEndpoint.present ? "已配置" : "端点不可用"}
                </span>
              }
              control={<Button size="sm" variant="outline" icon="pencil" onClick={beginEditEndpoint}>编辑端点</Button>}
            />
          ) : (
            <div className="border-b border-cx-border-subtle px-5 py-3">
              <Callout tone="warning">选择已有端点，或在列表中添加自定义端点。</Callout>
            </div>
          )}
          <SettingsRow
            title="端点模型"
            description="从该端点已有的模型中选择。"
            control={
              <Select<string>
                ariaLabel={`${purpose.title}端点模型`}
                className="w-56"
                searchable={modelOptions.length > 8}
                placeholder="选择该端点已有模型"
                disabled={!selectedEndpoint?.present || modelOptions.length === 0}
                value={profile.model || null}
                onChange={(model) => onUpdate({ model })}
                options={modelOptions}
              />
            }
          />
        </>
      ) : null}

      <SettingsRow
        title="自定义 Temperature"
        description={which === "planner" ? "用于 Reason 与任务列表标题；关闭时使用默认值。" : "关闭时使用默认值。"}
        control={
          <Switch
            size="sm"
            ariaLabel={`${purpose.title}自定义 Temperature`}
            checked={customTemperature}
            onCheckedChange={(on) => onUpdate({ temperature_mode: on ? (temperatureMode === "omit" ? "omit" : "custom") : "default", temperature: profile.temperature ?? 1 })}
          />
        }
      />
      {customTemperature ? (
        <>
          <SettingsRow
            title="发送策略"
            description={temperatureMode === "omit" ? "不发送该参数，使用模型默认行为。" : "发送下面指定的数值。"}
            control={
              <Select<"custom" | "omit">
                ariaLabel={`${purpose.title}Temperature 发送策略`}
                className="w-44"
                value={temperatureMode === "omit" ? "omit" : "custom"}
                onChange={(mode) => onUpdate({ temperature_mode: mode })}
                options={[{ value: "custom", label: "发送指定值" }, { value: "omit", label: "不发送该参数" }]}
              />
            }
          />
          {temperatureMode !== "omit" ? (
            <SettingsRow
              title="Temperature"
              description="数值越高输出越发散，范围 0 到 2。"
              control={<NumberField ariaLabel={`${purpose.title} Temperature`} min={0} max={2} step={0.1} value={profile.temperature ?? 1} onChange={(next) => onUpdate({ temperature: Number(next) })} />}
            />
          ) : null}
        </>
      ) : null}

      <SettingsRow
        title="测试模型连接"
        description={purpose.testNote}
        control={
          <Button size="sm" variant="outline" icon="plug" loading={testing} disabled={!selectedEndpoint?.present || !profile.model.trim() || testing} onClick={() => void runTest()}>
            {testLabel}
          </Button>
        }
      >
        {testing || testResult ? (
          <ModelTestTerminal testing={testing} result={testResult} compact title={`${purpose.role} 测试终端`} runningMessage="正在向当前模型发送最小测试请求，请等待返回…" />
        ) : null}
      </SettingsRow>
    </SettingsSection>
  );
}

export function ModelsSection({
  value, endpoints, onChange, onEndpointSaved,
}: {
  value: Profiles;
  endpoints: ModelEndpoint[];
  onChange: (next: Profiles) => void;
  onEndpointSaved: (endpoint: ModelEndpoint) => void;
}) {
  const update = (which: LlmProfileName, patch: ProfilePatch) => onChange({ ...value, [which]: { ...value[which], ...patch } });
  return (
    <div className="flex flex-col gap-8">
      <SettingsNote>
        模型端点来自 Agent 接入点。
        <ConversationRouteLink className="ml-1 inline-flex items-center gap-0.5 text-cx-accent underline-offset-2 hover:underline" href="/settings/agents">
          管理接入点<Icon name="arrowUpRight" size={12} />
        </ConversationRouteLink>
      </SettingsNote>
      {(["planner", "titler"] as const).map((which) => (
        <ProfileSection key={which} which={which} profile={value[which]} endpoints={endpoints} onUpdate={(patch) => update(which, patch)} onEndpointSaved={onEndpointSaved} />
      ))}
    </div>
  );
}
