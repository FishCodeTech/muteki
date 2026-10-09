"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { ChatMarkdown } from "@/components/chat/markdown/ChatMarkdown";
import { Badge, Button, Shortcut, TextArea } from "@/components/chat/ui";
import { ApprovalCard } from "../ai-native/approval-card";
import {
  ApprovalCard as QuestionCard,
  type ApprovalCardAnswers,
  type ApprovalCardQuestion,
} from "@/components/agentui/agents/approval-card";
import {
  approvalDiffFiles,
  buildApprovalPreview,
} from "@/lib/approvalPreview";
import { Icon } from "@/components/Icon";
import { useT } from "@/lib/i18n";
import { userInputAnswerError } from "@/lib/userInputValidation";
import {
  clearUserInputDraft,
  loadUserInputDraft,
  saveUserInputDraft,
  type UserInputAnswerMap,
} from "@/lib/userInputDraftStore";

/* ─────────────────────────────────────────────────────────
 * CONVERSATION APPROVAL — HITL approvals + structured user input (C22).
 * ───────────────────────────────────────────────────────── */

export type UserInputResolvePayload = {
  request_id: string;
  decision: "submit" | "cancel" | "decline";
  answers?: UserInputAnswerMap;
  text?: string;
};

export interface ConversationApprovalProps {
  pendingApproval: Record<string, unknown> | null;
  pendingApprovals?: Record<string, Record<string, unknown>> | null;
  pendingInput: Record<string, unknown> | null;
  threadId?: string;
  busy?: boolean;
  onApprovalDecision: (
    approvalId: string,
    decision: "allow" | "deny",
    scopeMode?: "once" | "session",
    optionId?: string,
    note?: string,
  ) => void;
  onUserInputResolve: (payload: UserInputResolvePayload) => void | Promise<boolean>;
  className?: string;
}

type QuestionOption = {
  value: string;
  label: string;
  recommended: boolean;
};

type NormalizedQuestion = {
  question_id: string;
  kind: string;
  prompt: string;
  required: boolean;
  options: QuestionOption[];
  allow_free_text: boolean;
  placeholder: string;
  schema?: Record<string, unknown>;
};

function field(row: Record<string, unknown>, ...keys: string[]): string {
  for (const key of keys) {
    const val = row[key];
    if (val != null && val !== "") {
      return typeof val === "string" ? val : JSON.stringify(val, null, 2);
    }
  }
  return "";
}

function asApprovalRows(
  pendingApprovals: Record<string, Record<string, unknown>> | null | undefined,
  pendingApproval: Record<string, unknown> | null,
): Record<string, unknown>[] {
  const map = pendingApprovals || {};
  const rows = Object.values(map).filter(
    (row) => row && typeof row === "object" && field(row, "approval_id"),
  );
  if (rows.length) {
    return [...rows].sort((a, b) => {
      const aAt = field(a, "requested_at");
      const bAt = field(b, "requested_at");
      if (aAt !== bAt) return aAt.localeCompare(bAt);
      return field(a, "approval_id").localeCompare(field(b, "approval_id"));
    });
  }
  if (pendingApproval && field(pendingApproval, "approval_id")) {
    return [pendingApproval];
  }
  return [];
}


function normalizeOption(raw: unknown): QuestionOption | null {
  if (typeof raw === "string" && raw.trim()) {
    return { value: raw.trim(), label: raw.trim(), recommended: false };
  }
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const row = raw as Record<string, unknown>;
  const value = String(row.value ?? row.id ?? row.label ?? row.title ?? "").trim();
  if (!value && row.value !== "") return null;
  const label = String(row.label ?? row.title ?? value).trim() || value || "空字符串";
  const recommended = Boolean(row.recommended ?? row.isRecommended ?? row.is_recommended);
  return { value, label, recommended };
}

