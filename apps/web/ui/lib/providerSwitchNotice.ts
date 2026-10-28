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

/** null ignores another scope; an empty string clears this scope's old notice. */
export function runtimeHandoffNoticeUpdate(input: {
  threadId: string;
  viewThreadId?: string;
  runtimeKey: string;
  credentialId: string;
  model: string;
  sessionId?: string | null;
  payload: Record<string, unknown>;
}): string | null {
  const p = input.payload;
  if (!input.threadId || input.viewThreadId !== input.threadId) return null;
  if (!p.adapter_id || `${p.adapter_id}:${p.instance_id || "default"}` !== input.runtimeKey) return null;
  if (String(p.credential_id || "") !== input.credentialId || String(p.model || "") !== input.model) return null;
  // Missing session identity cannot safely set or clear a current-session notice.
  if (!input.sessionId || p.agent_session_id !== input.sessionId) return null;
  if (p.recovery_kind !== "structured_handoff" || p.handoff_delivery_state !== "delivered") return "";
  if (!String(p.handoff_id || "").trim() || p.handoff_target_agent_session_id !== input.sessionId) return "";
  if (!String(p.handoff_delivery_turn_id || "").trim()) return "";
  const included = Number(p.handoff_included_count || 0);
  if (!Number.isInteger(included) || included <= 0) return "";
  const label = String(p.handoff_boundary_label || "").trim();
  if (label) return label;
  return `Runtime 会话已重建：Muteki 已向新 Session 注入当前分支历史（${included} 条）`;
}

/** Text-only callers do not need to distinguish ignored events from clearing. */
export function runtimeHandoffNotice(input: Parameters<typeof runtimeHandoffNoticeUpdate>[0]): string {
  return runtimeHandoffNoticeUpdate(input) ?? "";
}
