"use client";

import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { cn } from "../utils/cn";
import { Icon } from "../icons/Icon";
import { Button, IconButton } from "../primitives/Button";
import { Checkbox, Input, RadioGroup } from "../primitives/Field";
import { AgentDisclosure } from "./AgentDisclosure";

export type QuestionCardStatus =
  | "pending"
  | "submitting"
  | "approved"
  | "rejected"
  | "changes-requested"
  | "answered";

export interface QuestionCardOption {
  value: string;
  label: string;
  disabled?: boolean;
}

export interface QuestionCardQuestion {
  id: string;
  title: ReactNode;
  description?: ReactNode;
  options?: QuestionCardOption[];
  multiple?: boolean;
  autoAdvance?: boolean;
  allowCustom?: boolean;
  customPlaceholder?: string;
  required?: boolean;
  validationError?: string;
}

export interface QuestionCardAnswer {
  selected: string[];
  custom?: string;
}

export type QuestionCardAnswers = Record<string, QuestionCardAnswer>;

export interface QuestionCardProps {
  title?: ReactNode;
  description?: ReactNode;
  children?: ReactNode;
  questions?: QuestionCardQuestion[];
  status?: QuestionCardStatus;
  answers?: QuestionCardAnswers;
  defaultAnswers?: QuestionCardAnswers;
  onAnswersChange?: (answers: QuestionCardAnswers) => void;
  step?: number;
  defaultStep?: number;
  onStepChange?: (step: number) => void;
  onSubmit?: (answers: QuestionCardAnswers) => void;
  onApprove?: () => void;
  onReject?: () => void;
  onRequestChanges?: () => void;
  onDismiss?: () => void;
  dismissLabel?: string;
  approveLabel?: ReactNode;
  submitLabel?: ReactNode;
  rejectLabel?: ReactNode;
  submitDisabled?: boolean;
  result?: ReactNode;
  className?: string;
}

const EMPTY_ANSWER: QuestionCardAnswer = { selected: [], custom: "" };

function getStatusLabel(status: QuestionCardStatus) {
  if (status === "submitting") return "提交中";
  if (status === "approved") return "已批准";
  if (status === "rejected") return "已拒绝";
  if (status === "changes-requested") return "已要求修改";
  if (status === "answered") return "已提交回答";
  return "需要你的输入";
}

function getStatusClass(status: QuestionCardStatus) {
  if (status === "approved" || status === "answered") return "text-cx-success";
  if (status === "rejected") return "text-cx-danger";
  if (status === "changes-requested") return "text-cx-warning";
  return "text-cx-fg-3";
}

function getStatusBadgeClass(status: QuestionCardStatus) {
  if (status === "pending" || status === "changes-requested") {
    return "border-cx-warning/30 bg-cx-warning-soft text-cx-warning";
  }
  if (status === "submitting") return "border-cx-accent/30 bg-cx-accent-soft text-cx-accent";
  if (status === "approved" || status === "answered") {
    return "border-cx-success/30 bg-cx-success-soft text-cx-success";
  }
  return "border-cx-danger/30 bg-cx-danger-soft text-cx-danger";
}

function isAnswered(answer: QuestionCardAnswer) {
  return answer.selected.length > 0 || Boolean(answer.custom?.trim());
}

function QuestionOptions({
  question,
  answer,
  disabled,
  onChange,
  onSingleSelect,
}: {
  question: QuestionCardQuestion;
  answer: QuestionCardAnswer;
  disabled: boolean;
  onChange: (answer: QuestionCardAnswer) => void;
  onSingleSelect?: () => void;
}) {
  const custom = answer.custom ?? "";

  return (
    <div className="mt-3">
      {question.options?.length ? (
        question.multiple ? (
          <div className="grid gap-1">
            {question.options.map((option) => (
              <Checkbox
                key={option.value}
                checked={answer.selected.includes(option.value)}
                disabled={disabled || option.disabled}
                label={option.label}
                onCheckedChange={(checked) =>
                  onChange({
                    ...answer,
                    selected: checked
                      ? [...answer.selected, option.value]
                      : answer.selected.filter((value) => value !== option.value),
                  })
                }
                className="min-h-9 rounded-[8px] px-1.5 py-1 hover:bg-cx-hover/60"
              />
            ))}
          </div>
        ) : (
          <RadioGroup
            value={answer.selected[0] ?? ""}
            onValueChange={(value) => {
              onChange({ selected: [value], custom: question.allowCustom ? custom : "" });
              onSingleSelect?.();
            }}
            options={question.options}
            disabled={disabled}
            className="gap-1"
          />
        )
      ) : null}

      {question.allowCustom ? (
        <div className={cn(question.options?.length && "mt-2")}>
          <Input
            value={custom}
            disabled={disabled}
            placeholder={question.customPlaceholder ?? "补充其他回答…"}
            onChange={(event) =>
              onChange({
                selected: answer.selected,
                custom: event.target.value,
              })
            }
            size="md"
          />
        </div>
      ) : null}
    </div>
  );
}

