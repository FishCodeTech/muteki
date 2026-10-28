"use client";

import { useEffect, useMemo, useState } from "react";
import { Button } from "@heroui/react";
import { Icon } from "@/components/Icon";
import {
  fetchSrcCapabilities, fetchSrcPolicy, listSrcRecords,
  previewSrcPolicy, revokeSrcGrant, updateSrcPolicy,
} from "@/lib/srcApi";
import type {
  ExecutionApproval, ExecutionCapabilities, ExecutionPolicy, ExecutionReceipt,
  ExecutionRule, PolicyDecision, PolicyPreview, TaskExecutionGrant,
} from "@/lib/srcTypes";
import { SrcDefinitions, SrcEmpty, SrcError, SrcJson, SrcLoading, SrcSection, SrcStatus, stateTone, useSrcResource } from "./ui";

type PolicyData = {
  policy: ExecutionPolicy;
  capabilities: ExecutionCapabilities;
  approvals: Awaited<ReturnType<typeof listSrcRecords<ExecutionApproval>>>;
  grants: Awaited<ReturnType<typeof listSrcRecords<TaskExecutionGrant>>>;
  receipts: Awaited<ReturnType<typeof listSrcRecords<ExecutionReceipt>>>;
  decisions: Awaited<ReturnType<typeof listSrcRecords<PolicyDecision>>>;
};

type Tab = "policy" | "approvals" | "grants" | "history";

function lines(value: string) { return value.split("\n").map(row => row.trim()).filter(Boolean); }
function jsonObject(value: string, label: string) {
  const parsed = JSON.parse(value) as unknown;
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error(`${label}必须是 JSON 对象`);
  return parsed as Record<string, unknown>;
}
function newRule(): ExecutionRule {
  return {
    rule_id: `rule-${Date.now()}`, version: 1, enabled: true, effect: "ask",
    evaluation_point: "request", mode: "enforce", resource_kind: "http",
    selector: { path: { prefix: "/api/" } }, actions: ["request"], conditions: {},
    reason: "", source_refs: [], reusable_approval: true,
  };
}