function normalizeQuestions(pending: Record<string, unknown>): NormalizedQuestion[] {
  const raw = pending.questions;
  if (Array.isArray(raw) && raw.length > 0) {
    return raw
      .map((item, index): NormalizedQuestion | null => {
        if (!item || typeof item !== "object" || Array.isArray(item)) return null;
        const row = item as Record<string, unknown>;
        const options = Array.isArray(row.options)
          ? row.options.map(normalizeOption).filter((opt): opt is QuestionOption => Boolean(opt))
          : [];
        return {
          question_id: String(row.question_id ?? row.id ?? `q${index}`),
          kind: String(row.kind ?? (options.length ? "single_select" : "free_text")),
          prompt: String(row.prompt ?? row.question ?? row.message ?? row.header ?? ""),
          required: row.kind === "header" ? false : row.required !== false,
          schema: row.schema && typeof row.schema === "object" && !Array.isArray(row.schema) ? row.schema as Record<string, unknown> : undefined,
          options,
          allow_free_text: Boolean(row.allow_free_text ?? (!options.length && row.kind !== "header")),
          placeholder: String(row.placeholder ?? ""),
        } satisfies NormalizedQuestion;
      })
      .filter((item): item is NormalizedQuestion => Boolean(item));
  }

  const prompt = field(pending, "question", "prompt", "message") || "Agent 请求补充信息";
  const rawChoices = pending.options || pending.choices;
  const options = Array.isArray(rawChoices)
    ? rawChoices.map(normalizeOption).filter((opt): opt is QuestionOption => Boolean(opt))
    : [];
  return [{
    question_id: "answer",
    kind: options.length ? "single_select" : "free_text",
    prompt,
    required: true,
    options,
    allow_free_text: true,
    placeholder: "",
  }];
}

function approvalKindOf(row: Record<string, unknown>): string {
  return field(row, "approval_kind", "kind").toLowerCase().replace(/-/g, "_");
}

const UNANSWERABLE_LABELS: Record<string, string> = {
  session_closed: "所属的 Runtime 会话已关闭",
  session_replaced: "所属的 Runtime 会话已被新会话替换",
  stale_generation: "所属的执行代已过期",
  runtime_restarted: "Runtime 已重启，原请求无法再答复",
  turn_ended: "回合已结束，原请求已过期",
};

/** Empty when the issuing Runtime session can still receive the decision. */
function unanswerableReasonOf(row: Record<string, unknown>): string {
  const capability = row.response_capability;
  if (!capability || typeof capability !== "object") return "";
  const { answerable, reason } = capability as { answerable?: unknown; reason?: unknown };
  if (answerable !== false) return "";
  const code = typeof reason === "string" ? reason : "";
  return `${UNANSWERABLE_LABELS[code] || "原 Runtime 会话不可用"}，不能再作答${code ? `（${code}）` : ""}`;
}

/** Plan-exit cards require the full plan body. An empty string is not a plan. */
function planExitMarkdown(row: Record<string, unknown>): string {
  const value = row.plan_markdown;
  return typeof value === "string" ? value.trim() : "";
}