function ProgressDots({ current, ids }: { current: number; ids: string[] }) {
  return (
    <span className="flex items-center gap-1.5">
      <span className="sr-only">
        第 {current + 1} 题，共 {ids.length} 题
      </span>
      {ids.map((id, index) => (
        <span
          key={id}
          aria-hidden="true"
          className={cn(
            "size-1.5 rounded-full bg-cx-fg transition-[transform,opacity] duration-150 ease-cx-out",
            index === current ? "scale-100" : "scale-75",
            index <= current ? "opacity-100" : "opacity-35",
          )}
        />
      ))}
    </span>
  );
}

export function QuestionCard({
  title = "需要确认",
  description,
  children,
  questions = [],
  status = "pending",
  answers,
  defaultAnswers = {},
  onAnswersChange,
  step,
  defaultStep = 0,
  onStepChange,
  onSubmit,
  onApprove,
  onReject,
  onRequestChanges,
  onDismiss,
  dismissLabel = "关闭",
  approveLabel = "批准",
  submitLabel = "提交回答",
  rejectLabel = "拒绝",
  submitDisabled = false,
  result,
  className,
}: QuestionCardProps) {
  const [internalAnswers, setInternalAnswers] = useState<QuestionCardAnswers>(defaultAnswers);
  const [internalStep, setInternalStep] = useState(defaultStep);
  const autoAdvanceTimer = useRef<number | undefined>(undefined);
  const currentAnswers = answers ?? internalAnswers;
  const currentStep = Math.min(Math.max(0, step ?? internalStep), Math.max(0, questions.length - 1));
  const question = questions[currentStep];
  const questionMode = questions.length > 0;
  const pending = status === "pending";
  const busy = status === "submitting";
  const interactive = pending || busy;
  const currentAnswer = question ? (currentAnswers[question.id] ?? EMPTY_ANSWER) : EMPTY_ANSWER;
  const displayTitle = question?.title ?? title;
  const canContinue = (question?.required === false || isAnswered(currentAnswer)) && !question?.validationError;
  const lastStep = currentStep === questions.length - 1;
  const statusLabel = getStatusLabel(status);

  const clearAutoAdvance = useCallback(() => {
    if (autoAdvanceTimer.current === undefined) return;
    window.clearTimeout(autoAdvanceTimer.current);
    autoAdvanceTimer.current = undefined;
  }, []);

  useEffect(() => clearAutoAdvance, [clearAutoAdvance]);

  const setAnswers = useCallback(
    (next: QuestionCardAnswers) => {
      if (answers === undefined) setInternalAnswers(next);
      onAnswersChange?.(next);
    },
    [answers, onAnswersChange],
  );

  const setStep = (next: number) => {
    clearAutoAdvance();
    if (step === undefined) setInternalStep(next);
    onStepChange?.(next);
  };

  const updateCurrentAnswer = (next: QuestionCardAnswer) => {
    if (!question) return;
    setAnswers({ ...currentAnswers, [question.id]: next });
  };

  const continueQuestion = () => {
    if (currentStep < questions.length - 1) {
      setStep(currentStep + 1);
      return;
    }
    onSubmit?.(currentAnswers);
  };

  const queueAutoAdvance = () => {
    if (
      !question ||
      question.multiple ||
      question.autoAdvance === false ||
      currentStep >= questions.length - 1 ||
      busy
    ) {
      return;
    }
    clearAutoAdvance();
    autoAdvanceTimer.current = window.setTimeout(() => {
      setStep(currentStep + 1);
    }, 110);
  };

  return (
    <div
      data-slot="question-card"
      data-state={status}
      aria-busy={busy || undefined}
      className={cn(
        "ui-surface-ring w-full overflow-hidden rounded-[12px] bg-cx-elevated p-4 text-cx-sm",
        className,
      )}
    >
      <div className="flex items-start gap-3">
        <span aria-hidden="true" className={cn("mt-0.5 grid size-5 shrink-0 place-items-center", getStatusClass(status))}>
          {busy ? (
            <Icon name="loader" size={16} className="cx-spin" />
          ) : interactive ? (
            <Icon name={questionMode ? "help" : "messages"} size={16} />
          ) : status === "rejected" ? (
            <Icon name="x" size={16} />
          ) : (
            <Icon name="check" size={16} />
          )}
        </span>

        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-start gap-3">
            <h3 className="min-w-0 flex-1 whitespace-pre-wrap text-cx-base font-medium leading-6 text-cx-fg [overflow-wrap:anywhere]">
              {displayTitle}
            </h3>
            {questionMode && interactive ? (
              <span className="cx-tabular shrink-0 text-cx-xs text-cx-fg-3/65">
                {currentStep + 1}/{questions.length}
              </span>
            ) : (
              <span
                className={cn(
                  "shrink-0 rounded-full border px-2 py-0.5 text-cx-xs font-medium transition-colors",
                  getStatusBadgeClass(status),
                )}
              >
                {statusLabel}
              </span>
            )}
            {onDismiss ? (
              <IconButton icon="x" size="xs" label={dismissLabel} disabled={busy} onClick={onDismiss} />
            ) : null}
          </div>

          <AgentDisclosure open={interactive}>
            {questionMode && question ? (
              <div key={question.id} className="muteki-swap-enter">
                {question.description ? (
                  <p className="mt-1 whitespace-pre-wrap leading-5 text-cx-fg-3 [overflow-wrap:anywhere]">
                    {question.description}
                  </p>
                ) : null}
                <QuestionOptions
                  question={question}
                  answer={currentAnswer}
                  disabled={busy}
                  onChange={updateCurrentAnswer}
                  onSingleSelect={queueAutoAdvance}
                />
              </div>
            ) : (
              <div>
                {description ? (
                  <p className="mt-1 whitespace-pre-wrap leading-5 text-cx-fg-3 [overflow-wrap:anywhere]">
                    {description}
                  </p>
                ) : null}
                {children ? <div className="mt-3">{children}</div> : null}
              </div>
            )}

            {question?.validationError ? (
              <p role="alert" className="mt-2 text-cx-xs text-cx-danger">
                {question.validationError}
              </p>
            ) : null}

            {questionMode ? (
              <div className="mt-4 flex items-center gap-2">
                {onReject ? (
                  <Button variant="ghost" size="sm" agentConfirm="destructive" disabled={busy} onClick={onReject}>
                    {rejectLabel}
                  </Button>
                ) : null}
                <IconButton
                  icon="arrowLeft"
                  size="sm"
                  label="上一题"
                  disabled={busy || currentStep === 0}
                  onClick={() => setStep(currentStep - 1)}
                />
                <ProgressDots current={currentStep} ids={questions.map((item) => item.id)} />
                <Button
                  size="sm"
                  variant="primary"
                  iconOnly={!lastStep}
                  iconRight={lastStep && !busy ? "arrowRight" : undefined}
                  aria-label={lastStep ? "提交回答" : isAnswered(currentAnswer) ? "下一题" : "跳过"}
                  disabled={busy || !canContinue || (lastStep && submitDisabled)}
                  loading={busy}
                  onClick={continueQuestion}
                  className="ml-auto"
                >
                  {lastStep ? submitLabel : <Icon name="arrowRight" size={15} />}
                </Button>
              </div>
            ) : (
              <div className="mt-4 flex flex-wrap items-center gap-2">
                <Button
                  size="sm"
                  variant="primary"
                  agentConfirm="hitl"
                  disabled={busy}
                  loading={busy}
                  onClick={onApprove}
                >
                  {approveLabel}
                </Button>
                {onRequestChanges ? (
                  <Button size="sm" variant="secondary" disabled={busy} onClick={onRequestChanges}>
                    要求修改
                  </Button>
                ) : null}
                {onReject ? (
                  <Button
                    size="sm"
                    variant="ghost"
                    agentConfirm="destructive"
                    disabled={busy}
                    onClick={onReject}
                  >
                    {rejectLabel}
                  </Button>
                ) : null}
              </div>
            )}
          </AgentDisclosure>

          {!interactive && result ? <div className="mt-2.5 text-cx-xs text-cx-fg-3">{result}</div> : null}
        </div>
      </div>
    </div>
  );
}
