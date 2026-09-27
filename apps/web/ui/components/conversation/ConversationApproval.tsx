"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { Shortcut } from "@/components/chat/ui";
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
  decision: "submit" | "cancel";
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
  ) => void;
  onUserInputResolve: (payload: UserInputResolvePayload) => void;
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
  if (!value) return null;
  const label = String(row.label ?? row.title ?? value).trim() || value;
  const recommended = Boolean(row.recommended ?? row.isRecommended ?? row.is_recommended);
  return { value, label, recommended };
}

function normalizeQuestions(pending: Record<string, unknown>): NormalizedQuestion[] {
  const raw = pending.questions;
  if (Array.isArray(raw) && raw.length > 0) {
    return raw
      .map((item, index) => {
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

  const requestId = typeof pendingInput?.request_id === "string"
    ? pendingInput.request_id
    : "";
  const questions = useMemo(
    () => (pendingInput ? normalizeQuestions(pendingInput) : []),
    [pendingInput],
  );
  const title = pendingInput
    ? field(pendingInput, "title", "message") || t("conversation.userInput.title")
    : "";

  useEffect(() => {
    if (!pendingInput || !requestId) {
      setAnswers({});
      return;
    }
    const draft = threadId ? loadUserInputDraft(threadId, requestId) : null;
    setAnswers(draft?.answers ?? {});
  }, [pendingInput, requestId, threadId]);

  useEffect(() => {
    if (!threadId || !requestId) return;
    saveUserInputDraft(threadId, requestId, answers);
  }, [answers, threadId, requestId]);

  const approvalRows = useMemo(
    () => asApprovalRows(pendingApprovals, pendingApproval),
    [pendingApprovals, pendingApproval],
  );
  const pendingApprovalCount = approvalRows.filter(
    (row) => (field(row, "status") || "pending") === "pending",
  ).length;
  const expiredApprovalCount = approvalRows.filter(
    (row) => field(row, "status") === "expired",
  ).length;

  const regionRef = useRef<HTMLDivElement | null>(null);

  // Auto-focus first actionable element when approval/input appears (C34).
  useEffect(() => {
    if (pendingApprovalCount || pendingInput) {
      window.requestAnimationFrame(() => {
        regionRef.current
          ?.querySelector<HTMLElement>("[role=radio], [role=checkbox], input, textarea, button:not(:disabled)")
          ?.focus();
      });
    }
  }, [pendingApprovalCount, pendingInput]);

  if (!approvalRows.length && !pendingInput) return null;

  const approvalQueue = approvalRows.length ? (
      <div className="flex flex-col gap-2.5" data-testid="approval-queue">
        {approvalRows.length > 1 && (
          <div className="flex items-center gap-2 text-[12px] font-medium text-cx-fg-3">
            {pendingApprovalCount > 0 ? (
              <span className="size-1.5 rounded-full bg-cx-warning cx-pulse-dot" />
            ) : null}
            {pendingApprovalCount > 0
              ? `${pendingApprovalCount} 个操作等待审批${expiredApprovalCount ? ` · ${expiredApprovalCount} 个已过期` : ""}`
              : expiredApprovalCount === approvalRows.length
                ? "审批均已过期"
                : "审批记录"}
          </div>
        )}
        {approvalRows.map((row) => {
          const preview = buildApprovalPreview(row);
          const approvalId = preview.approvalId;
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
              reason={preview.reason || undefined}
              status={preview.status}
              busy={busy}
              onAllow={
                preview.status === "expired"
                  ? undefined
                  : (scopeMode) => onApprovalDecision(approvalId, "allow", scopeMode)
              }
              onDeny={
                preview.status === "expired"
                  ? undefined
                  : () => onApprovalDecision(approvalId, "deny")
              }
            />
          );
        })}
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
        {approvalQueue}
      </div>
    );
  }


  const interactive = questions.filter((q) => q.kind !== "header");
  const canSubmit = interactive.every((q) => answerSatisfied(q, answers[q.question_id]));

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

  const submit = () => {
    if (!canSubmit || busy) return;
    onUserInputResolve({
      request_id: requestId,
      decision: "submit",
      answers,
    });
    if (threadId) clearUserInputDraft(threadId, requestId);
  };

  const handleCancel = () => {
    onUserInputResolve({
      request_id: requestId,
      decision: "cancel",
    });
    if (threadId) clearUserInputDraft(threadId, requestId);
  };

  return (
    <div
      ref={regionRef}
      role="region"
      aria-label="待审批操作"
      className={cn("cx-animate-in flex flex-col gap-3", className)}
    >
      {approvalQueue}

      <div
        className="w-full"
        data-testid="user-input-card"
        data-request-id={requestId}
        onKeyDown={(event) => {
          if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
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
          <span className="ml-auto hidden shrink-0 items-center gap-1.5 text-[11.5px] text-cx-fg-4 sm:flex">
            <Shortcut keys="mod+enter" tone="subtle" /> 提交
          </span>
        </div>
        <QuestionCard
          title={title || t("conversation.userInput.title")}
          description={cardQuestions.length ? undefined : t("conversation.userInput.title")}
          questions={cardQuestions}
          status={busy ? "submitting" : "pending"}
          answers={cardAnswers}
          onAnswersChange={handleAnswersChange}
          onSubmit={submit}
          onApprove={submit}
          onReject={handleCancel}
          onDismiss={handleCancel}
          dismissLabel={t("conversation.userInput.cancel")}
          approveLabel={t("conversation.userInput.submit")}
          submitLabel={t("conversation.userInput.submit")}
          className="border border-cx-accent-line bg-cx-elevated shadow-cx-md"
        />
      </div>
    </div>
  );
}
