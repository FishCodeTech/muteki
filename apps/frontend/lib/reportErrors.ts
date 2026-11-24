import { ReportError } from "@/lib/reports";

export interface ReportErrorInfo {
  titleKey: string;
  hintKey?: string;
  link?: string;
  linkKey?: string;
  detail?: string;
  soft?: boolean;
}

const TABLE: Record<string, { titleKey: string; hintKey?: string; link?: string; linkKey?: string; soft?: boolean }> = {
  "run.followup.busy": { titleKey: "reports.err.busy", soft: true },
  "run.followup.run_still_active": { titleKey: "reports.err.runStillActive", hintKey: "reports.err.runStillActiveHint" },
  "run.followup.budget_exhausted": { titleKey: "reports.err.budget", hintKey: "reports.err.budgetHint" },
  "run.followup.planner_not_configured": {
    titleKey: "reports.err.planner",
    hintKey: "reports.err.plannerHint",
    link: "/settings/agents",
    linkKey: "reports.err.openSettings",
  },
  "run.followup.empty_response": { titleKey: "reports.err.emptyResponse", hintKey: "reports.err.retryHint" },
  "run.followup.server_restarted": { titleKey: "reports.err.restarted", hintKey: "reports.err.retryHint" },
  "run.followup.provider_failed": { titleKey: "reports.err.provider", hintKey: "reports.err.retryHint" },
  "run.followup.job_missing": { titleKey: "reports.err.jobMissing", hintKey: "reports.err.retryHint" },
  "run.followup.writeup_not_found": { titleKey: "reports.err.noWriteup" },
  "run.followup.followup_not_found": { titleKey: "reports.err.jobMissing" },
  "run.followup.followup_not_cancellable": { titleKey: "reports.err.jobMissing" },
  "run.followup.request_conflict": { titleKey: "reports.err.busy", soft: true },
  "run.followup.run_not_found": { titleKey: "reports.err.runMissing" },
  "run.report.not_supported": { titleKey: "reports.err.notSupported" },
  "run.report.revision_conflict": { titleKey: "reports.conflict" },
  "run.report.folder_not_found": { titleKey: "reports.err.folderMissing" },
  "run.report.folder_depth_exceeded": { titleKey: "reports.err.folderDepth" },
  "run.report.duplicate_import": { titleKey: "reports.err.duplicateImport" },
};

export function reportErrorInfo(error: unknown): ReportErrorInfo | null {
  const code =
    error instanceof ReportError
      ? error.code
      : typeof (error as { code?: unknown })?.code === "string"
        ? (error as { code: string }).code
        : "";
  const message =
    error instanceof Error
      ? error.message
      : typeof (error as { message?: unknown })?.message === "string"
        ? (error as { message: string }).message
        : String(error ?? "");
  const hit = TABLE[code];
  if (hit) {
    return {
      ...hit,
      detail: code === "run.followup.provider_failed" ? message : undefined,
    };
  }
  return null;
}

export function reportErrorText(t: (key: string) => string, error: unknown): string {
  const info = reportErrorInfo(error);
  if (info) return t(info.titleKey);
  return error instanceof Error ? error.message : String(error);
}