function PlanExitCard({
  row,
  busy,
  onDecision,
}: {
  row: Record<string, unknown>;
  busy: boolean;
  onDecision: (approvalId: string, decision: "allow" | "deny", note?: string) => void;
}) {
  const approvalId = field(row, "approval_id");
  const markdown = planExitMarkdown(row);
  const status = field(row, "status") || "pending";
  const title = field(row, "title") || "计划已就绪";
  const [note, setNote] = useState("");
  const pending = status === "pending";
  const unanswerable = unanswerableReasonOf(row);
  const decidable = pending && !busy && !unanswerable;
  const implement = () => {
    if (!decidable) return;
    onDecision(approvalId, "allow");
  };
  const keepPlanning = () => {
    if (!decidable) return;
    onDecision(approvalId, "deny", note);
  };

  return (
    <section
      className="flex flex-col gap-3 rounded-2xl border border-cx-border bg-cx-elevated p-3 shadow-cx-sm"
      data-testid="plan-approval-card"
      data-approval-id={approvalId}
      data-status={status}
      aria-label="计划确认"
      onKeyDown={(event) => {
        if (!decidable) return;
        if (event.target instanceof HTMLTextAreaElement || event.target instanceof HTMLInputElement) return;
        if (event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) return;
        const key = event.key.toLowerCase();
        if (key !== "y" && key !== "n") return;
        event.preventDefault();
        if (key === "y") implement();
        else keepPlanning();
      }}
    >
      <div className="flex items-center gap-2">
        <Badge tone="accent" icon="listChecks">规划</Badge>
        <span className="min-w-0 flex-1 truncate text-[13px] font-medium text-cx-fg">{title}</span>
        {status === "expired" ? <Badge tone="neutral">已过期</Badge> : null}
        {status === "resolving" || (pending && busy) ? <Badge tone="running" dot>正在确认</Badge> : null}
      </div>
      <div className="max-h-80 overflow-auto rounded-xl bg-cx-bg-subtle px-3 py-2 text-[13px] leading-5 text-cx-fg">
        <ChatMarkdown text={markdown} />
      </div>
      {unanswerable ? (
        <p className="text-[12px] leading-5 text-cx-warning" role="status" data-testid="approval-unanswerable">{unanswerable}</p>
      ) : null}
      {decidable ? (
        <TextArea
          value={note}
          onChange={(event) => setNote(event.target.value)}
          aria-label="继续规划的补充说明"
          placeholder="继续规划时可写下要改的地方，也可以留空"
          rows={2}
        />
      ) : null}
      <div className="flex flex-wrap items-center gap-2">
        <Button type="button" size="sm" variant="primary" disabled={!decidable} onClick={implement}>
          实施
        </Button>
        <Button type="button" size="sm" variant="secondary" disabled={!decidable} onClick={keepPlanning}>
          继续规划
        </Button>
        {decidable ? (
          <span className="ml-auto hidden items-center gap-2 text-[12px] text-cx-fg-4 sm:flex">
            <Shortcut keys="y" /> 实施
            <Shortcut keys="n" /> 继续规划
          </span>
        ) : null}
      </div>
      <p className="text-[12px] leading-5 text-cx-fg-3">
        实施会退出规划模式并按该计划继续。继续规划会留在规划模式，补充说明会交给 Agent。
      </p>
    </section>
  );
}

function answerSatisfied(question: NormalizedQuestion, entry?: { values: string[]; text?: string }): boolean {
  if (question.kind === "header" || !question.required) return true;
  const values = entry?.values ?? [];
  const text = (entry?.text ?? "").trim();
  return values.length > 0 || Boolean(text);
}

