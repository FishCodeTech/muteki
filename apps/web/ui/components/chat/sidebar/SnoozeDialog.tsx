"use client";

import { useEffect, useMemo, useState } from "react";
import { Button, Dialog, TextField } from "@/components/chat/ui";
import { formatWakeTime, toLocalInputValue } from "@/lib/sidebarInbox";

const MINUTE = 60_000;
const DURATIONS: { label: string; ms: number }[] = [
  { label: "30 分钟", ms: 30 * MINUTE },
  { label: "2 小时", ms: 120 * MINUTE },
  { label: "6 小时", ms: 360 * MINUTE },
  { label: "2 天", ms: 2 * 1440 * MINUTE },
  { label: "1 周", ms: 7 * 1440 * MINUTE },
];

export function SnoozeDialog({
  open,
  subject,
  onOpenChange,
  onConfirm,
}: {
  open: boolean;
  /** "「标题」" or "3 个对话". */
  subject: string;
  onOpenChange: (open: boolean) => void;
  onConfirm: (until: number) => void;
}) {
  const [value, setValue] = useState("");
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (!open) return;
    const current = Date.now();
    setNow(current);
    const tomorrow = new Date(current);
    tomorrow.setDate(tomorrow.getDate() + 1);
    tomorrow.setHours(9, 0, 0, 0);
    setValue(toLocalInputValue(tomorrow.getTime()));
  }, [open]);

  const until = useMemo(() => {
    const parsed = value ? new Date(value).getTime() : NaN;
    return Number.isFinite(parsed) ? parsed : null;
  }, [value]);
  const error = until === null ? "请选择提醒时间" : until <= Date.now() ? "提醒时间需要晚于现在" : "";

  const submit = () => {
    if (until === null || until <= Date.now()) return;
    onConfirm(until);
    onOpenChange(false);
  };

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      size="sm"
      icon="clock"
      title="自定义稍后提醒"
      description={`${subject}会先移出列表，到时间后带着「已唤醒」标记回来。需要你审批或回复时会提前回来。`}
      footer={(
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" disabled={Boolean(error)} onClick={submit}>
            {until !== null && !error ? `${formatWakeTime(until)} 提醒` : "设置提醒"}
          </Button>
        </>
      )}
    >
      <form
        className="flex flex-col gap-3"
        onSubmit={(event) => {
          event.preventDefault();
          submit();
        }}
      >
        <div className="flex flex-wrap gap-1.5" role="group" aria-label="快速选择时长">
          {DURATIONS.map((duration) => (
            <Button key={duration.label} type="button" size="sm" variant="outline" onClick={() => setValue(toLocalInputValue(Date.now() + duration.ms))}>
              {duration.label}
            </Button>
          ))}
        </div>
        <TextField
          type="datetime-local"
          label="提醒时间"
          value={value}
          min={toLocalInputValue(now)}
          onChange={(event) => setValue(event.target.value)}
          error={value ? error || undefined : undefined}
          data-autofocus=""
        />
      </form>
    </Dialog>
  );
}
