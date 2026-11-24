"use client";

import React, { useEffect, useMemo, useState } from "react";
import { Button, Dialog, TextField } from "@muteki/ui";
import { cn } from "@/lib/cn";
import {
  formatScheduledTime,
  fromDateTimeLocalValue,
  laterPresets,
  toDateTimeLocalValue,
} from "@/lib/composerLater";

export interface LaterTarget {
  at: number;
  label: string;
}

export function ComposerLaterDialog({
  open,
  onOpenChange,
  onConfirm,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onConfirm: (target: LaterTarget) => void;
}) {
  const [choice, setChoice] = useState("10m");
  const [custom, setCustom] = useState("");
  const [error, setError] = useState("");
  const presets = useMemo(() => (open ? laterPresets() : []), [open]);

  useEffect(() => {
    if (!open) return;
    setChoice("10m");
    setError("");
    setCustom(toDateTimeLocalValue(Math.floor(Date.now() / 1000) + 3600));
  }, [open]);

  const confirm = () => {
    const preset = presets.find((row) => row.id === choice);
    if (preset) {
      onConfirm({ at: preset.at, label: preset.label });
      return;
    }
    const at = fromDateTimeLocalValue(custom);
    if (at == null) {
      setError("请选择发送时间");
      return;
    }
    if (at * 1000 <= Date.now()) {
      setError("发送时间必须晚于当前时间");
      return;
    }
    onConfirm({ at, label: formatScheduledTime(at) });
  };

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title="定时发送这条消息"
      description="消息进入队列，到时间自动发送；在队列中可以随时修改、提前发送或删除。"
      icon="clock"
      size="sm"
      testId="composer-later-dialog"
      footer={(
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" data-testid="composer-later-confirm" onClick={confirm}>定时发送</Button>
        </>
      )}
    >
      <div className="flex flex-col gap-2" role="radiogroup" aria-label="发送时间">
        {presets.map((preset) => (
          <label
            key={preset.id}
            className={cn(
              "flex cursor-pointer items-center justify-between gap-2 rounded-xl border px-3 py-2 text-cx-sm",
              choice === preset.id ? "border-cx-accent bg-cx-accent-soft text-cx-fg" : "border-cx-border text-cx-fg-2 hover:bg-cx-hover",
            )}
          >
            <span className="flex items-center gap-2">
              <input
                type="radio"
                name="later-preset"
                value={preset.id}
                checked={choice === preset.id}
                onChange={() => { setChoice(preset.id); setError(""); }}
                className="accent-cx-accent"
              />
              {preset.label}
            </span>
            <span className="cx-tabular text-cx-xs text-cx-fg-4">{formatScheduledTime(preset.at)}</span>
          </label>
        ))}
        <label
          className={cn(
            "flex cursor-pointer flex-col gap-2 rounded-xl border px-3 py-2 text-cx-sm",
            choice === "custom" ? "border-cx-accent bg-cx-accent-soft text-cx-fg" : "border-cx-border text-cx-fg-2 hover:bg-cx-hover",
          )}
        >
          <span className="flex items-center gap-2">
            <input
              type="radio"
              name="later-preset"
              value="custom"
              checked={choice === "custom"}
              onChange={() => { setChoice("custom"); setError(""); }}
              className="accent-cx-accent"
            />
            自定义
          </span>
          <TextField
            label="发送时间"
            type="datetime-local"
            value={custom}
            onFocus={() => setChoice("custom")}
            onChange={(event) => { setChoice("custom"); setCustom(event.target.value); setError(""); }}
            error={error || undefined}
          />
        </label>
      </div>
    </Dialog>
  );
}
