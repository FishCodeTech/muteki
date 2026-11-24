"use client";

import { useCallback, useEffect, useState } from "react";
import { Button } from "@muteki/ui";
import { describeGrant, answerGrantRequest, fetchControlThread, type ControlGrantRequest } from "@/lib/controlGrants";

const POLL_MS = 3000;

export function ConversationGrantRequest({
  threadId,
  watermark,
}: {
  threadId?: string;
  watermark?: number;
}) {
  const [pending, setPending] = useState<ControlGrantRequest[]>([]);
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");

  const refresh = useCallback(async () => {
    if (!threadId) return;
    try {
      setPending((await fetchControlThread(threadId)).pending);
    } catch {
    }
  }, [threadId]);

  useEffect(() => {
    if (!threadId) return;
    void refresh();
  }, [threadId, watermark, refresh]);

  useEffect(() => {
    setPending([]);
    setError("");
    if (!threadId) return;
    void refresh();
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") void refresh();
    }, POLL_MS);
    return () => window.clearInterval(timer);
  }, [threadId, refresh]);

  if (!threadId || pending.length === 0) return null;

  const answer = async (request: ControlGrantRequest, approve: boolean) => {
    setBusy(request.request_id);
    setError("");
    try {
      await answerGrantRequest(threadId, request.request_id, approve);
      await refresh();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "操作失败，请重试。");
    } finally {
      setBusy("");
    }
  };

  return (
    <div role="group" aria-label="Agent 授权申请" className="flex flex-col gap-3">
      {pending.map((request) => (
        <div key={request.request_id} className="rounded-xl bg-cx-elevated p-4 shadow-cx-pop">
          <div className="text-cx-sm font-semibold text-cx-fg">Agent 申请更多控制权限</div>
          <div className="mt-1 text-cx-sm text-cx-fg-2">{describeGrant(request.grant)}</div>
          <div className="mt-2 whitespace-pre-wrap text-cx-sm leading-5 text-cx-fg-3">原因：{request.reason}</div>
          <div className="mt-3 flex items-center gap-2">
            <Button variant="primary" size="sm" loading={busy === request.request_id} disabled={busy !== ""} onClick={() => void answer(request, true)}>批准</Button>
            <Button variant="secondary" size="sm" disabled={busy !== ""} onClick={() => void answer(request, false)}>拒绝</Button>
          </div>
        </div>
      ))}
      {error ? <div role="alert" className="text-cx-sm text-cx-danger">{error}</div> : null}
    </div>
  );
}
