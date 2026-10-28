"use client";

import { useEffect, useRef, useState } from "react";
import { Button } from "@heroui/react";
import { Icon } from "@/components/Icon";
import { decideSrcApproval, fetchExecutionReceipt, getSrcRecord } from "@/lib/srcApi";
import type { ExecutionApproval, ExecutionReceipt } from "@/lib/srcTypes";
import { SrcDefinitions, SrcEmpty, SrcError, SrcJson, SrcLoading, SrcSection, SrcStatus, stateTone, useSrcResource } from "./ui";

export function ApprovalInspector({ taskId, approvalId }: { taskId: string; approvalId: string }) {
  const resource = useSrcResource(() => getSrcRecord<ExecutionApproval>(taskId, "execution_approval", approvalId), [taskId, approvalId], { pollMs: 3000 });
  const [scopeMode, setScopeMode] = useState(false);
  const [resourceSelector, setResourceSelector] = useState("{}");
  const [actions, setActions] = useState("");
  const [identity, setIdentity] = useState("{}");
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const [receipt, setReceipt] = useState<ExecutionReceipt | null>(null);
  const [receiptError, setReceiptError] = useState("");
  const approvalRef = useRef(resource.data?.payload);
  approvalRef.current = resource.data?.payload;
  useEffect(() => {
    const approval = approvalRef.current;
    if (!approval) return;
    const request = approval.request_snapshot;
    setResourceSelector(JSON.stringify({ url: { target: request.arguments.url || request.arguments.target_url || "" } }, null, 2));
    setActions(request.action);
    setIdentity(JSON.stringify(request.identity_ref ? { identity_ref: request.identity_ref } : {}, null, 2));
  }, [resource.data?.record_id]);
  useEffect(() => {
    if (!resource.data) return;
    setReceipt(null); setReceiptError("");
    void fetchExecutionReceipt(taskId, resource.data.payload.invocation_id).then(setReceipt).catch(cause => setReceiptError(cause instanceof Error ? cause.message : String(cause)));
  }, [taskId, resource.data]);
  const decide = async (decision: "approve_once" | "deny" | "approve_task_scope") => {
    if (!resource.data) return;
    setBusy(decision); setError("");
    try {
      let scope;
      if (decision === "approve_task_scope") {
        const parsedSelector = JSON.parse(resourceSelector) as unknown;
        const parsedIdentity = JSON.parse(identity) as unknown;
        if (!parsedSelector || typeof parsedSelector !== "object" || Array.isArray(parsedSelector) || !parsedIdentity || typeof parsedIdentity !== "object" || Array.isArray(parsedIdentity)) throw new Error("授权范围必须是 JSON 对象");
        scope = {
          resource_selector: parsedSelector as Record<string, unknown>,
          identity_constraints: parsedIdentity as Record<string, unknown>,
          actions: actions.split(",").map(value => value.trim()).filter(Boolean),
        };
        if (!scope.actions.length) throw new Error("Task 范围至少保留一个明确动作");
      }
      await decideSrcApproval(taskId, resource.data, decision, scope);
      await resource.reload();
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setBusy(""); }
  };
  if (resource.loading && !resource.data) return <SrcLoading label="正在恢复审批请求与固定输入…" />;
  if (!resource.data) return <SrcError error={resource.error} onRetry={() => void resource.reload()} />;
  const approval = resource.data.payload;
  const request = approval.request_snapshot;
  return <div>
    <SrcError error={error || resource.error} />
    <SrcSection title="审批状态" actions={<SrcStatus tone={stateTone(approval.status)}>{approval.status}</SrcStatus>}>
      <SrcDefinitions rows={[["审批 / 调用", <code key="ids">{approval.approval_id}<br />{approval.invocation_id}</code>], ["工具 / 动作", `${request.tool_name} · ${request.action}`], ["Run / Worker", `${request.run_id || "主聊天"} / ${request.worker_id || "—"}`], ["身份", `${request.identity_ref || "未指定"} · revision ${request.identity_revision}`], ["策略", `v${approval.policy_version}`], ["环境", `${request.environment_id || "未命名"} · generation ${request.environment_generation}`], ["cwd", request.cwd || "—"], ["请求 / 到期", `${approval.requested_at} / ${approval.expires_at}`], ["决定", approval.decision ? `${approval.decision} · ${approval.decided_by} · ${approval.decided_at}` : "尚未决定"], ["消费时间", approval.consumed_at || "尚未下发"]]} />
    </SrcSection>
    <SrcSection title="固定输入与目标">
      <p className="src-hint">批准绑定这份参数、目标、身份、cwd、策略和环境。任何关键字段变化都必须重新判定。</p>
      <SrcJson value={request.arguments} />
      <SrcDefinitions rows={[["参数 hash", <code key="hash">{request.arguments_hash}</code>], ["审批输入 digest", <code key="digest">{approval.input_digest}</code>], ["命中规则", approval.matched_rule_refs.join("、") || "新主动测试目标"]]} />
    </SrcSection>
    <SrcSection title="资源事实与来源">
      <SrcJson value={request.resource_facts} />
    </SrcSection>
    {approval.status === "pending" ? <SrcSection title="决定">
      <label className="src-inline-checks"><input type="checkbox" checked={scopeMode} onChange={event => setScopeMode(event.target.checked)} />同时保存本 Task 的明确范围（不是仅批准此次）</label>
      {scopeMode ? <div className="src-scope-editor">
        <label className="src-field">资源选择器（JSON）<textarea rows={4} value={resourceSelector} onChange={event => setResourceSelector(event.target.value)} /></label>
        <label className="src-field">允许动作（逗号分隔）<input value={actions} onChange={event => setActions(event.target.value)} /></label>
        <label className="src-field">身份约束（JSON）<textarea rows={3} value={identity} onChange={event => setIdentity(event.target.value)} /></label>
        <p className="src-hint">不会默认扩展到整域、其他身份、任意脚本或 full-access。原生执行器仍只收到当前请求的 once。</p>
      </div> : null}
      <div className="src-button-row"><Button variant="primary" isPending={busy === (scopeMode ? "approve_task_scope" : "approve_once")} isDisabled={Boolean(busy)} onPress={() => void decide(scopeMode ? "approve_task_scope" : "approve_once")}><Icon name="check" size={14} />{scopeMode ? "批准并保存范围" : "仅批准此次"}</Button><Button variant="secondary" isPending={busy === "deny"} isDisabled={Boolean(busy)} onPress={() => void decide("deny")}>拒绝，不下发</Button></div>
    </SrcSection> : null}
    <SrcSection title="执行回执">
      {receipt ? <><SrcStatus tone={stateTone(receipt.dispatch_state)}>{receipt.dispatch_state}</SrcStatus><SrcDefinitions rows={[["错误", receipt.error_code || "无"], ["结果已知", receipt.outcome_known ? "是" : "否"], ["开始 / 结束", `${receipt.started_at || "未下发"} / ${receipt.ended_at || "未结束"}`]]} /><SrcJson value={receipt.history} /></> : receiptError ? <SrcError error={receiptError} /> : <SrcEmpty>正在读取独立执行回执…</SrcEmpty>}
    </SrcSection>
  </div>;
}
