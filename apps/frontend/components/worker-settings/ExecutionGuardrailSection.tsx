"use client";

import { useCallback, useEffect, useState } from "react";
import { Badge, Button, Callout, Input, Select, StatusDot, Switch, type Tone } from "@muteki/ui";
import { SettingsNote, SettingsRow, SettingsSection } from "@/components/settings/primitives";
import { useLang } from "@/lib/i18n";
import {
  getExecutionGuardrailSettings,
  probeExecutionGuardrail,
  updateExecutionGuardrailSettings,
  type ExecutionGuardrailFailPolicy,
  type ExecutionGuardrailProbeResult,
  type ExecutionGuardrailSettings as GuardrailView,
} from "@/lib/useRun";

type Draft = {
  enabled: boolean;
  provider: string;
  endpoint: string;
  api_key: string;
  clear_api_key: boolean;
  timeout_ms: string;
  fail_policy: ExecutionGuardrailFailPolicy;
  read_proceed_min: string;
  scope_escalate_min: string;
  reject_deny_min: string;
};

function draftOf(view: GuardrailView): Draft {
  return {
    enabled: view.enabled,
    provider: view.provider || "cronus",
    endpoint: view.endpoint || "",
    api_key: "",
    clear_api_key: false,
    timeout_ms: String(view.timeout_ms),
    fail_policy: view.fail_policy,
    read_proceed_min: view.read_proceed_min.toFixed(2),
    scope_escalate_min: view.scope_escalate_min.toFixed(2),
    reject_deny_min: view.reject_deny_min.toFixed(2),
  };
}

function parseUnitFloat(raw: string): number | null {
  const trimmed = raw.trim();
  if (!trimmed) return null;
  const num = Number(trimmed);
  if (!Number.isFinite(num) || num < 0 || num > 1) return null;
  return num;
}

function parseTimeoutMs(raw: string): number | null {
  const trimmed = raw.trim();
  if (!trimmed) return null;
  const num = Number(trimmed);
  if (!Number.isFinite(num) || !Number.isInteger(num) || num < 200 || num > 120_000) return null;
  return num;
}

function formatHeadDetail(raw: unknown): string {
  if (raw == null) return "";
  if (typeof raw === "string") return raw;
  if (typeof raw === "object") {
    try {
      return JSON.stringify(raw);
    } catch {
      return String(raw);
    }
  }
  return String(raw);
}