export function ExecutionPolicyPanel({ taskId, initialTab = "policy", onOpenApproval }: {
  taskId: string; initialTab?: Tab; onOpenApproval: (approvalId: string) => void;
}) {
  const resource = useSrcResource<PolicyData>(async () => {
    const [policy, capabilities, approvals, grants, receipts, decisions] = await Promise.all([
      fetchSrcPolicy(taskId), fetchSrcCapabilities(taskId),
      listSrcRecords<ExecutionApproval>(taskId, "execution_approval"),
      listSrcRecords<TaskExecutionGrant>(taskId, "task_grant"),
      listSrcRecords<ExecutionReceipt>(taskId, "execution_receipt"),
      listSrcRecords<PolicyDecision>(taskId, "policy_decision"),
    ]);
    return { policy, capabilities, approvals, grants, receipts, decisions };
  }, [taskId], { pollMs: 5000 });
  const [tab, setTab] = useState<Tab>(initialTab);
  useEffect(() => setTab(initialTab), [initialTab]);
  const [draft, setDraft] = useState<ExecutionPolicy | null>(null);
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [actionError, setActionError] = useState("");
  const [notice, setNotice] = useState("");
  const [previewInput, setPreviewInput] = useState({ tool_name: "http", action: "request", resource_kind: "http", url: "https://example.com/api/items", method: "POST", identity_ref: "default", tenant: "" });
  const [preview, setPreview] = useState<PolicyPreview | null>(null);
  const [previewing, setPreviewing] = useState(false);
  useEffect(() => {
    if (resource.data?.policy && !dirty) setDraft(resource.data.policy);
  }, [resource.data?.policy, dirty]);
  const edit = (changes: Partial<ExecutionPolicy>) => { setDirty(true); setDraft(current => current ? { ...current, ...changes } : current); };
  const editRule = (index: number, changes: Partial<ExecutionRule>) => {
    if (!draft) return;
    edit({ rules: draft.rules.map((rule, i) => i === index ? { ...rule, ...changes } : rule) });
  };
  const save = async () => {
    if (!draft) return;
    setSaving(true); setActionError(""); setNotice("");
    try {
      for (const rule of draft.rules) {
        if (!rule.rule_id.trim()) throw new Error("规则 ID 不能为空");
        jsonObject(JSON.stringify(rule.selector), "selector");
        jsonObject(JSON.stringify(rule.conditions), "conditions");
      }
      const previousRules = new Map(resource.data?.policy.rules.map(rule => [rule.rule_id, rule]));
      const rules = draft.rules.map(rule => {
        const previous = previousRules.get(rule.rule_id);
        return previous && JSON.stringify(previous) !== JSON.stringify(rule)
          ? { ...rule, version: previous.version + 1 } : rule;
      });
      const receipt = await updateSrcPolicy(taskId, {
        base_version: draft.version,
        rules, authorized_targets: draft.authorized_targets,
        excluded_targets: draft.excluded_targets, network_mode: draft.network_mode,
        inspection_requirements: draft.inspection_requirements,
        environment_requirements: draft.environment_requirements,
        source_refs: draft.source_refs,
      });
      setDirty(false); setDraft(receipt.output); setNotice(`策略 v${receipt.output.version} 已保存；请核对实际环境生效状态。`);
      await resource.reload();
    } catch (cause) { setActionError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setSaving(false); }
  };
  const previewRequest = async () => {
    setPreviewing(true); setActionError("");
    try {
      const resourceFacts = previewInput.tenant ? { tenant: { value: previewInput.tenant, origin: "user", state: "known" } } : {};
      setPreview(await previewSrcPolicy(taskId, {
        invocation_id: `preview-${Date.now()}`, tool_name: previewInput.tool_name,
        action: previewInput.action, arguments: {
          url: previewInput.url, method: previewInput.method,
        }, arguments_hash: "preview", resource_kind: previewInput.resource_kind,
        resource_facts: resourceFacts, identity_ref: previewInput.identity_ref,
      }));
    } catch (cause) { setActionError(cause instanceof Error ? cause.message : String(cause)); setPreview(null); }
    finally { setPreviewing(false); }
  };
  const diff = useMemo(() => {
    if (!draft || !resource.data) return [];
    const current = resource.data.policy;
    const rows: string[] = [];
    if (JSON.stringify(current.authorized_targets) !== JSON.stringify(draft.authorized_targets)) rows.push("授权目标");
    if (JSON.stringify(current.excluded_targets) !== JSON.stringify(draft.excluded_targets)) rows.push("排除目标");
    if (current.network_mode !== draft.network_mode) rows.push(`网络 Scope：${current.network_mode} → ${draft.network_mode}`);
    if (JSON.stringify(current.inspection_requirements) !== JSON.stringify(draft.inspection_requirements)) rows.push("HTTP 取证与内容规则");
    if (JSON.stringify(current.environment_requirements) !== JSON.stringify(draft.environment_requirements)) rows.push("执行环境与服务例外");
    if (JSON.stringify(current.rules) !== JSON.stringify(draft.rules)) rows.push(`规则：${current.rules.length} → ${draft.rules.length}`);
    return rows;
  }, [draft, resource.data]);
  if (resource.loading && !resource.data) return <SrcLoading label="正在读取执行权限与实际环境…" />;
  if (!resource.data || !draft) return <SrcError error={resource.error || "执行策略不可读"} onRetry={() => void resource.reload()} />;
  const { policy, capabilities, approvals, grants, receipts, decisions } = resource.data;
  const pending = approvals.filter(row => row.payload.status === "pending");
  return <div className="src-policy">
    <div className="src-tabs" role="tablist" aria-label="执行权限视图">
      {([["policy", "策略与环境"], ["approvals", `审批 ${pending.length}`], ["grants", "Task 授权"], ["history", "决定与回执"]] as Array<[Tab, string]>).map(([value, label]) => <button key={value} role="tab" aria-selected={tab === value} onClick={() => setTab(value)}>{label}</button>)}
    </div>
    <SrcError error={resource.error || actionError} onRetry={() => void resource.reload()} />
    {notice ? <p className="src-notice src-notice--success" role="status"><Icon name="check" size={14} />{notice}</p> : null}
    {tab === "policy" ? <>
      <SrcSection title="有效策略" actions={<SrcStatus tone={stateTone(String(policy.activation.state || "neutral"))}>v{policy.version} · {policy.network_mode}</SrcStatus>}>
        <SrcDefinitions rows={[["策略 digest", <code key="digest">{policy.digest}</code>], ["授权依据", policy.source_refs.length ? policy.source_refs.join("、") : "用户明确输入"], ["环境生效", <span key="activation">{policy.activation.state ? JSON.stringify(policy.activation) : "按当前通道实际能力执行"}</span>]]} />
      </SrcSection>
      <SrcSection title="目标与排除">
        <label className="src-field">允许主动测试目标（每行一个完整 URL）<textarea rows={3} value={draft.authorized_targets.join("\n")} onChange={event => edit({ authorized_targets: lines(event.target.value) })} /></label>
        <label className="src-field">明确排除目标<textarea rows={2} value={draft.excluded_targets.join("\n")} onChange={event => edit({ excluded_targets: lines(event.target.value) })} /></label>
      </SrcSection>
      <SrcSection title="执行环境与流量">
        <div className="src-form-grid">
          <label className="src-field">网络 Scope<select value={draft.network_mode} onChange={event => edit({ network_mode: event.target.value as ExecutionPolicy["network_mode"] })}><option value="declared">declared · 声明边界</option><option value="enforced">enforced · 实际出口</option></select></label>
          <label className="src-field">HTTP 取证与内容规则<select value={draft.inspection_requirements.mode} onChange={event => edit({ inspection_requirements: { ...draft.inspection_requirements, mode: event.target.value as ExecutionPolicy["inspection_requirements"]["mode"] } })}><option value="off">off · 不记录</option><option value="record">record · 尽力记录</option><option value="enforce">enforce · 发送前强制</option></select></label>
        </div>
        <label className="src-field">基础服务例外（JSON 数组；强制模式必须明确控制面/Provider）<textarea rows={4} value={JSON.stringify(draft.environment_requirements.service_exceptions || [], null, 2)} onChange={event => {
          try { edit({ environment_requirements: { ...draft.environment_requirements, service_exceptions: JSON.parse(event.target.value) } }); } catch { setDraft(current => current ? { ...current } : current); setDirty(true); }
        }} /></label>
        <div className="src-capability-grid">{Object.entries(capabilities.tools).map(([name, capability]) => <article key={name}><strong>{name}</strong><small>{capability.environment_boundary || "按适配器边界"}</small><p>下发前 gate：{capability.pre_dispatch_gate ? "已接入" : "未覆盖"}</p><p>网络：{capability.network_scope?.join(" / ") || "未声明"} · 内容：{capability.content_inspection?.join(" / ") || "未声明"}</p><p>取消：{capability.cancellation || "未声明"} · 回执：{capability.receipt_support || "未声明"}</p></article>)}</div>
        <p className="src-notice"><Icon name="info" size={14} />实际强制出口：{capabilities.network_enforced ? "已核验" : "未启用或未核验"}；内容强制：{capabilities.content_enforced ? "已核验" : "未启用或未核验"}。保存策略不等于环境已生效。</p>
      </SrcSection>
      <SrcSection title="执行规则" count={draft.rules.length} actions={<Button size="sm" variant="secondary" onPress={() => edit({ rules: [...draft.rules, newRule()] })}><Icon name="plus" size={13} />新增规则</Button>}>
        {!draft.rules.length ? <SrcEmpty>未配置自定义限制；在明确授权目标内按默认策略自主执行。</SrcEmpty> : <div className="src-rule-list">{draft.rules.map((rule, index) => <article className="src-rule" key={`${rule.rule_id}-${index}`}>
          <div className="src-rule-head"><input aria-label="规则 ID" value={rule.rule_id} onChange={event => editRule(index, { rule_id: event.target.value })} /><label>效果<select value={rule.effect} onChange={event => editRule(index, { effect: event.target.value as ExecutionRule["effect"] })}><option value="allow">allow</option><option value="ask">ask</option><option value="deny">deny</option></select></label><label>模式<select value={rule.mode} onChange={event => editRule(index, { mode: event.target.value as ExecutionRule["mode"] })}><option value="enforce">enforce</option><option value="observe">observe</option></select></label><Button size="sm" variant="ghost" aria-label={`删除规则 ${rule.rule_id}`} onPress={() => edit({ rules: draft.rules.filter((_row, i) => i !== index) })}><Icon name="trash" size={13} /></Button></div>
          <div className="src-form-grid"><label className="src-field">资源<select value={rule.resource_kind} onChange={event => editRule(index, { resource_kind: event.target.value as ExecutionRule["resource_kind"] })}>{["http", "browser", "file", "process", "network", "mcp", "control"].map(value => <option key={value} value={value}>{value}</option>)}</select></label><label className="src-field">求值边界<select value={rule.evaluation_point} onChange={event => editRule(index, { evaluation_point: event.target.value as ExecutionRule["evaluation_point"] })}>{["tool", "filesystem", "connection", "request"].map(value => <option key={value} value={value}>{value}</option>)}</select></label></div>
          <label className="src-field">动作（逗号分隔）<input value={rule.actions.join(",")} onChange={event => editRule(index, { actions: event.target.value.split(",").map(value => value.trim()).filter(Boolean) })} /></label>
          <div className="src-form-grid"><label className="src-field">selector（JSON）<textarea rows={3} value={JSON.stringify(rule.selector, null, 2)} onChange={event => { try { editRule(index, { selector: JSON.parse(event.target.value) }); } catch { setDirty(true); } }} /></label><label className="src-field">conditions（JSON）<textarea rows={3} value={JSON.stringify(rule.conditions, null, 2)} onChange={event => { try { editRule(index, { conditions: JSON.parse(event.target.value) }); } catch { setDirty(true); } }} /></label></div>
          <label className="src-field">原因<input value={rule.reason} onChange={event => editRule(index, { reason: event.target.value })} /></label>
          <div className="src-inline-checks"><label><input type="checkbox" checked={rule.enabled} onChange={event => editRule(index, { enabled: event.target.checked })} />启用</label><label><input type="checkbox" checked={rule.reusable_approval} onChange={event => editRule(index, { reusable_approval: event.target.checked })} />允许保存明确 Task 范围</label></div>
        </article>)}</div>}
      </SrcSection>
      <SrcSection title="保存前版本差异">
        {!diff.length ? <SrcEmpty>当前没有未保存修改。</SrcEmpty> : <ul className="src-list">{diff.map(row => <li key={row}>{row}</li>)}</ul>}
        <div className="src-button-row"><Button variant="primary" isPending={saving} isDisabled={!dirty || saving} onPress={() => void save()}><Icon name="check" size={14} />保存为新版本</Button>{dirty ? <Button variant="ghost" onPress={() => { setDraft(policy); setDirty(false); }}>放弃修改</Button> : null}</div>
      </SrcSection>
      <SrcSection title="规则命中预览">
        <div className="src-form-grid"><label className="src-field">资源<input value={previewInput.resource_kind} onChange={event => setPreviewInput({ ...previewInput, resource_kind: event.target.value })} /></label><label className="src-field">动作<input value={previewInput.action} onChange={event => setPreviewInput({ ...previewInput, action: event.target.value })} /></label><label className="src-field">完整 URL<input value={previewInput.url} onChange={event => setPreviewInput({ ...previewInput, url: event.target.value })} /></label><label className="src-field">方法<input value={previewInput.method} onChange={event => setPreviewInput({ ...previewInput, method: event.target.value.toUpperCase() })} /></label><label className="src-field">身份<input value={previewInput.identity_ref} onChange={event => setPreviewInput({ ...previewInput, identity_ref: event.target.value })} /></label><label className="src-field">租户（可选）<input value={previewInput.tenant} onChange={event => setPreviewInput({ ...previewInput, tenant: event.target.value })} /></label></div>
        <Button size="sm" variant="secondary" isPending={previewing} onPress={() => void previewRequest()}><Icon name="eye" size={14} />只预览，不执行</Button>
        {preview ? <div className="src-preview-result"><SrcStatus tone={stateTone(preview.status)}>{preview.effect} · {preview.status}</SrcStatus><SrcDefinitions rows={[["原因", preview.reason_code], ["命中规则", preview.matched_rule_refs.join("、") || "无"], ["观察规则", preview.observed_rule_refs.join("、") || "无"], ["未知字段", preview.unknown_fields.join("、") || "无"]]} /></div> : null}
      </SrcSection>
    </> : null}
    {tab === "approvals" ? <SrcSection title="审批请求" count={approvals.length}>
      {!approvals.length ? <SrcEmpty>当前 Task 没有审批记录。</SrcEmpty> : <div className="src-record-list">{[...approvals].reverse().map(row => <button key={row.record_id} onClick={() => onOpenApproval(row.record_id)}><span><strong>{row.payload.request_snapshot.tool_name} · {row.payload.request_snapshot.action}</strong><small>{row.payload.request_snapshot.arguments.url as string || row.payload.invocation_id}</small></span><SrcStatus tone={stateTone(row.payload.status)}>{row.payload.status}</SrcStatus></button>)}</div>}
    </SrcSection> : null}
    {tab === "grants" ? <SrcSection title="Task 范围授权" count={grants.length}>
      {!grants.length ? <SrcEmpty>没有保存的 Task 范围授权。一次批准不会自动创建 Grant。</SrcEmpty> : <div className="src-grant-list">{grants.map(row => <article key={row.record_id}><header><code>{row.record_id}</code><SrcStatus tone={row.payload.revoked_at ? "danger" : "positive"}>{row.payload.revoked_at ? "已撤销" : "有效"}</SrcStatus></header><SrcDefinitions rows={[["规则", row.payload.rule_refs.join("、") || "新目标授权"], ["动作", row.payload.actions.join("、")], ["来源审批", row.payload.source_approval_id], ["过期时间", row.payload.expires_at || "跟随来源审批"]]} /><SrcJson value={{ resource: row.payload.resource_selector, identity: row.payload.identity_constraints, environment: row.payload.environment_constraints, input: row.payload.input_constraints }} />{!row.payload.revoked_at ? <Button size="sm" variant="secondary" onPress={async () => { setActionError(""); try { await revokeSrcGrant(taskId, row); await resource.reload(); } catch (cause) { setActionError(cause instanceof Error ? cause.message : String(cause)); } }}>撤销后续授权</Button> : null}</article>)}</div>}
    </SrcSection> : null}
    {tab === "history" ? <>
      <SrcSection title="策略决定" count={decisions.length}><div className="src-record-list">{[...decisions].reverse().map(row => <article key={row.record_id}><code>{row.payload.invocation_id}</code><p>{row.payload.reason_code} · policy v{row.payload.policy_version}</p><small>{row.payload.matched_rule_refs.join("、") || "默认策略"}{row.payload.unknown_fields.length ? ` · 未知：${row.payload.unknown_fields.join("、")}` : ""}</small></article>)}</div></SrcSection>
      <SrcSection title="执行回执" count={receipts.length}><div className="src-record-list">{[...receipts].reverse().map(row => <article key={row.record_id}><header><code>{row.record_id}</code><SrcStatus tone={stateTone(row.payload.dispatch_state)}>{row.payload.dispatch_state}</SrcStatus></header><p>{row.payload.error_code || "执行完成"} · outcome known：{row.payload.outcome_known ? "是" : "否"}</p><small>{row.payload.started_at || "尚未下发"}{row.payload.ended_at ? ` → ${row.payload.ended_at}` : ""}</small></article>)}</div></SrcSection>
    </> : null}
  </div>;
}
