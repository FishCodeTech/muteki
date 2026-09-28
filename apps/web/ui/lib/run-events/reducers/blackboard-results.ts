/** results BLACKBOARD_DELTA projection; event behavior is unchanged. */
import {
  POC_CAP,
  capPush,
  markTruncated,
  type BlackboardGatedFinding,
  type BlackboardPoc,
} from "../types";
import {
  FLAG_SUBMISSION_REJECTION_I18N,
  gid,
  invalidateFlag,
  isInvalidatedFlag,
  mergeFlags,
  pushChat,
  upsertStatusChat,
} from "../helpers";
import type { FlagConfirmation, PlatformConfirmationStatus } from "../types";

import type { BlackboardReducerContext } from "./blackboard-context";

function confirmationRank(status: PlatformConfirmationStatus): number {
  switch (status) {
    case "internal":
      return 0;
    case "pending":
      return 1;
    case "accepted":
    case "rejected":
      return 2;
    default: {
      const _never: never = status;
      return _never;
    }
  }
}

function findFlagConfirmation(
  rows: FlagConfirmation[],
  patch: Pick<FlagConfirmation, "submissionId" | "candidateId" | "flag">,
): FlagConfirmation | undefined {
  return rows.find((row) =>
    (patch.submissionId && row.submissionId === patch.submissionId)
    || (patch.candidateId && row.candidateId === patch.candidateId)
    || (patch.flag && row.flag === patch.flag));
}

function upsertFlagConfirmation(
  s: { flagConfirmations: FlagConfirmation[] },
  patch: FlagConfirmation,
): void {
  const current = s.flagConfirmations || [];
  const existing = findFlagConfirmation(current, patch);
  if (!existing) {
    s.flagConfirmations = [...current, patch];
    return;
  }
  if (confirmationRank(existing.status) > confirmationRank(patch.status)) {
    s.flagConfirmations = current.map((row) =>
      row.id === existing.id
        ? {
            ...row,
            flag: row.flag || patch.flag,
            submissionId: row.submissionId || patch.submissionId,
            candidateId: row.candidateId || patch.candidateId,
            title: row.title || patch.title,
            source: row.source || patch.source,
            detail: row.detail || patch.detail,
          }
        : row);
    return;
  }
  s.flagConfirmations = current.map((row) =>
    row.id === existing.id
      ? {
          ...row,
          ...patch,
          id: row.id,
          flag: patch.flag || row.flag,
          submissionId: patch.submissionId || row.submissionId,
          candidateId: patch.candidateId || row.candidateId,
          title: patch.title || row.title,
          source: patch.source || row.source,
        }
      : row);
}

