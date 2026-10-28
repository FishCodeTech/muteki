/** Agent-proposed workbench suggestions (data only; operator adopts). */
import type { StudioRef } from "../model";
import type { StudioSuggestion, SuggestionKind } from "../store";
import { postCommand, querySrc } from "../../src/reviewPrimitives";
import type { RealDomain } from "./domain";

type Api = Record<string, unknown>;
const text = (v: unknown) => (typeof v === "string" ? v : "");
const num = (v: unknown, d = 0) => (Number.isFinite(Number(v)) ? Number(v) : d);
const iso = (v: unknown) => text(v) || "";

export function createSuggestionsDomain(): RealDomain {
  const revisions = new Map<string, number>();
  return {
    id: "suggestions",
    pollMs: 20000,
    owns: ["suggestions", "suggestionScopes", "suggestionEmpty"],
    combines: {},
    async load(taskId, ctx) {
      const rows = await querySrc<{ items?: Api[] }>(ctx.taskId, "src.workbench.suggestions", {});
      const suggestions: Record<string, StudioSuggestion[]> = {};
      for (const row of rows.items ?? []) {
        const turn = text(row.turn_id) || text(row.source_message_id) || "_";
        revisions.set(text(row.suggestion_id), num(row._revision) || 1);
        const kind = text(row.kind) as SuggestionKind;
        const p = (row.proposal ?? {}) as Api;
        const sug = {
          id: text(row.suggestion_id), kind, turnId: turn,
          title: `${kind}`,
          rationale: text(row.rationale),
          proposal: p,
          refs: (row.evidence_refs as StudioRef[]) ?? [],
          createdAt: iso(row.proposed_at), decidedBy: undefined,
        } as unknown as StudioSuggestion;
        (suggestions[turn] ??= []).push(sug);
      }
      return { suggestions, suggestionScopes: {}, suggestionEmpty: {} };
    },
    mutators: {
      async dismissSuggestion(ctx, ...args) {
        const [suggestionId] = args as [string];
        await postCommand(ctx.taskId, "src.workbench.suggestion.resolve", { suggestion_id: suggestionId, decision: "dismissed", base_revision: revisions.get(suggestionId) ?? 0 });
      },
      async adoptSuggestions(ctx, ...args) {
        const [, ids, input] = args as [string, string[], { threadId: string; assignee: string }];
        const state = ctx.state();
        const list = Object.values(state.suggestions).flat().filter((s) => ids.includes(s.id));
        for (const sug of list) {
          const p = (sug as unknown as { proposal: Api }).proposal;
          const kind = (sug as unknown as { kind: string }).kind;
          try {
            if (kind === "classify" && p.finding_id) {
              await postCommand(ctx.taskId, "src.workbench.finding.meta.save", { finding_id: p.finding_id, meta: { category: p.category, severity: p.severity } });
            } else if (kind === "new_finding") {
              await postCommand(ctx.taskId, "src.finding.claim.create", { text: p.text ?? p.title ?? "", status: "unverified", finding_id: "" });
            } else if (kind === "merge" && p.primary_id) {
              await postCommand(ctx.taskId, "src.workbench.finding.merge", { kind: p.object_kind === "clue" ? "clue" : "source", primary_id: p.primary_id, member_ids: p.member_ids ?? [], reason: p.reason ?? "" });
            } else if (kind === "conflict_resolve" && p.asset_id) {
              await postCommand(ctx.taskId, "src.asset.correct", { asset_id: p.asset_id, attributes: { [String(p.field)]: p.chosen_value }, reason: p.basis ?? "采纳建议裁决" });
            } else if (kind === "plan_update") {
              await postCommand(ctx.taskId, "src.research.plan.save", { payload: { question: p.summary ?? "", rationale: p.basis ?? "", gaps: p.gaps ?? [], actions: p.actions ?? [] }, actor_id: ctx.actor });
            } else if (kind === "report_draft" && p.finding_id) {
              await postCommand(ctx.taskId, "src.workbench.report.draft.save", { payload: { finding_id: p.finding_id, template_id: p.template_id ?? "general_src", sections: p.sections ?? {}, title: p.title ?? "" } });
            } else if (kind === "retest_record" && p.report_id) {
              await postCommand(ctx.taskId, "src.retest.request", { report_id: p.report_id, finding_id: p.finding_id ?? "", repair_statement: p.basis ?? "", expected_target_version: p.verdict ?? "", authorization_ref: "workbench", requester: ctx.actor, idempotency_key: `wb-sug-${sug.id}` });
            }
          } catch (error) {
            throw error;
          }
          const rev = revisions.get(sug.id) ?? 0;
          await postCommand(ctx.taskId, "src.workbench.suggestion.resolve", { suggestion_id: sug.id, decision: "accepted", base_revision: rev, applied_ref: String(p.finding_id ?? p.asset_id ?? p.report_id ?? "") });
        }
        void input;
      },
    },
  };
}
