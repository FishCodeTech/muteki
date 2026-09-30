// Vendored from AgentUI (https://www.agentui.pro), MIT License. See ./LICENSE.
import type { ReactNode } from "react";

export type ApprovalCardStatus =
  | "pending"
  | "submitting"
  | "approved"
  | "rejected"
  | "changes-requested"
  | "answered";

export interface ApprovalCardOption {
  value: string;
  label: string;
  disabled?: boolean;
}

export interface ApprovalCardQuestion {
  id: string;
  title: ReactNode;
  description?: ReactNode;
  options?: ApprovalCardOption[];
  multiple?: boolean;
  autoAdvance?: boolean;
  allowCustom?: boolean;
  customPlaceholder?: string;
  /** Optional questions can be skipped without an answer. Defaults to true. */
  required?: boolean;
  validationError?: string;
}

export interface ApprovalCardAnswer {
  selected: string[];
  custom?: string;
}

export type ApprovalCardAnswers = Record<string, ApprovalCardAnswer>;

export interface ApprovalCardProps {
  title?: ReactNode;
  description?: ReactNode;
  children?: ReactNode;
  questions?: ApprovalCardQuestion[];
  status?: ApprovalCardStatus;
  answers?: ApprovalCardAnswers;
  defaultAnswers?: ApprovalCardAnswers;
  onAnswersChange?: (answers: ApprovalCardAnswers) => void;
  step?: number;
  defaultStep?: number;
  onStepChange?: (step: number) => void;
  onSubmit?: (answers: ApprovalCardAnswers) => void;
  onApprove?: () => void;
  onReject?: () => void;
  onRequestChanges?: () => void;
  onDismiss?: () => void;
  /** Accessible label for the dismiss control. */
  dismissLabel?: string;
  approveLabel?: ReactNode;
  submitLabel?: ReactNode;
  rejectLabel?: ReactNode;
  submitDisabled?: boolean;
  result?: ReactNode;
  className?: string;
}