export function reduceBlackboardResults({ ev, s, p, bb, actor, tlabel }: BlackboardReducerContext): boolean {
  switch (p.kind) {
        case "poc_saved": {
          const id = p.poc_id ?? gid("poc");
          const existing = bb.pocs.find((x) => x.id === id);
          const row: BlackboardPoc = {
            id,
            name: p.name ?? id,
            entryCommand: p.entry_command ?? "",
            status: p.status ?? "available",
            note: p.note ?? undefined,
            intentId: p.intent_id ?? undefined,
            artifactId: p.artifact_id ?? undefined,
            path: p.path ?? undefined,
            worker: actor || undefined,
            savedTs: existing?.savedTs ?? ev.ts,
            claimedTs: existing?.claimedTs,
            concludedTs: existing?.concludedTs,
          };
          if (existing) {
            bb.pocs = bb.pocs.map((x) => x.id === id ? { ...x, ...row } : x);
          } else {
            const pushed = capPush(bb.pocs, row, POC_CAP);
            bb.pocs = pushed.list;
            if (pushed.truncated) markTruncated(bb, "pocs");
          }
          tlabel(`${actor} saved PoC ${id} (${row.status})`);
          break;
        }
        case "poc_claimed": {
          bb.pocs = bb.pocs.map((x) =>
            x.id === p.poc_id ? { ...x, status: "wip", worker: p.worker ?? actor, claimedTs: ev.ts } : x);
          tlabel(`${p.worker ?? actor} claimed PoC ${p.poc_id}`);
          break;
        }
        case "poc_concluded": {
          bb.pocs = bb.pocs.map((x) =>
            x.id === p.poc_id ? { ...x, status: p.status ?? "spent", note: p.note ?? x.note, concludedTs: ev.ts } : x);
          tlabel(`${actor} concluded PoC ${p.poc_id} → ${p.status ?? "spent"}`);
          break;
        }
        case "flag_submission_decision":
        case "flag_submission_accepted":
        case "flag_submission_rejected": {
          const submissionId = String(p.submission_id ?? "").trim();
          const candidateId = String(p.candidate_id ?? p.candidateId ?? "").trim();
          const accepted = p.kind === "flag_submission_accepted"
            || (p.kind === "flag_submission_decision" && p.accepted === true);
          const code = String(p.code ?? (accepted ? "accepted" : "rejected")).trim();
          const detail = String(p.detail ?? p.reason ?? "").trim().slice(0, 240);
          const rejectionI18nKey = FLAG_SUBMISSION_REJECTION_I18N[code];
          const flagText = String(p.flag ?? "").trim();
          upsertFlagConfirmation(s, {
            id: submissionId || candidateId || `event-${ev.seq}`,
            flag: flagText,
            submissionId: submissionId || undefined,
            candidateId: candidateId || undefined,
            status: accepted ? "accepted" : "rejected",
            code,
            title: String(p.title ?? "").trim() || undefined,
            source: String(p.source ?? actor).trim() || undefined,
            detail,
            actor,
            ts: ev.ts,
          });
          const label = accepted
            ? `Flag submission accepted: ${submissionId || "unknown"} (${code})`
            : `Flag submission rejected: ${submissionId || "unknown"} (${code})`;
          const statusKey = candidateId
            ? `flag-candidate:${candidateId}`
            : `flag-submission:${submissionId || `event-${ev.seq}`}`;
          tlabel(label, statusKey);
          upsertStatusChat(s, statusKey, {
            role: "system",
            kind: "status",
            content: detail ? `${label} — ${detail}` : label,
            ts: ev.ts,
            i18nKey: accepted
              ? "sys.flagSubmissionAccepted"
              : (rejectionI18nKey ?? "sys.flagSubmissionRejected"),
            i18nVars: {
              submissionId: submissionId || "—",
              code,
              detail: rejectionI18nKey ? "" : (detail ? ` · ${detail}` : ""),
            },
          });
          break;
        }
        case "flag_submission_required": {
          const submissionId = String(p.submission_id ?? "").trim();
          const correlationId = submissionId
            || String(p.candidate_id ?? p.observation_id ?? `event-${ev.seq}`);
          const code = String(p.code ?? "submission_required").trim();
          const detail = String(p.detail ?? p.reason ?? "").trim().slice(0, 240);
          const statusKey = submissionId
            ? `flag-submission:${submissionId}`
            : `flag-candidate:${correlationId}`;
          upsertFlagConfirmation(s, {
            id: submissionId || correlationId,
            flag: String(p.flag ?? "").trim(),
            submissionId: submissionId || undefined,
            candidateId: String(p.candidate_id ?? p.candidateId ?? "").trim() || undefined,
            status: "pending",
            code,
            title: String(p.title ?? "").trim() || undefined,
            source: String(p.source ?? actor).trim() || undefined,
            detail,
            actor,
            ts: ev.ts,
          });
          tlabel(`Flag submission required (${code})`, statusKey);
          upsertStatusChat(s, statusKey, {
            role: "system",
            kind: "status",
            content: detail
              ? `Flag submission required (${code}) — ${detail}`
              : `Flag submission required (${code})`,
            ts: ev.ts,
            i18nKey: "sys.flagSubmissionRequired",
            i18nVars: {
              code,
              detail: detail ? ` · ${detail}` : "",
            },
          });
          break;
        }
        case "flag_invalidated": {
          invalidateFlag(s, p.flag);
          if (bb.flag === p.flag) bb.flag = undefined;
          bb.flags = [...s.flags];
          if (bb.flag === undefined) bb.flag = bb.flags[0];
          tlabel(`flag invalidated: ${(p.flag ?? "").slice(0, 40)}`);
          break;
        }
        case "flag_found": {
          if (isInvalidatedFlag(s, p.flag)) {
            tlabel(`${actor} ignored invalidated flag ${p.flag ?? ""}`);
            break;
          }
          bb.flag = p.flag ?? bb.flag;
          // multi-flag: collect on the deck in real time so the UI shows N/total
          // as flags land, not just at run end.
          mergeFlags(s, p.flag);
          bb.flags = [...s.flags]; // mirror the deduped flag set onto the blackboard
          // Attribution keyed by flag text: the first sighting wins; a later
          // duplicate (graph bridge copy) may only fill in a missing intent_id.
          const flagText = String(p.flag ?? "");
          const flagIntent = String(p.intent_id ?? "").trim() || undefined;
          if (flagText) {
            const origin = bb.flagOrigins[flagText];
            bb.flagOrigins = {
              ...bb.flagOrigins,
              [flagText]: origin
                ? { ...origin, intentId: origin.intentId ?? flagIntent }
                : { actor, ts: ev.ts, intentId: flagIntent },
            };
            upsertFlagConfirmation(s, {
              id: `flag:${flagText}`,
              flag: flagText,
              status: "internal",
              title: String(p.title ?? "").trim() || undefined,
              source: String(p.source ?? actor).trim() || undefined,
              actor,
              ts: ev.ts,
            });
          }
          tlabel(`${actor} FLAG ${p.flag ?? ""}`);
          break;
        }
        case "finding_found": {
          const findingClass = String(p.finding_class ?? "generic");
          const resourceId = String(p.resource_id ?? "");
          const identityA = p.identity_a ? String(p.identity_a) : undefined;
          const identityB = p.identity_b ? String(p.identity_b) : undefined;
          const id = [findingClass.toLowerCase(), resourceId, identityA ?? "", identityB ?? ""].join("::");
          const row: BlackboardGatedFinding = {
            id,
            findingClass,
            resourceId,
            identityA,
            identityB,
            title: String(p.title ?? "").trim() || undefined,
            source: String(p.source ?? p.from_fact ?? actor).trim() || undefined,
            actor,
            ts: ev.ts,
          };
          if (!bb.gatedFindings.some((finding) => finding.id === id)) {
            bb.gatedFindings = [...bb.gatedFindings, row].slice(-80);
            const identities = [identityA, identityB].filter(Boolean).join(" ↔ ");
            const summary = [row.title, findingClass.toUpperCase(), resourceId, identities, row.source]
              .filter(Boolean).join(" · ");
            tlabel(`${actor} accepted finding: ${summary}`);
            pushChat(s, {
              role: "system",
              kind: "insight",
              content: `Verified finding accepted — ${summary}`,
              ts: ev.ts,
            });
          }
          break;
        }
        case "finding_rejected": {
          const findingClass = String(p.finding_class ?? "generic");
          tlabel(`${actor} rejected finding ${findingClass}: ${String(p.reason ?? "missing evidence").slice(0, 72)}`);
          break;
        }
        case "goal_complete": {
          // pentest: the engagement goal was judged met (no flag). Mark the run
          // solved-by-goal and stash the rationale for the outcome panel.
          s.solved = true;
          s.outcomeReason = "goal_met";
          const internalWhy = p.why === "model_goal_with_evidence";
          if (p.why && !internalWhy) s.goalWhy = String(p.why);
          tlabel("goal complete");
          pushChat(s, { role: "system", kind: "status",
            content: internalWhy ? "Goal met — supported by evidence" : `Goal met — ${p.why ?? "engagement objective reached"}`,
            ts: ev.ts, i18nKey: internalWhy ? "sys.pentestGoalMet" : "sys.goalMet", i18nVars: { why: String(p.why ?? "") } });
          break;
        }
    default:
      return false;
  }
  return true;
}
