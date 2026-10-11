"use client";

import { useEffect, useState } from "react";
import { Badge, Callout, CopyButton, Dialog, Spinner } from "@/components/chat/ui";
import { fetchTurnContext, type TurnContextLedger, type TurnContextPart } from "@/lib/useConversation";

const CHANNEL_LABEL: Record<string, string> = {
  system_prompt: "系统提示追加",
  opencode_instructions: "OpenCode instructions",
  codex_additional_context: "Codex additionalContext",
  codex_inject_items: "Codex inject_items",
  claude_content_blocks: "独立内容块",
  acp_blocks: "ACP 内容块",
  text_blocks: "独立文本块",
  text_envelope: "文本外壳",
};

function PartSection({ part }: { part: TurnContextPart }) {
  return (
    <section className="rounded-xl border border-cx-border-subtle bg-cx-bg-subtle">
      <header className="flex flex-wrap items-center gap-1.5 border-b border-cx-border-subtle px-3 py-2">
        <code className="font-cx-mono text-[12px] text-cx-fg">{part.key}</code>
        <Badge tone={part.trust === "application" ? "accent" : "warning"}>
          {part.trust === "application" ? "宿主规则" : "不可信数据"}
        </Badge>
        <Badge>{part.scope === "session" ? "会话级" : "本轮"}</Badge>
        <Badge>{CHANNEL_LABEL[part.channel] || part.channel}</Badge>
        <span className="ml-auto text-[12px] text-cx-fg-4">{part.chars} 字符</span>
        {part.text ? <CopyButton text={part.text} label="复制此段" /> : null}
      </header>
      <pre className="max-h-[320px] overflow-auto whitespace-pre-wrap break-words px-3 py-2 font-cx-mono text-[12px] text-cx-fg-2">
        {part.text ?? ""}
      </pre>
    </section>
  );
}

export function TurnContextDialog({
  threadId,
  turnId,
  onClose,
}: {
  threadId: string;
  turnId: string | null;
  onClose: () => void;
}) {
  const [ledger, setLedger] = useState<TurnContextLedger | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    if (!turnId) return;
    const controller = new AbortController();
    setLedger(null);
    setError("");
    fetchTurnContext(threadId, turnId, controller.signal)
      .then(setLedger)
      .catch((reason: unknown) => {
        if (controller.signal.aborted) return;
        setError(reason instanceof Error ? reason.message : String(reason));
      });
    return () => controller.abort();
  }, [threadId, turnId]);

  return (
    <Dialog
      open={Boolean(turnId)}
      onOpenChange={(open) => { if (!open) onClose(); }}
      title="实际发送内容"
      description="用户原文原样发送；下列上下文经引擎原生通道另行投递，不进入用户消息。"
      icon="eye"
      size="xl"
      testId="turn-context-dialog"
    >
      {error ? (
        <Callout tone="danger" title="无法加载">{error}</Callout>
      ) : !ledger ? (
        <div className="flex items-center gap-2 py-6 text-cx-fg-3"><Spinner /> 正在加载…</div>
      ) : (
        <div className="flex flex-col gap-3">
          <section className="rounded-xl border border-cx-border-subtle">
            <header className="flex items-center gap-1.5 border-b border-cx-border-subtle px-3 py-2">
              <span className="text-[13px] font-medium text-cx-fg">用户原文</span>
              <span className="ml-auto text-[12px] text-cx-fg-4">{ledger.user_text.length} 字符</span>
              <CopyButton text={ledger.user_text} label="复制原文" />
            </header>
            <pre className="max-h-[240px] overflow-auto whitespace-pre-wrap break-words px-3 py-2 font-cx-mono text-[12px] text-cx-fg-2">
              {ledger.user_text}
            </pre>
          </section>
          {ledger.parts.length === 0 ? (
            <p className="text-[13px] text-cx-fg-3">本轮没有随附上下文。</p>
          ) : (
            ledger.parts.map((part, index) => <PartSection key={`${part.key}-${part.sha256}-${index}`} part={part} />)
          )}
        </div>
      )}
    </Dialog>
  );
}
