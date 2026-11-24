"use client";

import { useEffect, useRef, useState } from "react";
import { Icon } from "@muteki/ui";
import { Button } from "@muteki/ui";
import { Dialog } from "@muteki/ui";
import { desktopChatBridge, type DesktopQuitRequest } from "@/lib/desktopChatBridge";

export function DesktopQuitDialog() {
  const [request, setRequest] = useState<DesktopQuitRequest | null>(null);
  const [busy, setBusy] = useState<"quit" | "cancel" | null>(null);
  const [error, setError] = useState("");
  const cancelRef = useRef<HTMLButtonElement>(null);
  const submitting = useRef(false);

  useEffect(() => {
    const bridge = desktopChatBridge();
    if (!bridge?.onQuitRequested || !bridge.getQuitRequest) return;
    let mounted = true;
    const accept = (next: DesktopQuitRequest | null) => {
      if (!mounted) return;
      setRequest(previous => previous?.token === next?.token ? previous : next);
    };
    const unsubscribe = bridge.onQuitRequested(accept);
    void bridge.getQuitRequest().then(next => { if (next) accept(next); }).catch(cause => {
      console.error("desktop.quit_pending_failed", cause);
    });
    return () => { mounted = false; unsubscribe(); };
  }, []);

  const answer = async (confirmed: boolean) => {
    const bridge = desktopChatBridge();
    if (!request || !bridge?.replyQuit || submitting.current) return;
    submitting.current = true;
    setBusy(confirmed ? "quit" : "cancel");
    setError("");
    try {
      await bridge.replyQuit({ token: request.token, confirmed });
      if (!confirmed) setRequest(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "无法完成退出操作，请重试");
    } finally {
      submitting.current = false;
      setBusy(null);
    }
  };
  const tasks = request?.tasks ?? [];
  const count = request?.active ?? tasks.length;

  return (
    <Dialog
      open={!!request}
      onOpenChange={open => { if (!open) void answer(false); }}
      title={request?.error ? "无法确认后台任务状态" : `仍有 ${count} 个任务正在运行`}
      description="退出 Muteki 会停止本机 Runner，并中断正在运行的任务。"
      size="md"
      className="cx-root"
      hideClose
      dismissable={false}
      initialFocusRef={cancelRef}
      testId="desktop-quit-dialog"
      footer={<>
        <span className="mr-auto flex items-center gap-1.5 text-cx-xs text-cx-fg-3 max-sm:w-full">
          <Icon name="power" size={13} />
          本机 Runner 将停止
        </span>
        <Button ref={cancelRef} variant="ghost" disabled={!!busy} loading={busy === "cancel"} onClick={() => void answer(false)}>
          继续运行 <kbd className="ml-1 font-sans text-cx-2xs text-cx-fg-3">Esc</kbd>
        </Button>
        <Button variant="danger" agentConfirm="destructive" disabled={!!busy} loading={busy === "quit"} onClick={() => void answer(true)}>
          {busy === "quit" ? "正在退出…" : "退出并中断"}
        </Button>
      </>}
    >
      {tasks.length > 0 ? (
        <ul aria-label="正在运行的任务" className="m-0 space-y-1 p-0">
          {tasks.map(task => (
            <li key={`${task.kind}:${task.id}`} className="flex items-start gap-3 rounded-lg px-2 py-2.5">
              <Icon name="loader" size={16} className="mt-0.5 shrink-0 animate-spin text-cx-fg-3 motion-reduce:animate-none" />
              <span className="min-w-0 flex-1 break-words text-cx-sm leading-5 text-cx-fg">
                {task.title || (task.kind === "turn" ? "未命名对话" : task.id)}
              </span>
              <span className="shrink-0 rounded bg-cx-hover px-1.5 text-cx-2xs leading-5 text-cx-fg-3">
                {task.kind === "turn" ? "对话" : "任务"}
              </span>
            </li>
          ))}
        </ul>
      ) : null}
      {request?.error ? <p role="alert" className="m-0 break-words text-cx-warning">{request.error}</p> : null}
      {error ? <p role="alert" className="mb-0 mt-3 break-words text-cx-danger">{error}</p> : null}
    </Dialog>
  );
}
