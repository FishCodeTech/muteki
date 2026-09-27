import {
  computeProviderSwitchDelta,
  summarizeProviderSwitchDelta,
  type InteractionCapabilityMatrix,
} from "./interactionCapabilityMatrix";

type RuntimeBinding = {
  adapter_id?: string;
  instance_id?: string;
  credential_id?: string;
  credential_ref?: string;
};

export function providerNoticeScope(
  draftKey: string,
  runtimeKey: string,
  credentialId: string,
  model: string,
  sessionId = "",
): string {
  return JSON.stringify([draftKey, runtimeKey, credentialId, model, sessionId]);
}

/** Recompute from the actual thread binding, including stash and default restores. */
export function providerSwitchPreview(input: {
  threadId: string;
  viewThreadId?: string;
  boundRuntime?: RuntimeBinding;
  credentialId: string;
  runtime?: { key: string; health?: { capabilities?: Record<string, unknown> } | null };
  matrix: InteractionCapabilityMatrix | null;
}): string {
  const { boundRuntime: bound, runtime } = input;
  if (!input.threadId || input.viewThreadId !== input.threadId || !bound?.adapter_id || !runtime) return "";
  const boundKey = `${bound.adapter_id}:${bound.instance_id || "default"}`;
  const boundCredential = bound.credential_id || bound.credential_ref || "";
  if (runtime.key === boundKey && (!boundCredential || boundCredential === input.credentialId)) return "";
  const caps = runtime.health?.capabilities || {};
  const after: InteractionCapabilityMatrix = {
    revision: 0,
    stale: false,
    rows: [
      { key: "native_history", level: caps.resume === true ? "supported" : "unsupported" },
      { key: "attachments", level: caps.image_input === true ? "supported" : "limited" },
      { key: "approval", level: caps.approval === true ? "supported" : "unknown" },
      { key: "plan", level: "unknown" },
      { key: "rewind", level: "unknown" },
    ],
  };
  const summary = summarizeProviderSwitchDelta(computeProviderSwitchDelta(input.matrix, after));
  return summary
    ? `切换 Provider 后能力变化：${summary}。发送时 Muteki 会注入整理后的当前分支历史。`
    : "切换 Provider 后将新建 Session；Muteki 会注入整理后的当前分支历史，发送前请确认附件、审批、计划与回退能力";
}

/** A late runtime response may describe another selection or another session. */
export function runtimeHandoffNotice(input: {
  threadId: string;
  viewThreadId?: string;
  runtimeKey: string;
  credentialId: string;
  model: string;
  sessionId?: string | null;
  payload: Record<string, unknown>;
}): string {
  const p = input.payload;
  if (!input.threadId || input.viewThreadId !== input.threadId) return "";
  if (!p.adapter_id || `${p.adapter_id}:${p.instance_id || "default"}` !== input.runtimeKey) return "";
  if (String(p.credential_id || "") !== input.credentialId || String(p.model || "") !== input.model) return "";
  if (p.agent_session_id && p.agent_session_id !== input.sessionId) return "";
  const label = String(p.handoff_boundary_label || "").trim();
  if (label) return label;
  const included = Number(p.handoff_included_count || 0);
  if (p.recovery_kind === "structured_handoff" && included > 0) {
    return `Runtime 会话已重建：Muteki 已向新 Session 注入当前分支历史（${included} 条）`;
  }
  return p.recovery_kind === "native_resume" ? "已尝试原生 resume 续接会话" : "";
}