export function ConversationApproval({
  pendingApproval,
  pendingApprovals,
  pendingInput,
  threadId = "",
  busy = false,
  onApprovalDecision,
  onUserInputResolve,
  className = "",
}: ConversationApprovalProps) {
  const t = useT();
  const [answers, setAnswers] = useState<UserInputAnswerMap>({});
  const [hydratedScope, setHydratedScope] = useState("");
  const [resolvingInput, setResolvingInput] = useState(false);
  const resolvingInputRef = useRef(false);
  const [inputResolveError, setInputResolveError] = useState("");

  const requestId = typeof pendingInput?.request_id === "string"
    ? pendingInput.request_id
    : "";
  const answerScope = `${threadId}::${requestId}`;
  const inputBusy = busy || resolvingInput || pendingInput?.status === "resolving";
  const questions = useMemo(
    () => (pendingInput ? normalizeQuestions(pendingInput) : []),
    [pendingInput],
  );
  const title = pendingInput
    ? field(pendingInput, "title", "message") || t("conversation.userInput.title")
    : "";

  useEffect(() => {
    if (!requestId) {
      setAnswers({});
      setHydratedScope(answerScope);
      return;
    }
    const draft = threadId ? loadUserInputDraft(threadId, requestId) : null;
    setAnswers(draft?.answers ?? {});
    setHydratedScope(answerScope);
    setInputResolveError("");
  }, [answerScope, requestId, threadId]);

  useEffect(() => {
    if (!threadId || !requestId || hydratedScope !== answerScope) return;
    saveUserInputDraft(threadId, requestId, answers);
  }, [answers, answerScope, hydratedScope, threadId, requestId]);

  const approvalRows = useMemo(
    () => asApprovalRows(pendingApprovals, pendingApproval),
    [pendingApprovals, pendingApproval],
  );
  const { planExitRows, planExitMissing, genericRows } = useMemo(() => {
    const plans: Record<string, unknown>[] = [];
    const missing: Record<string, unknown>[] = [];
    const generic: Record<string, unknown>[] = [];
    for (const row of approvalRows) {
      if (approvalKindOf(row) !== "plan_exit") {
        generic.push(row);
        continue;
      }
      if (planExitMarkdown(row)) plans.push(row);
      else missing.push(row);
    }
    return { planExitRows: plans, planExitMissing: missing, genericRows: generic };
  }, [approvalRows]);
  const pendingApprovalCount = genericRows.filter(
    (row) => (field(row, "status") || "pending") === "pending",
  ).length;
  const expiredApprovalCount = genericRows.filter(
    (row) => field(row, "status") === "expired",
  ).length;

  const regionRef = useRef<HTMLDivElement | null>(null);

  // Incoming approvals must not move a typing user's focus onto a decision.

  if (!approvalRows.length && !pendingInput) return null;

  const decidePlan = (approvalId: string, decision: "allow" | "deny", note?: string) => {
    onApprovalDecision(approvalId, decision, "once", undefined, note);
  };
  const visiblePlanRows = planExitRows.filter((row) => field(row, "status") !== "expired");
  const expiredPlanRows = planExitRows.filter((row) => field(row, "status") === "expired");
  const planSection = visiblePlanRows.length || expiredPlanRows.length || planExitMissing.length ? (
    <div className="flex flex-col gap-2.5" data-testid="plan-approval-queue">
      {visiblePlanRows.map((row) => (
        <PlanExitCard key={field(row, "approval_id")} row={row} busy={busy} onDecision={decidePlan} />
      ))}
      {planExitMissing.map((row) => {
        const approvalId = field(row, "approval_id");
        const status = field(row, "status") || "pending";
        const pending = status === "pending";
        return (
          <p key={approvalId} className="flex flex-wrap items-center gap-2 text-[12px] leading-5 text-cx-fg-3" data-testid="plan-approval-missing" data-approval-id={approvalId} role="status">
            <span className="min-w-0 flex-1">这条计划确认没有计划正文，未显示计划卡片。</span>
            {pending ? (
              <Button type="button" size="xs" variant="ghost" disabled={busy} onClick={() => decidePlan(approvalId, "deny", "")}>
                忽略
              </Button>
            ) : null}
          </p>
        );
      })}
      {expiredPlanRows.length ? (
        <details className="rounded-xl border border-cx-border px-3 py-2 text-xs text-cx-fg-3" data-testid="expired-plan-approval-history">
          <summary className="cursor-pointer">{expiredPlanRows.length} 个已过期的计划确认</summary>
          <div className="mt-3 flex flex-col gap-2.5">
            {expiredPlanRows.map((row) => (
              <PlanExitCard key={field(row, "approval_id")} row={row} busy={busy} onDecision={decidePlan} />
            ))}
          </div>
        </details>
      ) : null}
    </div>
  ) : null;

  const approvalCards = genericRows.map((row) => {
          const preview = buildApprovalPreview(row);
          const approvalId = preview.approvalId;
          const unanswerable = unanswerableReasonOf(row);
          const answerable = preview.status === "pending" && !unanswerable;
          const diffFiles = approvalDiffFiles(preview);
          const pathFiles = preview.files.map((f) => {
            const matched = diffFiles.find((d) => d.path === f.path);
            return {
              path: f.path,
              status: f.status,
              raw: matched?.raw || f.diff,
              additions: matched?.additions ?? f.additions,
              deletions: matched?.deletions ?? f.deletions,
            };
          });
          // Ensure Diff-only payloads still surface under DiffTable even without files[].
          const cardFiles = pathFiles.length
            ? pathFiles
            : diffFiles.map((d) => ({
                path: d.path,
                raw: d.raw,
                additions: d.additions,
                deletions: d.deletions,
              }));
          return (
            <ApprovalCard
              key={approvalId}
              data-tooltip="需要操作审批"
              approvalId={approvalId}
              action={
                preview.primaryPath
                  ? `${preview.actionLabel} · ${preview.primaryPath}`
                  : preview.files.length > 1
                    ? `${preview.actionLabel} · ${preview.files.length} 个文件`
                    : preview.actionLabel
              }
              command={preview.command || undefined}
              args={preview.args || undefined}
              cwd={preview.cwd || undefined}
              diff={preview.diff || undefined}
              files={cardFiles}
              missingPaths={preview.missingPaths}
              missingDiff={preview.missingDiff}
              scope={preview.scope || undefined}
              expires={preview.expires || undefined}
              reason={unanswerable || preview.reason || undefined}
              status={preview.status}
              busy={busy}
              nativeOptions={unanswerable ? undefined : preview.nativeOptions}
              onNativeOption={(optionId, kind) => {
                if (!answerable || busy) return;
                if (!["allow_once", "allow_always", "reject_once", "reject_always"].includes(kind)
                  || !preview.nativeOptions?.some((option) => option.option_id === optionId && option.kind === kind)) return;
                onApprovalDecision(approvalId, kind === "allow_once" || kind === "allow_always" ? "allow" : "deny", kind === "allow_always" || kind === "reject_always" ? "session" : "once", optionId);
              }}
              onAllow={
                !answerable
                  ? undefined
                  : (scopeMode) => onApprovalDecision(approvalId, "allow", scopeMode)
              }
              onDeny={
                !answerable
                  ? undefined
                  : () => onApprovalDecision(approvalId, "deny")
              }
            />
          );
        });
  const approvalQueue = genericRows.length ? (
    <div className="flex flex-col gap-2.5" data-testid="approval-queue">
      <div className="flex items-center gap-2 text-[12px] font-medium text-cx-fg-3">
        {pendingApprovalCount > 0 ? <span className="size-1.5 rounded-full bg-cx-warning cx-pulse-dot" /> : null}
        {pendingApprovalCount > 0 ? `${pendingApprovalCount} 个操作等待审批` : "审批记录"}
      </div>
      {approvalCards.filter((card) => card.props.status !== "expired")}
      {expiredApprovalCount > 0 ? (
        <details className="rounded-xl border border-cx-border px-3 py-2 text-xs text-cx-fg-3" data-testid="expired-approval-history">
          <summary className="cursor-pointer">{expiredApprovalCount} 个已过期审批记录</summary>
          <div className="mt-3 flex flex-col gap-2.5">{approvalCards.filter((card) => card.props.status === "expired")}</div>
        </details>
      ) : null}
    </div>
  ) : null;

  if (!pendingInput || !requestId) {
    return (
      <div
        ref={regionRef}
        role="region"
        aria-label="待审批操作"
        className={cn("cx-animate-in flex flex-col gap-3", className)}
      >
        {planSection}
        {approvalQueue}
      </div>
    );
  }


  const interactive = questions.filter((q) => q.kind !== "header");
  const canSubmit = interactive.every((q) => answerSatisfied(q, answers[q.question_id])
    && !userInputAnswerError(q, answers[q.question_id]));

  const cardQuestions: ApprovalCardQuestion[] = [];
  let sectionHeader = "";
  for (const question of questions) {
    if (question.kind === "header") {
      sectionHeader = question.prompt;
      continue;
    }
    const multi = question.kind === "multi_select";
    const hints = [
      sectionHeader,
      question.required ? "" : t("conversation.userInput.optional"),
      multi ? t("conversation.userInput.multiSelect") : "",
    ].filter(Boolean);
    cardQuestions.push({
      id: question.question_id,
      title: question.prompt,
      description: hints.length ? hints.join(" · ") : undefined,
      options: question.options.map((option) => ({
        value: option.value,
        label: option.recommended ? `${option.label}（${t("conversation.userInput.recommended")}）` : option.label,
      })),
      multiple: multi,
      allowCustom: question.kind === "free_text" || question.allow_free_text || question.kind === "number",
      customPlaceholder: question.placeholder || t("conversation.userInput.placeholder"),
      required: question.required,
      validationError: answers[question.question_id] ? userInputAnswerError(question, answers[question.question_id]) || undefined : undefined,
      autoAdvance: !question.allow_free_text,
    });
    sectionHeader = "";
  }

  const cardAnswers: ApprovalCardAnswers = {};
  for (const [id, entry] of Object.entries(answers)) {
    cardAnswers[id] = { selected: entry.values ?? [], custom: entry.text ?? "" };
  }
  const handleAnswersChange = (next: ApprovalCardAnswers) => {
    const mapped: UserInputAnswerMap = {};
    for (const [id, entry] of Object.entries(next)) {
      mapped[id] = { values: entry.selected, text: entry.custom ?? "" };
    }
    setAnswers(mapped);
  };

  const resolveInput = async (decision: "submit" | "cancel" | "decline") => {
    if (inputBusy || resolvingInputRef.current || (decision === "submit" && !canSubmit)) return;
    const submittedAnswers = answers;
    resolvingInputRef.current = true;
    setResolvingInput(true);
    setInputResolveError("");
    try {
      const confirmed = await onUserInputResolve({
        request_id: requestId, decision,
        ...(decision === "submit" ? { answers: submittedAnswers } : {}),
      });
      if (confirmed === true && threadId) {
        const current = loadUserInputDraft(threadId, requestId);
        if (!current || JSON.stringify(current.answers) === JSON.stringify(submittedAnswers)) {
          clearUserInputDraft(threadId, requestId);
        }
      }
    } catch (error) {
      setInputResolveError(error instanceof Error ? error.message : String(error));
    } finally {
      resolvingInputRef.current = false;
      setResolvingInput(false);
    }
  };
  const submit = () => { void resolveInput("submit"); };
  const handleCancel = () => { void resolveInput("cancel"); };
  const canDecline = Array.isArray(pendingInput.response_actions) && pendingInput.response_actions.includes("decline");
  const handleDecline = () => { void resolveInput("decline"); };

  return (
    <div
      ref={regionRef}
      role="region"
      aria-label="待审批操作"
      className={cn("cx-animate-in flex flex-col gap-3", className)}
    >
      {planSection}
      {approvalQueue}

      <div
        className="w-full"
        data-testid="user-input-card"
        data-request-id={requestId}
        onKeyDown={(event) => {
          if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
            if (event.nativeEvent.isComposing) return;
            event.preventDefault();
            event.stopPropagation();
            submit();
          }
        }}
      >
        <div className="mb-1.5 flex items-center gap-1.5 px-1 text-[12px]">
          <Icon name="messageCircle" size={13} className="shrink-0 text-cx-accent" />
          <span className="shrink-0 font-medium text-cx-accent">{t("conversation.userInput.badge")}</span>
          {title ? <span className="min-w-0 truncate text-cx-fg-3">· {title}</span> : null}
          <span className="ml-auto hidden shrink-0 items-center gap-1.5 text-[12px] text-cx-fg-4 sm:flex">
            <Shortcut keys="mod+enter" tone="subtle" /> 提交
          </span>
        </div>
        <QuestionCard
          key={answerScope}
          title={title || t("conversation.userInput.title")}
          description={cardQuestions.length ? undefined : t("conversation.userInput.title")}
          questions={cardQuestions}
          status={inputBusy ? "submitting" : "pending"}
          answers={cardAnswers}
          onAnswersChange={handleAnswersChange}
          onSubmit={submit}
          onApprove={submit}
          onReject={canDecline ? handleDecline : undefined}
          rejectLabel="明确拒绝"
          onDismiss={handleCancel}
          dismissLabel={t("conversation.userInput.cancel")}
          approveLabel={t("conversation.userInput.submit")}
          submitLabel={t("conversation.userInput.submit")}
          submitDisabled={!canSubmit}
          className="border border-cx-accent-line bg-cx-elevated shadow-cx-md"
        />
        {inputResolveError ? <p role="alert" className="mt-2 whitespace-pre-wrap text-[12px] text-cx-danger">{inputResolveError}</p> : null}
      </div>
    </div>
  );
}
