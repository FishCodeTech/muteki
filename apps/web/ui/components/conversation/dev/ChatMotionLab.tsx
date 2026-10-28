"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import { Button, SegmentedControl, Switch } from "@/components/chat/ui";
import { useStickToBottom } from "@/components/chat/timeline/turnMotion";
import { SourceChips } from "@/components/chat/timeline/SourceChips";
import { setMotionPreference, useMotionPreference } from "@/lib/motionPreference";
import { ConversationTurnProcess } from "../ConversationTurnProcess";
import { ConversationMessage } from "../ConversationMessage";
import {
  buildConversationTurnFold,
  presentConversationTurn,
  settleTurnSegmentsAgainstStatus,
} from "../conversationEventViews";
import { extractUrls } from "../conversationSources";
import { MOCK_SCENARIOS, type MockBranch, type MockStep } from "./chatMotionScenarios";

type Speed = "0.5" | "1" | "2" | "4";
const SPEED_OPTIONS: { value: Speed; label: string }[] = [
  { value: "0.5", label: "0.5×" },
  { value: "1", label: "1×" },
  { value: "2", label: "2×" },
  { value: "4", label: "4×" },
];
type EventStep = Extract<MockStep, { kind: "event" }>;

function allowed(step: MockStep, decision: MockBranch | null): boolean {
  return step.kind !== "event" || !step.branch || step.branch === (decision ?? "approve");
}

function nextIndex(steps: MockStep[], from: number, decision: MockBranch | null): number {
  let index = from;
  while (index < steps.length && !allowed(steps[index], decision)) index += 1;
  return index;
}

function describe(step: EventStep): string {
  const payload = step.event.payload;
  const type = step.event.event_type.replace(/^core\./, "");
  const input = payload.input as Record<string, unknown> | undefined;
  const detail = String(
    payload.text ?? payload.reasoning_summary ?? payload.chunk ?? input?.command
      ?? (Array.isArray(input?.changes) ? (input.changes as Array<{ path: string }>).map((change) => change.path).join(", ") : "")
      ?? "",
  ) || String(payload.status ?? payload.tool_call_id ?? "");
  return `${type}  ${detail.replace(/\s+/g, " ").slice(0, 60)}`;
}

const noop = () => {};

/**
 * Plays scripted turns through the production fold and turn components, so
 * motion can be tuned step by step without a live model.
 */
