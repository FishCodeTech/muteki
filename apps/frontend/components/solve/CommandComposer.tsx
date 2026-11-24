// ai-design-check-ignore-file use-shared-components -- shared composer action pills
"use client";
import { useRef, useState, type ReactNode } from "react";
import { PromptBar } from "@/components/chat/composer/MutekiPromptBar";
import { Button, Tooltip } from "@muteki/ui";
import type { ControlCommandOpts } from "@/components/Conversation";
import type { ControlCommand, DeckState } from "@/lib/events";
import { Icon, type IconName } from "@muteki/ui";
import { useT } from "@/lib/i18n";
import { cn } from "@/lib/cn";
import { isImeComposingKeyEvent } from "@/lib/imeKeyboard";
import { localPiSessionStateLabel } from "../run-view/LocalPiInspector";
import { EmergencyStopConfirmDialog } from "../run-view/PentestGuardApprovalQueue";
export interface FollowupActions {
  onFollowup: (kind: "ask" | "writeup", question: string, opts?: { templateId?: string; customPrompt?: string }) => Promise<{ ok: boolean }>;
  onCancelFollowup: (id: string) => Promise<void>;
  onOpenWriteup?: () => void;
}
const SLASH_VERBS = new Set([
  "ask",
  "writeup",
  "resolve",
  "hint",
  "focus",
  "redirect",
  "directive",
  "correction",
  "mark_false",
  "pause",
  "resume",
  "stop",
  "freeze",
  "thaw",
  "progress",
]);
const NO_PAYLOAD_VERBS = new Set([
  "writeup",
  "resolve",
  "pause",
  "stop",
  "resume",
  "progress",
  "freeze",
  "thaw",
]);
type Tone = "default" | "accent" | "danger";
const TONE: Record<Tone, string> = {
  default: "text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg-2",
  accent: "text-cx-accent hover:bg-cx-accent-soft",
  danger: "text-cx-danger hover:bg-cx-danger-soft",
};
function ActionChip({ icon, label, tip, tone = "default", minor, disabled, busy, onPress }: {
  icon: IconName; label: ReactNode; tip: string; tone?: Tone; minor?: boolean; disabled?: boolean; busy?: boolean; onPress: () => void;
}) {
  return (
    <Tooltip content={tip} placement="top">
      <button
        type="button"
        aria-label={tip}
        aria-busy={busy || undefined}
        disabled={disabled}
        onClick={onPress}
        className={cn(
          "cx-press cc-action-chip inline-flex h-[26px] min-w-0 shrink-0 items-center gap-[5px] rounded-full px-[10px] outline-none transition-colors",
          "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)] disabled:cursor-not-allowed disabled:opacity-50",
          minor && "cc-chip-minor",
          TONE[tone],
        )}
      >
        <Icon name={icon} size={13} className={cn("shrink-0", icon === "loader" && "cx-spin")} />
        <span className="cc-chip-label truncate">{label}</span>
      </button>
    </Tooltip>
  );
}