export function ExecutionGuardrailSection({ onNotice }: { onNotice?: (message: string) => void } = {}) {
  const { lang } = useLang();
  const en = lang === "en";
  const t = (zh: string, english: string) => (en ? english : zh);

  const [view, setView] = useState<GuardrailView | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [saving, setSaving] = useState(false);
  const [probing, setProbing] = useState(false);
  const [probeResult, setProbeResult] = useState<ExecutionGuardrailProbeResult | null>(null);
  const [probeError, setProbeError] = useState("");

  const load = useCallback(async (signal?: AbortSignal) => {
    try {
      setError("");
      const data = await getExecutionGuardrailSettings(signal);
      setView(data);
      setDraft(draftOf(data));
      if (data.last_probe) setProbeResult(data.last_probe);
    } catch (cause) {
      if (!signal?.aborted) {
        setError(cause instanceof Error ? cause.message : String(cause));
      }
    }
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    void load(controller.signal);
    return () => controller.abort();
  }, [load]);

  if (!view || !draft) {
    return (
      <SettingsNote tone={error ? "danger" : "neutral"}>
        {error || t("正在读取执行护栏设置…", "Loading execution guardrail settings…")}
        {error ? (
          <Button className="ml-3" size="sm" variant="secondary" onClick={() => void load()}>
            {t("重试", "Retry")}
          </Button>
        ) : null}
      </SettingsNote>
    );
  }

  const parsedTimeout = parseTimeoutMs(draft.timeout_ms);
  const parsedReadMin = parseUnitFloat(draft.read_proceed_min);
  const parsedScopeMin = parseUnitFloat(draft.scope_escalate_min);
  const parsedRejectMin = parseUnitFloat(draft.reject_deny_min);
  const formValid = parsedTimeout !== null
    && parsedReadMin !== null
    && parsedScopeMin !== null
    && parsedRejectMin !== null;

  const dirty = draft.enabled !== view.enabled
    || draft.provider !== view.provider
    || draft.endpoint.trim() !== view.endpoint.trim()
    || Boolean(draft.api_key)
    || draft.clear_api_key
    || parsedTimeout !== view.timeout_ms
    || draft.fail_policy !== view.fail_policy
    || (parsedReadMin !== null && Math.abs(parsedReadMin - view.read_proceed_min) > 1e-6)
    || (parsedScopeMin !== null && Math.abs(parsedScopeMin - view.scope_escalate_min) > 1e-6)
    || (parsedRejectMin !== null && Math.abs(parsedRejectMin - view.reject_deny_min) > 1e-6);

  const update = (patch: Partial<Draft>) => {
    setDraft((current) => (current ? { ...current, ...patch } : current));
    setNotice("");
    setError("");
  };

  const buildPayload = () => ({
    enabled: draft.enabled,
    provider: draft.provider || "cronus",
    endpoint: draft.endpoint.trim(),
    ...(draft.api_key ? { api_key: draft.api_key } : {}),
    ...(draft.clear_api_key ? { clear_api_key: true } : {}),
    timeout_ms: parsedTimeout ?? view.timeout_ms,
    fail_policy: draft.fail_policy,
    read_proceed_min: parsedReadMin ?? view.read_proceed_min,
    scope_escalate_min: parsedScopeMin ?? view.scope_escalate_min,
    reject_deny_min: parsedRejectMin ?? view.reject_deny_min,
  });

  const save = async () => {
    if (!formValid || saving) return;
    setSaving(true);
    setError("");
    setNotice("");
    try {
      const next = await updateExecutionGuardrailSettings(buildPayload());
      setView(next);
      setDraft(draftOf(next));
      setProbeResult(next.last_probe);
      const savedMessage = t(
        "已保存执行护栏设置。后续开启「生产保护模式」的渗透测试将使用最新配置。",
        "Execution guardrail settings saved. Subsequent protected pentest runs will use this configuration.",
      );
      setNotice(savedMessage);
      onNotice?.(savedMessage);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSaving(false);
    }
  };

  const runProbe = async () => {
    if (probing) return;
    setProbing(true);
    setProbeError("");
    try {
      const result = await probeExecutionGuardrail({
        endpoint: draft.endpoint.trim() || undefined,
        timeout_ms: parsedTimeout ?? view.timeout_ms,
        ...(draft.api_key ? { api_key: draft.api_key } : {}),
        ...(draft.clear_api_key ? { clear_api_key: true } : {}),
      });
      setProbeResult(result);
      onNotice?.(
        result.healthy && result.decision_head_ok && result.scope_head_ok
          ? t("Cronus 双头自检通过。", "Cronus dual-head probe passed.")
          : t("Cronus 双头自检未完全通过，请检查下方详情。", "Cronus dual-head probe reported an issue."),
      );
    } catch (cause) {
      setProbeError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setProbing(false);
    }
  };

  const activeProbe = probeResult ?? view.last_probe;
  const bothHeadsOk = Boolean(activeProbe?.decision_head_ok && activeProbe?.scope_head_ok);
  const probeVerdict: { tone: Tone; label: string; detail: string } | null = activeProbe
    ? activeProbe.healthy && bothHeadsOk
      ? {
          tone: "success",
          label: t("服务健康 · 双头路由正常", "Healthy · Dual heads verified"),
          detail: t(
            "Cronus /healthz 与 /evaluate 双头自检均通过，decision 头与 scope 头均未退化为 W0。",
            "Both /healthz and /evaluate dual-head checks passed. Neither decision nor scope head degraded to W0.",
          ),
        }
      : activeProbe.healthy
        ? {
            tone: "warning",
            label: t("双头自检未完全通过（存在 W0 退化风险）", "Dual-head check degraded (W0 risk)"),
            detail: t(
              "服务在线，但 decision 头或 scope 头未全部返回绿色正常状态，请排查模型权重与头路由配置。",
              "Service responded, but at least one head did not pass verification. Check model weights and head routing.",
            ),
          }
        : {
            tone: "danger",
            label: t("服务不可用", "Unhealthy"),
            detail: activeProbe.error || t(
              "无法通过 Cronus 健康检查，生产保护模式下的动作将按 Fail-Closed 策略处理。",
              "Cronus health check failed. Actions in protected mode will follow the Fail-Closed policy.",
            ),
          }
    : null;

  return (
    <div className="flex flex-col gap-6">
      <Callout
        tone="info"
        title={t("作用域与引擎说明", "Scope & engine policy")}
      >
        {t(
          "执行护栏仅在渗透测试显式开启「生产保护模式」时介入，对普通非保护模式的 Pentest / CTF 与聊天完全无影响。开启生产保护模式后，任务将锁定仅使用内置二开 Muteki-Pi 多模型蜂群（所有 FGS 图协同角色与护栏均已内化为 Pi 插件，支持任意已配置 API 模型混编；外部订阅型 CLI 引擎仅在非保护模式可用）。",
          "Execution guardrails only apply to pentest runs with Production Protection Mode enabled; unprotected pentest, CTF, and chat sessions are completely unaffected. Protected runs are locked to the built-in Muteki-Pi multi-model swarm (external CLI engines remain available in unprotected mode).",
        )}
      </Callout>

      <SettingsSection
        anchor="execution-guardrail-connection"
        title={t("Cronus 护栏服务", "Cronus guardrail service")}
        description={t(
          "配置 Cronus-9B 实时风险评估服务端点、鉴权凭据与超时控制。",
          "Configure the Cronus-9B real-time risk evaluation endpoint, bearer token, and timeout.",
        )}
        actions={
          <Button
            size="sm"
            variant="primary"
            loading={saving}
            disabled={saving || !dirty || !formValid}
            onClick={() => void save()}
          >
            {t("保存设置", "Save settings")}
          </Button>
        }
      >
        <SettingsRow
          title={t("启用执行护栏", "Enable execution guardrail")}
          description={t(
            "开启后，渗透测试启动页可激活「生产保护模式」并由 Cronus-9B 对危险动作进行实时评估与门控。",
            "When enabled, pentest dispatch can activate Production Protection Mode with real-time Cronus-9B gating.",
          )}
          control={
            <Switch
              ariaLabel={t("启用执行护栏", "Enable execution guardrail")}
              checked={draft.enabled}
              onCheckedChange={(enabled) => update({ enabled })}
            />
          }
        />

        <SettingsRow
          title={t("Cronus 端点 URL", "Cronus endpoint URL")}
          description={t(
            "填写部署了 Cronus-9B 推理服务的 HTTP/HTTPS 根地址（默认参考 http://192.168.31.200:18971）。",
            "Base HTTP/HTTPS URL of the Cronus-9B evaluation service (e.g. http://192.168.31.200:18971).",
          )}
          control={
            <Input
              className="w-72 font-cx-mono"
              size="sm"
              aria-label={t("Cronus 端点 URL", "Cronus endpoint URL")}
              value={draft.endpoint}
              placeholder="http://192.168.31.200:18971"
              spellCheck={false}
              autoComplete="off"
              onChange={(event) => update({ endpoint: event.target.value })}
            />
          }
        />

        <SettingsRow
          title={t("Bearer Token", "Bearer token")}
          description={
            view.api_key_configured && !draft.clear_api_key
              ? t(
                  `已配置密钥${view.api_key_masked ? `（${view.api_key_masked}）` : ""}；留空保存不会覆盖原密钥。`,
                  `Token configured${view.api_key_masked ? ` (${view.api_key_masked})` : ""}. Leave blank to keep the existing token.`,
                )
              : draft.clear_api_key
                ? t("保存后将清空已保存的 Bearer Token。", "The saved Bearer token will be cleared on save.")
                : t("当 Cronus 服务端开启 auth_required 时必填；密钥仅加密/私有保存在服务端。", "Required when Cronus enables auth_required. Stored privately on the service host.")
          }
          control={
            <div className="flex flex-wrap items-center justify-end gap-2">
              {view.api_key_configured && !draft.clear_api_key ? (
                <Badge tone="success">
                  {view.api_key_masked || t("已配置", "Configured")}
                </Badge>
              ) : null}
              {draft.clear_api_key ? (
                <Badge tone="warning">{t("待清空", "Will clear")}</Badge>
              ) : null}
              <Input
                className="w-52 font-cx-mono"
                size="sm"
                type="password"
                autoComplete="new-password"
                aria-label={t("Cronus Bearer Token", "Cronus Bearer token")}
                value={draft.api_key}
                placeholder={
                  view.api_key_configured && !draft.clear_api_key
                    ? t("留空保持不变", "Leave empty to keep")
                    : t("输入 Bearer Token", "Enter bearer token")
                }
                onChange={(event) => update({ api_key: event.target.value, clear_api_key: false })}
              />
              {view.api_key_configured && !draft.clear_api_key ? (
                <Button
                  size="xs"
                  variant="ghost"
                  onClick={() => update({ api_key: "", clear_api_key: true })}
                >
                  {t("清空", "Clear")}
                </Button>
              ) : draft.clear_api_key ? (
                <Button
                  size="xs"
                  variant="ghost"
                  onClick={() => update({ clear_api_key: false })}
                >
                  {t("撤销清空", "Undo")}
                </Button>
              ) : null}
            </div>
          }
        />

        <SettingsRow
          title={t("评估超时（ms）", "Evaluation timeout (ms)")}
          description={t(
            "单次 Cronus 评估等待上限（200 – 120000 ms）；超时将按 Fail-Closed 策略拦截。",
            "Maximum wait per Cronus evaluation (200 – 120000 ms). Timeouts trigger the Fail-Closed policy.",
          )}
          control={
            <div className="flex items-center gap-2">
              <Input
                className="w-28 text-right font-cx-mono cx-tabular"
                size="sm"
                type="number"
                min={200}
                max={120000}
                step={100}
                invalid={parsedTimeout === null}
                aria-label={t("评估超时（ms）", "Evaluation timeout (ms)")}
                value={draft.timeout_ms}
                onChange={(event) => update({ timeout_ms: event.target.value })}
              />
              <span className="text-cx-xs text-cx-fg-3">ms</span>
            </div>
          }
        />
      </SettingsSection>

      <SettingsSection
        anchor="execution-guardrail-policy"
        title={t("Fail-Closed 与判定阈值", "Fail-Closed policy & thresholds")}
        description={t(
          "定义 Cronus 不可用时的兜底动作以及校准概率门控边界（范围 0.00 – 1.00）。",
          "Define fallback behavior when Cronus is unreachable and calibrated probability thresholds (0.00 – 1.00).",
        )}
      >
        <SettingsRow
          title={t("Fail-Closed 策略", "Fail-Closed policy")}
          description={
            draft.fail_policy === "fail_closed_ask"
              ? t(
                  "Cronus 超时或异常时，暂停该动作并推送到人工审批队列，绝不静默放行。",
                  "When Cronus times out or fails, hold the action for human approval instead of allowing silently.",
                )
              : t(
                  "Cronus 超时或异常时，立即拒绝该动作并返回拦截回执。",
                  "When Cronus times out or fails, deny the action immediately.",
                )
          }
          control={
            <Select<ExecutionGuardrailFailPolicy>
              ariaLabel={t("Fail-Closed 策略", "Fail-Closed policy")}
              className="w-60"
              value={draft.fail_policy}
              onChange={(fail_policy) => update({ fail_policy })}
              options={[
                { value: "fail_closed_ask", label: t("fail_closed_ask · 转人工审批", "fail_closed_ask · Escalate to human") },
                { value: "fail_closed_deny", label: t("fail_closed_deny · 直接拒绝", "fail_closed_deny · Deny immediately") },
              ]}
            />
          }
        />

        <SettingsRow
          title={t("只读放行阈值（read_proceed_min）", "Read auto-proceed threshold (read_proceed_min)")}
          description={t(
            "只读探测动作需要达到的最小 proceed 校准概率（默认 0.90）；低于该值则升级人工审批。",
            "Minimum calibrated proceed probability required to auto-allow read actions (default 0.90).",
          )}
          control={
            <Input
              className="w-28 text-right font-cx-mono cx-tabular"
              size="sm"
              type="number"
              min={0}
              max={1}
              step={0.01}
              invalid={parsedReadMin === null}
              aria-label="read_proceed_min"
              value={draft.read_proceed_min}
              onChange={(event) => update({ read_proceed_min: event.target.value })}
            />
          }
        />

        <SettingsRow
          title={t("越界升级阈值（scope_escalate_min）", "Out-of-scope escalation threshold (scope_escalate_min)")}
          description={t(
            "scope 头判定越界概率达到或超过该值时强制转人工审批（默认 0.20）。",
            "Escalate to human approval when scope head probability reaches or exceeds this threshold (default 0.20).",
          )}
          control={
            <Input
              className="w-28 text-right font-cx-mono cx-tabular"
              size="sm"
              type="number"
              min={0}
              max={1}
              step={0.01}
              invalid={parsedScopeMin === null}
              aria-label="scope_escalate_min"
              value={draft.scope_escalate_min}
              onChange={(event) => update({ scope_escalate_min: event.target.value })}
            />
          }
        />

        <SettingsRow
          title={t("自动拒绝阈值（reject_deny_min）", "Auto-deny threshold (reject_deny_min)")}
          description={t(
            "decision 头 reject 概率达到或超过该值时直接阻断危险动作（默认 0.85）。",
            "Immediately deny high-risk actions when decision head reject probability meets or exceeds this threshold (default 0.85).",
          )}
          control={
            <Input
              className="w-28 text-right font-cx-mono cx-tabular"
              size="sm"
              type="number"
              min={0}
              max={1}
              step={0.01}
              invalid={parsedRejectMin === null}
              aria-label="reject_deny_min"
              value={draft.reject_deny_min}
              onChange={(event) => update({ reject_deny_min: event.target.value })}
            />
          }
        />
      </SettingsSection>

      {!formValid ? (
        <SettingsNote tone="danger">
          {t("请检查超时（200–120000 ms）和概率阈值（0.00–1.00）输入是否有效。", "Check that timeout (200–120000 ms) and probability thresholds (0.00–1.00) are valid.")}
        </SettingsNote>
      ) : null}
      {error ? <SettingsNote tone="danger">{error}</SettingsNote> : null}
      {notice ? <SettingsNote>{notice}</SettingsNote> : null}

      <SettingsSection
        anchor="execution-guardrail-probe"
        title={t("连接测试与双头自检", "Connection & dual-head self-check")}
        description={t(
          "实探 Cronus /healthz 与 /evaluate 双头路由，确认 decision 头与 scope 头均已正确加载且未退化为 W0。",
          "Probes Cronus /healthz and /evaluate to verify both decision and scope heads are loaded without W0 degradation.",
        )}
        actions={
          <Button
            size="sm"
            variant="secondary"
            icon="refresh"
            loading={probing}
            disabled={probing}
            onClick={() => void runProbe()}
          >
            {probing ? t("自检中…", "Probing…") : t("测试连接与双头自检", "Test connection & dual-head probe")}
          </Button>
        }
      >
        {probeVerdict ? (
          <SettingsRow
            title={t("综合状态", "Overall status")}
            description={probeVerdict.detail}
            control={<Badge tone={probeVerdict.tone}>{probeVerdict.label}</Badge>}
          />
        ) : null}

        {activeProbe ? (
          <>
            <SettingsRow
              title={t("服务延迟与端点", "Latency & endpoint")}
              description={activeProbe.endpoint || draft.endpoint || "—"}
              control={
                <span className="font-cx-mono text-cx-xs text-cx-fg-2 cx-tabular">
                  {activeProbe.latency_ms} ms
                </span>
              }
            />
            <SettingsRow
              title={t("模型与清单摘要", "Model & manifest")}
              description={
                <div className="flex flex-col gap-0.5 font-cx-mono text-cx-xs text-cx-fg-3">
                  <span>model_name: {activeProbe.model_name || "—"}</span>
                  <span className="break-all" title={activeProbe.manifest_sha256 || undefined}>
                    manifest_sha256: {activeProbe.manifest_sha256 || "—"}
                  </span>
                </div>
              }
              control={
                <div className="flex flex-wrap items-center justify-end gap-1.5">
                  <Badge tone={activeProbe.auth_required ? "success" : "warning"}>
                    auth_required: {activeProbe.auth_required === null || activeProbe.auth_required === undefined ? "—" : String(activeProbe.auth_required)}
                  </Badge>
                  {typeof activeProbe.allow_cache_reset === "boolean" ? (
                    <Badge tone={activeProbe.allow_cache_reset ? "warning" : "neutral"}>
                      allow_cache_reset: {String(activeProbe.allow_cache_reset)}
                    </Badge>
                  ) : null}
                  {typeof activeProbe.python_debug === "boolean" ? (
                    <Badge tone={activeProbe.python_debug ? "danger" : "neutral"}>
                      python_debug: {String(activeProbe.python_debug)}
                    </Badge>
                  ) : null}
                </div>
              }
            />
            <SettingsRow
              title={t("decision 头路由状态", "decision head routing")}
              description={
                formatHeadDetail(activeProbe.decision_head)
                  ? <code className="break-all font-cx-mono text-cx-xs">{formatHeadDetail(activeProbe.decision_head)}</code>
                  : t("负责评估动作 proceed / ask / reject 校准概率。", "Evaluates proceed / ask / reject calibrated probabilities.")
              }
              control={
                <Badge tone={activeProbe.decision_head_ok ? "success" : "danger"}>
                  <StatusDot tone={activeProbe.decision_head_ok ? "success" : "danger"} className="mr-1.5" />
                  {activeProbe.decision_head_ok ? t("正常（非 W0）", "OK") : t("异常 / 退化", "Failed")}
                </Badge>
              }
            />
            <SettingsRow
              title={t("scope 头路由状态", "scope head routing")}
              description={
                formatHeadDetail(activeProbe.scope_head)
                  ? <code className="break-all font-cx-mono text-cx-xs">{formatHeadDetail(activeProbe.scope_head)}</code>
                  : t("负责评估动作是否越出授权目标范围（scope p_true）。", "Evaluates out-of-scope probability (scope p_true).")
              }
              control={
                <Badge tone={activeProbe.scope_head_ok ? "success" : "danger"}>
                  <StatusDot tone={activeProbe.scope_head_ok ? "success" : "danger"} className="mr-1.5" />
                  {activeProbe.scope_head_ok ? t("正常（非 W0）", "OK") : t("异常 / 退化", "Failed")}
                </Badge>
              }
            />
          </>
        ) : (
          <SettingsRow
            title={t("尚未执行自检", "Not probed yet")}
            description={t(
              "点击「测试连接与双头自检」验证 Cronus 服务连通性与 decision / scope 双头路由（两者均绿色才说明未退化为 W0）。",
              "Select Test connection & dual-head probe to check Cronus connectivity and confirm both heads are green.",
            )}
          />
        )}

        {probeError || activeProbe?.error ? (
          <div className="px-5 py-3">
            <Callout tone="danger" title={t("探测异常", "Probe error")}>
              {probeError || activeProbe?.error}
            </Callout>
          </div>
        ) : null}
      </SettingsSection>

      <SettingsNote>
        {t(
          "提示：仅当 decision 头与 scope 头均显示绿色正常时，才说明双头路由完整、未退化为 W0。非保护模式任务不调用护栏接口。",
          "Note: Both decision and scope heads must be green to confirm non-degraded routing (not W0). Unprotected runs never call the guardrail service.",
        )}
      </SettingsNote>
    </div>
  );
}
