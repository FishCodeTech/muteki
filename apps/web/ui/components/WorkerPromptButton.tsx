"use client";

import { MotionIcon } from "@/components/MotionIcon";

import { Button, Chip, Modal, ScrollShadow } from "@heroui/react";
import { useState } from "react";
import { Icon } from "@/components/Icon";
import type { DeckState, WorkerPromptRecord } from "@/lib/events";
import { formatClock } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { useCopied } from "@/lib/useCopied";
import { workerPrompts } from "@/lib/workerPrompts";

function PromptBody({ record }: { record: WorkerPromptRecord }) {
  const t = useT();
  const [copied, copy] = useCopied();
  return <>
    <div className="worker-prompt-meta">
      <Chip size="sm" variant="soft" color={record.status === "sent" ? "success" : record.status === "prepared" ? "default" : "warning"}>
        {t(`worker.prompt.status.${record.status}`)}
      </Chip>
      <time>{formatClock(record.ts, "—")}</time>
      {record.model && <span>{record.model}</span>}
      {record.intentId && <span title={record.intentId}>{record.intentId}</span>}
      <Button size="sm" variant="ghost" onPress={() => copy(record.prompt)} aria-label={t("worker.prompt.copy")}>
        <MotionIcon active={copied} from="copy" to="check" size={13} />{t(copied ? "common.copied" : "worker.prompt.copy")}
      </Button>
    </div>
    {record.redacted && <p className="worker-prompt-note">{t("worker.prompt.redacted")}</p>}
    <ScrollShadow className="worker-prompt-text" tabIndex={0} aria-label={t("worker.prompt.content")}>
      <pre>{record.prompt}</pre>
    </ScrollShadow>
  </>;
}

/** One shared HeroUI viewer for runtime rosters and collaboration details. */
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
  // The user wants the prompt carried when this worker was created, not the
  // latest checkpoint/continuation prompt. Keep the first invocation visible.
  const initialPrompt = workerPrompts(deck, workerId, asOf)[0];
  return <span className="worker-prompt-trigger" onClick={(event) => event.stopPropagation()} onKeyDown={(event) => event.stopPropagation()}>
    <Button size="sm" variant="ghost" isIconOnly={compact} className={className}
      aria-label={t("worker.prompt.open", { name })} data-tooltip={t("worker.prompt.title")}
      onPress={() => setOpen(true)}>
      <Icon name="file" size={13} />{!compact && t("worker.prompt.title")}
    </Button>
    {open && <Modal isOpen onOpenChange={setOpen}>
      <Modal.Backdrop>
        <Modal.Container size="lg">
          <Modal.Dialog className="worker-prompt-dialog">
            <Modal.CloseTrigger aria-label={t("dialog.close")} />
            <Modal.Header>
              <Modal.Heading>{t("worker.prompt.heading")}</Modal.Heading>
              <p className="worker-prompt-name">{name}<span>{workerId}</span></p>
            </Modal.Header>
            <Modal.Body className="worker-prompt-body">
              {initialPrompt ? <PromptBody key={initialPrompt.id} record={initialPrompt} />
                : <p className="worker-prompt-empty">{t("worker.prompt.empty")}</p>}
            </Modal.Body>
            <Modal.Footer><Button variant="ghost" onPress={() => setOpen(false)}>{t("settings.close")}</Button></Modal.Footer>
          </Modal.Dialog>
        </Modal.Container>
      </Modal.Backdrop>
    </Modal>}
  </span>;
}