export function CommandComposer({ mode, driverKind, localPiSession, phase, text, onTextChange,
  running, paused, followupPending, writeupGenerating, reportGenerating, activeFollowupJobId, hasWriteup, solved, flags,
  onCommand, onRequestProgress, onResolve, onOpenSideChat, onOpenReport, onFollowup,
  onCancelFollowup, onOpenWriteup, receipt, compact, onRequestExpand,
  productionGuardProtected, guardPendingCount, guardEmergencyStopping, onEmergencyStop,
}: FollowupActions & {
  runId: string; mode: "ctf" | "pentest"; driverKind?: "idle" | "swarm" | "local_pi";
  localPiSession?: DeckState["localPiSession"]; phase: "running" | "finished";
  text: string; onTextChange: (text: string) => void; running: boolean; paused: boolean;
  followupPending: boolean; writeupGenerating?: boolean; reportGenerating?: boolean; activeFollowupJobId?: string; hasWriteup: boolean;
  solved: boolean; solvers: string[]; flags: string[];
  onCommand: (target: string, action: string, text: string, opts?: ControlCommandOpts) => Promise<boolean>;
  onRequestProgress: () => Promise<boolean>; onResolve: (text?: string) => Promise<boolean>;
  onOpenSideChat?: (question?: string) => void; onOpenReport?: () => void; receipt?: ControlCommand;
  compact?: boolean; onRequestExpand?: () => void;
  productionGuardProtected?: boolean;
  guardPendingCount?: number;
  guardEmergencyStopping?: boolean;
  onEmergencyStop?: () => Promise<boolean>;
}) {
  const t = useT();
  const finished = phase === "finished";
  const pentest = mode === "pentest";
  const [notice, setNotice] = useState("");
  const [progressPending, setProgressPending] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [markFalseOpen, setMarkFalseOpen] = useState(false);
  const [emergencyStopOpen, setEmergencyStopOpen] = useState(false);
  const [emergencyStopBusy, setEmergencyStopBusy] = useState(false);
  const [composerUndoEpoch, setComposerUndoEpoch] = useState(0);
  const draft = useRef(text);
  draft.current = text;
  const clearSubmitted = (submitted: string) => {
    if (draft.current === submitted) {
      setComposerUndoEpoch((n) => n + 1);
      onTextChange("");
    }
  };
  const resume = async (override?: string) => {
    if (submitting) return;
    const submitted = text;
    const guidance = (override !== undefined ? override : submitted).trim() || undefined;
    setSubmitting(true);
    try { if (await onResolve(guidance)) clearSubmitted(submitted); }
    finally { setSubmitting(false); }
  };
  const submit = async () => {
    if (submitting || !text.trim()) return;
    const sideCommand = /^\/(side|btw)(\s|$)/i.test(text.trim());
    if (!sideCommand && ((!finished && !running) || followupPending)) return;
    const submitted = text;
    const raw = text.trim();
    const slash = raw.match(/^\/([a-z_]+)(?:\s+([\s\S]*))?$/i);
    const verb = slash?.[1].toLowerCase();
    const payload = slash ? (slash[2] || "").trim() : raw;
    setNotice("");
    if ((verb === "side" || verb === "btw") && onOpenSideChat) {
      onOpenSideChat(payload || undefined);
      clearSubmitted(submitted);
      return;
    }
    if (verb && !SLASH_VERBS.has(verb)) {
      setNotice(`未知命令 /${verb}，请检查后重试`); return;
    }
    if (finished && verb && ["hint", "focus", "redirect", "directive", "correction"].includes(verb)) {
      onTextChange(payload);
      setNotice("已准备为新思路，请点击「带着新思路继续」或按 Ctrl/Cmd+Shift+Enter"); return;
    }
    if (finished && verb && ["pause", "stop", "progress", "resume", "freeze", "thaw"].includes(verb)) {
      setNotice("任务已结束"); return;
    }
    if (finished && verb === "mark_false" && !payload) {
      setNotice("请提供需要标记误报的 Flag，或点击「标记误报」按钮"); return;
    }
    if (!finished && verb === "writeup") { setNotice("请等待任务结束后生成 Writeup"); return; }
    if (!finished && verb === "resolve") { setNotice("任务仍在运行中，无需重开"); return; }
    if (!finished && verb === "mark_false") { setNotice("请等待任务结束后标记误报"); return; }
    if (!payload && (!verb || !NO_PAYLOAD_VERBS.has(verb))) {
      setNotice("请输入正文"); return;
    }
    if (verb === "resolve") {
      await resume(payload);
      return;
    }
    setSubmitting(true);
    try {
      let ok = false;
      if (verb === "ask" || (finished && (!verb || verb === "writeup"))) {
        ok = (await onFollowup(verb === "writeup" ? "writeup" : "ask", payload)).ok;
      } else if (verb === "progress") {
        ok = await onRequestProgress();
      } else if (verb === "mark_false") {
        ok = await onCommand("global", "mark_false", payload);
      } else if (verb && ["pause", "stop", "resume", "freeze", "thaw"].includes(verb)) {
        ok = await onCommand("global", verb, "");
      } else {
        const action = verb && ["hint", "focus", "redirect", "directive", "correction"].includes(verb) ? verb : "hint";
        ok = await onCommand("global", action, payload);
      }
      if (ok) clearSubmitted(submitted);
    } finally { setSubmitting(false); }
  };
  const progress = async () => {
    setProgressPending(true);
    try { await onRequestProgress(); } finally { setProgressPending(false); }
  };
  const piActive = driverKind === "local_pi" && Boolean(localPiSession?.turnActive ?? localPiSession?.turn_active);
  const piRawState = localPiSession?.sessionState || localPiSession?.session_state || (finished ? "stopped" : "running");
  const piStateText = localPiSessionStateLabel(
    finished && piRawState !== "failed" && piRawState !== "resume_failed" ? "stopped" : piRawState,
  );
  const continueLabel = text.trim() ? "带着新思路继续" : pentest ? "继续测试" : "继续做题";
  const statusText = notice || (receipt ? receipt.detail || t(`control.status.${receipt.status}`) : "");
  return <div className="cx-root command-composer mx-auto flex w-full max-w-[768px] flex-col px-6 pb-3 pt-2"
    onKeyDownCapture={event => {
      if (finished && event.key === "Enter" && event.shiftKey && (event.metaKey || event.ctrlKey) && !isImeComposingKeyEvent(event)) {
        event.preventDefault(); event.stopPropagation(); void resume();
      }
    }}>
    <PromptBar value={text} onChange={onTextChange} onSubmit={() => void submit()}
      submitDisabled={!text.trim() || submitting || (!/^\/(side|btw)(\s|$)/i.test(text.trim()) && (followupPending || (!finished && !running)))}
      running={piActive} onStop={piActive ? () => void onCommand("global", "interrupt_turn", "") : undefined}
      sendKey="enter" placeholder={finished ? "输入追问，或带着新思路继续" : "输入对当前任务的引导"}
      composerLabel="当前任务" compact={compact} onRequestExpand={onRequestExpand}
      undoScope={String(composerUndoEpoch)} />
    <div className="cc-toolbar flex flex-wrap items-center gap-1 pt-2">
      {driverKind === "local_pi" && <span className="inline-flex h-[26px] items-center rounded-full bg-cx-hover px-2.5 text-cx-xs text-cx-fg-3">Pi 会话 · {piStateText}</span>}
      {finished ? <>
        <ActionChip icon="refresh" tone="accent" label={continueLabel} tip={`${continueLabel} · Ctrl/Cmd+Shift+Enter`} disabled={submitting} onPress={() => void resume()} />
        {!pentest && <ActionChip
          icon={writeupGenerating ? "loader" : hasWriteup ? "rows" : "pencil"}
          label={writeupGenerating ? "生成 Writeup 中…" : hasWriteup ? "查看 Writeup" : "生成 Writeup"}
          tip={writeupGenerating ? "正在生成 Writeup" : hasWriteup ? "查看 Writeup" : "生成 Writeup"}
          busy={writeupGenerating}
          disabled={!hasWriteup && (followupPending || writeupGenerating)}
          onPress={() => { if (hasWriteup) onOpenWriteup?.(); else void onFollowup("writeup", ""); }}
        />}
        {pentest && onOpenReport && <ActionChip
          icon={reportGenerating ? "loader" : "rows"}
          label={reportGenerating ? "生成报告中…" : "查看报告"}
          tip={reportGenerating ? "正在生成测试报告" : "查看报告"}
          busy={reportGenerating}
          onPress={onOpenReport}
        />}
      </> : running ? <ActionChip icon="radio" label="询问进展" tip="询问进展" disabled={progressPending} onPress={() => void progress()} /> : null}
      {onOpenSideChat && <ActionChip icon="messages" label="侧边聊天" tip="带着 Run 上下文另开一个聊天，可查看和操作 Run · ⌘/Ctrl+Shift+/ 或 /side" onPress={() => onOpenSideChat()} />}
      {finished && !pentest && solved && <ActionChip icon="alert" label="标记误报" tip="标记误报" tone="danger" onPress={() => {
        if (flags.length === 1) void onCommand("global", "mark_false", flags[0]); else setMarkFalseOpen(!markFalseOpen);
      }} />}
      {followupPending && activeFollowupJobId && <ActionChip icon="x" label="取消" tip="取消追问或 Writeup" onPress={() => void onCancelFollowup(activeFollowupJobId)} />}
      {!finished && !running && <span role="status" className="inline-flex h-[26px] items-center text-cx-xs text-cx-fg-3">任务正在结束，请等待完成事件</span>}
      <span role="status" className={cn("flex-1 text-right text-cx-xs text-cx-fg-3", statusText ? "min-w-[min(100%,10rem)]" : "min-w-0")}>{statusText}</span>
      {!finished && running && <>
        <ActionChip icon={paused ? "play" : "pause"} label={paused ? "继续" : "暂停"} tip={paused ? "继续" : "暂停"} tone={paused ? "accent" : "default"} onPress={() => void onCommand("global", paused ? "resume" : "pause", "")} />
        <ActionChip icon="xCircle" label="停止" tip="停止任务" tone="danger" onPress={() => void onCommand("global", "stop", "")} />
      </>}
      {pentest && productionGuardProtected && !finished && (running || (guardPendingCount ?? 0) > 0) && onEmergencyStop ? (
        <Button
          size="xs"
          variant="danger"
          icon="stopCircle"
          loading={emergencyStopBusy || Boolean(guardEmergencyStopping)}
          disabled={emergencyStopBusy || Boolean(guardEmergencyStopping)}
          data-testid="composer-emergency-stop"
          onClick={() => setEmergencyStopOpen(true)}
        >
          紧急停止 Run
        </Button>
      ) : null}
    </div>
    <EmergencyStopConfirmDialog
      open={emergencyStopOpen}
      busy={emergencyStopBusy}
      onOpenChange={setEmergencyStopOpen}
      onConfirm={async () => {
        if (!onEmergencyStop || emergencyStopBusy) return;
        setEmergencyStopBusy(true);
        try {
          await onEmergencyStop();
        } finally {
          setEmergencyStopBusy(false);
          setEmergencyStopOpen(false);
        }
      }}
    />
    {markFalseOpen && (
      <div className="mt-2 flex flex-wrap gap-1.5">
        {flags.map(flag => (
          <button
            key={flag}
            type="button"
            className="cx-press inline-flex max-w-full items-center break-all rounded-lg border border-cx-border bg-cx-elevated px-2.5 py-1 text-left font-cx-mono text-cx-xs text-cx-danger transition-colors hover:border-cx-danger/40 hover:bg-cx-danger-soft"
            onClick={() => { void onCommand("global", "mark_false", flag); setMarkFalseOpen(false); }}
          >
            {flag}
          </button>
        ))}
      </div>
    )}
  </div>;
}