export function ChatMotionLab() {
  const [scenarioId, setScenarioId] = useState(MOCK_SCENARIOS[0].id);
  const scenario = MOCK_SCENARIOS.find((item) => item.id === scenarioId) ?? MOCK_SCENARIOS[0];
  const [cursor, setCursor] = useState(0);
  const [playing, setPlaying] = useState(true);
  const [speed, setSpeed] = useState<Speed>("1");
  const [decision, setDecision] = useState<MockBranch | null>(null);
  const [run, setRun] = useState(0);
  const [startedAt, setStartedAt] = useState(() => new Date().toISOString());
  const [completedAt, setCompletedAt] = useState<string | undefined>();
  const motion = useMotionPreference();
  const steps = scenario.steps;
  const approvalIndex = steps.findIndex((step) => step.kind === "approval");
  const approval = approvalIndex >= 0 ? steps[approvalIndex] : undefined;
  const awaiting = approvalIndex >= 0 && cursor > approvalIndex && decision === null;

  const restart = useCallback((id = scenarioId) => {
    setScenarioId(id);
    setCursor(0);
    setDecision(null);
    setRun((value) => value + 1);
    setStartedAt(new Date().toISOString());
    setCompletedAt(undefined);
    setPlaying(true);
  }, [scenarioId]);

  useEffect(() => {
    if (!playing || awaiting) return;
    const index = nextIndex(steps, cursor, decision);
    if (index >= steps.length) {
      setPlaying(false);
      return;
    }
    const timer = window.setTimeout(() => setCursor(index + 1), steps[index].delay / Number(speed));
    return () => window.clearTimeout(timer);
  }, [playing, awaiting, cursor, decision, speed, steps]);

  const applied = useMemo(
    () => steps.slice(0, cursor).filter((step): step is EventStep => step.kind === "event" && allowed(step, decision)),
    [steps, cursor, decision],
  );
  const events = useMemo(() => applied.map((step) => step.event), [applied]);
  const turnId = String(steps.find((step): step is EventStep => step.kind === "event")?.event.payload.turn_id ?? "");
  const fold = useMemo(() => buildConversationTurnFold(events), [events]);
  const terminal = events.findLast((event) => /^core\.turn\.(completed|failed|interrupted)$/.test(event.event_type));
  const status = terminal ? terminal.event_type.slice("core.turn.".length) : "running";
  const running = status === "running";
  const segments = settleTurnSegmentsAgainstStatus(fold.segmentsByTurn[turnId] || [], status);
  const presentation = presentConversationTurn(segments, status);
  const lastSegment = segments.at(-1);

  useEffect(() => {
    if (running) setCompletedAt(undefined);
    else setCompletedAt((value) => value ?? new Date().toISOString());
  }, [running]);

  const step = useCallback(() => {
    setPlaying(false);
    if (awaiting) return;
    setCursor((value) => Math.min(steps.length, nextIndex(steps, value, decision) + 1));
  }, [awaiting, steps, decision]);

  const scrub = (value: number) => {
    setPlaying(false);
    if (value < cursor) {
      setRun((current) => current + 1);
      if (approvalIndex < 0 || value <= approvalIndex) setDecision(null);
    }
    setCursor(value);
  };

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target?.closest("input, textarea, [contenteditable]")) return;
      if (event.key === " ") {
        event.preventDefault();
        setPlaying((value) => !value);
      } else if (event.key === "ArrowRight") {
        event.preventDefault();
        step();
      } else if (event.key.toLowerCase() === "r" && !event.metaKey && !event.ctrlKey) {
        restart();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [restart, step]);

  const stick = useStickToBottom<HTMLElement>(running);

  return (
    <div className="cx-root flex h-full min-h-0 w-full bg-cx-bg text-cx-fg" data-testid="chat-motion-lab">
      <aside className="flex w-[300px] shrink-0 flex-col gap-5 overflow-y-auto border-r border-cx-border-subtle p-4 text-[13px]">
        <div>
          <h1 className="text-[15px] font-semibold">动效实验室</h1>
          <p className="mt-1 text-[12px] leading-5 text-cx-fg-3">脚本化的 Codex 事件经真实折叠管线播放，不消耗模型额度。空格 播放/暂停 · → 单步 · R 重播</p>
        </div>

        <section className="flex flex-col gap-1">
          <div className="mb-1 text-[12px] font-medium text-cx-fg-3">场景</div>
          {MOCK_SCENARIOS.map((item) => (
            <button
              key={item.id}
              type="button"
              onClick={() => restart(item.id)}
              data-active={item.id === scenarioId || undefined}
              className="cx-press rounded-lg px-2.5 py-2 text-left hover:bg-cx-hover data-[active]:bg-cx-active"
            >
              <div className="font-medium">{item.title}</div>
              <div className="text-[12px] leading-4 text-cx-fg-3">{item.description}</div>
            </button>
          ))}
        </section>

        <section className="flex flex-col gap-3">
          <div className="flex items-center gap-1.5">
            <Button size="sm" variant="secondary" icon={playing ? "pause" : "play"} onClick={() => setPlaying((value) => !value)} disabled={awaiting} data-testid="lab-play">
              {playing ? "暂停" : "播放"}
            </Button>
            <Button size="sm" variant="ghost" icon="chevronRight" onClick={step} disabled={awaiting || cursor >= steps.length}>单步</Button>
            <Button size="sm" variant="ghost" icon="retry" onClick={() => restart()} data-testid="lab-restart">重播</Button>
          </div>
          <div>
            <div className="mb-1 flex justify-between text-[12px] text-cx-fg-3">
              <span>进度</span>
              <span className="cx-tabular">{cursor} / {steps.length}</span>
            </div>
            <input
              type="range"
              className="cx-slider w-full"
              min={0}
              max={steps.length}
              value={cursor}
              onChange={(event) => scrub(Number(event.target.value))}
              aria-label="播放进度"
            />
          </div>
          <div className="flex items-center justify-between">
            <span className="text-[12px] text-cx-fg-3">倍速</span>
            <SegmentedControl value={speed} onChange={setSpeed} options={SPEED_OPTIONS} size="xs" ariaLabel="播放倍速" />
          </div>
          <Switch
            checked={motion === "reduce"}
            onCheckedChange={(checked) => setMotionPreference(checked ? "reduce" : "system")}
            label="减少动效"
            description="与设置中的动效偏好同步"
            size="sm"
          />
        </section>

        <section className="min-h-0 flex-1">
          <div className="mb-1 text-[12px] font-medium text-cx-fg-3">最近事件</div>
          <ol className="flex flex-col gap-0.5 font-cx-mono text-[11px] leading-4 text-cx-fg-3">
            {applied.slice(-14).map((item) => (
              <li key={item.event.seq} className="cx-enter-soft truncate">{describe(item)}</li>
            ))}
          </ol>
        </section>
      </aside>

      <main ref={stick} className="min-w-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex max-w-[760px] flex-col gap-4 px-6 pb-40 pt-10" key={`${scenario.id}:${run}`}>
          <ConversationMessage role="user" text={scenario.prompt} showActions={false} />
          <div className="assistant flex w-full flex-col">
            <ConversationTurnProcess
              turnId={`${turnId}:${run}`}
              status={status}
              segments={presentation.activity}
              answerText={presentation.answerText}
              running={running}
              liveIndicator={!awaiting}
              turnCreatedAt={startedAt}
              turnCompletedAt={running ? undefined : completedAt}
              onOpenDrawer={noop}
              onOpenToolDiff={noop}
            />
            {awaiting && approval?.kind === "approval" ? (
              <div className="cx-enter mb-3 rounded-xl border border-cx-border bg-cx-elevated p-3 shadow-cx-sm" data-testid="lab-approval">
                <div className="text-[13px] font-medium">需要批准运行命令</div>
                <div className="mt-0.5 text-[12px] text-cx-fg-3">{approval.reason}</div>
                <pre className="cx-term mt-2 overflow-x-auto rounded-lg px-3 py-2 font-cx-mono text-[12px]">{approval.command}</pre>
                <div className="mt-2.5 flex gap-2">
                  <Button size="sm" variant="primary" onClick={() => { setDecision("approve"); setPlaying(true); }}>批准执行</Button>
                  <Button size="sm" variant="ghost" onClick={() => { setDecision("decline"); setPlaying(true); }}>拒绝</Button>
                </div>
              </div>
            ) : null}
            {presentation.answerText ? (
              <ConversationMessage
                role="assistant"
                messageId={`msg-${turnId}-${run}`}
                turnId={turnId}
                text={presentation.answerText}
                isStreaming={running && lastSegment?.kind === "text" && lastSegment.phase === "final_answer"}
                showIdentity={false}
                showActions={!running}
              />
            ) : null}
            {!running && presentation.answerText ? (
              <SourceChips urls={[...new Set(extractUrls(presentation.answerText))]} className="mt-2" />
            ) : null}
            {!running && status === "failed" ? (
              <p className={cn("cx-enter text-[13px] text-cx-danger")}>{String(terminal?.payload.error ?? "回合失败")}</p>
            ) : null}
          </div>
        </div>
      </main>
    </div>
  );
}
