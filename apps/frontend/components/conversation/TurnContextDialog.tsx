"use client";

import { useEffect, useState } from "react";
import { Badge, Callout, CopyButton, Dialog, Spinner } from "@muteki/ui";
import { fetchTurnContext, type TurnContextLedger, type TurnContextPart } from "@/lib/useConversation";

const CHANNEL_LABEL: Record<string, string> = {
  system_prompt: "系统提示追加",
  mcp_server_instructions: "MCP 服务说明（独立于用户消息）",
  unsupported: "未发送：该通道不支持应用说明",
  opencode_instructions: "OpenCode instructions",
  codex_additional_context: "Codex additionalContext",
  codex_inject_items: "Codex inject_items",
  claude_content_blocks: "独立内容块",
  acp_blocks: "ACP 内容块",
  text_blocks: "文本内容块",
  image_url: "图片内容块",
  text_envelope: "文本外壳",
};

function PartSection({ part }: { part: TurnContextPart }) {
  return (
    <section className="rounded-xl border border-cx-border-subtle bg-cx-bg-subtle">
      <header className="flex flex-wrap items-center gap-1.5 border-b border-cx-border-subtle px-3 py-2">
        <code className="font-cx-mono text-cx-xs text-cx-fg">{part.key}</code>
        <Badge tone={part.trust === "application" ? "accent" : "warning"}>
          {part.trust === "application" ? "宿主规则" : "不可信数据"}
        </Badge>
        <Badge>{part.scope === "session" ? "会话级" : "本轮"}</Badge>
        <Badge>{CHANNEL_LABEL[part.channel] || part.channel}</Badge>
        <span className="ml-auto text-cx-xs text-cx-fg-4">{part.chars} 字符</span>
        {part.text ? <CopyButton text={part.text} label="复制此段" /> : null}
      </header>
      <pre className="max-h-[320px] overflow-auto whitespace-pre-wrap break-words px-3 py-2 font-cx-mono text-cx-xs text-cx-fg-2">
        {part.text ?? (part.kind === "image_attachment" ? `图片：${part.name || "未命名附件"}\n类型：${part.media_type || "image"}\n大小：${part.size ?? 0} 字节\n${part.artifact_id ? `工件 ID：${part.artifact_id}` : "原件未保存，需重选文件"}\n图片字节随请求发送，此处仅保留元数据。` : "此记录仅保留元数据，没有保存完整文本。")}
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
      description="查看用户原文、随附上下文和应用说明的配置快照。"
      icon="eye"
      size="xl"
      testId="turn-context-dialog"
    >
      <p className="mb-3 text-cx-sm text-cx-fg-3">会话级说明通过标注的原生通道提供，不追加到用户消息；配置记录不代表模型已读取。</p>
      {error ? (
        <Callout tone="danger" title="无法加载">{error}</Callout>
      ) : !ledger ? (
        <div className="flex items-center gap-2 py-6 text-cx-fg-3"><Spinner /> 正在加载…</div>
      ) : (
        <div className="flex flex-col gap-3">
          {ledger.delivery_status === "request_prepared" ? (
            <Callout tone="warning" title="请求已组装">下列内容已保存为本次请求快照，尚未确认服务端收到。连接失败或请求中断时也会保留此记录。</Callout>
          ) : null}
          {ledger.availability === "metadata_only" ? (
            <Callout tone="warning" title="仅有元数据">此历史回合没有完整上下文快照，以下信息来自回合元数据。</Callout>
          ) : null}
          <section className="rounded-xl border border-cx-border-subtle">
            <header className="flex items-center gap-1.5 border-b border-cx-border-subtle px-3 py-2">
              <span className="text-cx-sm font-medium text-cx-fg">用户原文</span>
              <span className="ml-auto text-cx-xs text-cx-fg-4">{ledger.user_text.length} 字符</span>
              <CopyButton text={ledger.user_text} label="复制原文" />
            </header>
            <pre className="max-h-[240px] overflow-auto whitespace-pre-wrap break-words px-3 py-2 font-cx-mono text-cx-xs text-cx-fg-2">
              {ledger.user_text}
            </pre>
          </section>
          {ledger.parts.length === 0 ? (
            <p className="text-cx-sm text-cx-fg-3">本轮没有随附上下文。</p>
          ) : (
            ledger.parts.map((part, index) => <PartSection key={`${part.key}-${index}`} part={part} />)
          )}
        </div>
      )}
    </Dialog>
  );
}
