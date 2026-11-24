"use client";

import { MotionIcon } from "@/components/MotionIcon";

import { useState } from "react";
import { Icon } from "@muteki/ui";
import { Badge, Button, Dialog } from "@muteki/ui";
import type { DeckState, WorkerPromptRecord } from "@/lib/events";
import { formatClock } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { useCopied } from "@/lib/useCopied";
import { workerPrompts } from "@/lib/workerPrompts";

function PromptBody({ record }: { record: WorkerPromptRecord }) {
  const t = useT();
  const [copied, copy] = useCopied();
  return <div className="flex min-h-0 flex-col gap-3">
    <div className="flex flex-wrap items-center gap-2 text-cx-xs text-cx-fg-3">
      <Badge tone={record.status === "sent" ? "success" : record.status === "prepared" ? "neutral" : "warning"}>
        {t(`worker.prompt.status.${record.status}`)}
      </Badge>
      <time>{formatClock(record.ts, "—")}</time>
      {record.model && <span className="[overflow-wrap:anywhere]">{record.model}</span>}
      {record.intentId && <span className="[overflow-wrap:anywhere]" title={record.intentId}>{record.intentId}</span>}
      <Button size="sm" variant="ghost" className="ml-auto" onClick={() => copy(record.prompt)} aria-label={t("worker.prompt.copy")}>
        <MotionIcon active={copied} from="copy" to="check" size={13} />{t(copied ? "common.copied" : "worker.prompt.copy")}
      </Button>
    </div>
    {record.redacted && <p className="text-cx-xs leading-relaxed text-cx-fg-3">{t("worker.prompt.redacted")}</p>}
    <div className="cx-scroll max-h-[48dvh] min-h-[100px] overflow-auto rounded-lg border border-cx-border bg-cx-bg-subtle" tabIndex={0} aria-label={t("worker.prompt.content")}>
      <pre className="m-0 p-3.5 font-mono text-cx-xs leading-[1.7] whitespace-pre-wrap [overflow-wrap:anywhere]">{record.prompt}</pre>
    </div>
  </div>;
}

export function WorkerPromptButton({ deck, workerId, name, compact = false, className, asOf }: {
  deck: DeckState;
  workerId: string;
  name: string;
  compact?: boolean;
  className?: string;
  asOf?: number;
}) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const initialPrompt = workerPrompts(deck, workerId, asOf)[0];
  return <>
    <span className="worker-prompt-trigger" onClick={(event) => event.stopPropagation()} onKeyDown={(event) => event.stopPropagation()}>
      <Button size="sm" variant="ghost" iconOnly={compact} className={className}
        aria-label={t("worker.prompt.open", { name })} data-tooltip={t("worker.prompt.title")}
        onClick={() => setOpen(true)}>
        <Icon name="file" size={13} />{!compact && t("worker.prompt.title")}
      </Button>
    </span>
    <Dialog
      open={open}
      onOpenChange={setOpen}
      size="lg"
      title={t("worker.prompt.heading")}
      description={<>{name}<span className="mt-1 block text-cx-2xs text-cx-fg-4">{workerId}</span></>}
      footer={<Button variant="ghost" onClick={() => setOpen(false)}>{t("settings.close")}</Button>}
    >
      {initialPrompt ? <PromptBody key={initialPrompt.id} record={initialPrompt} />
        : <p className="py-5 text-cx-xs leading-relaxed text-cx-fg-3">{t("worker.prompt.empty")}</p>}
    </Dialog>
  </>;
}
