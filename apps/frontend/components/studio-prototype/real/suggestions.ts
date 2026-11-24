import type { ClueCategory, RetestVerdict, StudioRef } from "../model";
import { turnKey, type StudioSuggestion, type SuggestionKind } from "../store";
import { postCommand, querySrc } from "../../src/reviewPrimitives";
import type { RealDomain } from "./domain";

type Api = Record<string, unknown>;
const text = (v: unknown) => (typeof v === "string" ? v : "");
const num = (v: unknown, d = 0) => (Number.isFinite(Number(v)) ? Number(v) : d);
const iso = (v: unknown) => text(v) || "";

const KIND_TITLES: Record<string, string> = {
  classify: "调整分类/定级",
  recategorize: "调整分类/定级",
  new_finding: "记录新发现",
  finding: "记录新发现",
  merge: "归并重复项",
  conflict_resolve: "裁决字段冲突",
  plan_update: "更新研究计划",
  report_draft: "生成报告草稿",
  retest_record: "发起修复复测",
  supplement_evidence: "补抓对照证据",
  intel_match: "情报指纹命中",
};

function normalizeSuggestion(row: Api, turn: string): StudioSuggestion & { proposal: Api } {
  const kind = text(row.kind) as SuggestionKind;
  const p = (row.proposal && typeof row.proposal === "object" ? (row.proposal as Api) : {});
  const displayTitle = text(p.title)
    || text(p.summary)
    || text(p.text)
    || (text(p.cve_id) ? `${text(p.cve_id)} · ${text(p.asset_key) || text(p.asset_id) || text(p.package_name)}` : "")
    || KIND_TITLES[kind]
    || kind;
  return {
    id: text(row.suggestion_id),
    kind,
    title: displayTitle,
    rationale: text(row.rationale),
    clueId: text(p.finding_id) || undefined,
    from: (text(p.from) || undefined) as ClueCategory | undefined,
    to: (text(p.to) || text(p.category) || undefined) as ClueCategory | undefined,
    sourceId: text(p.asset_id) || undefined,
    passageId: text(p.passage_id) || undefined,
    quote: text(p.quote) || text(p.text) || undefined,
    category: (text(p.category) || undefined) as ClueCategory | undefined,
    mergeKind: p.object_kind === "clue" ? "clue" : p.object_kind === "source" ? "source" : undefined,
    primaryId: text(p.primary_id) || undefined,
    memberIds: Array.isArray(p.member_ids) ? p.member_ids.map(String) : undefined,
    conflictId: text(p.conflict_id) || undefined,
    conflictValue: p.chosen_value !== undefined ? String(p.chosen_value) : undefined,
    planFocusQuestion: text(p.summary) || undefined,
    planJudgement: text(p.basis) || undefined,
    reportId: text(p.report_id) || undefined,
    retestVerdict: (text(p.verdict) || undefined) as RetestVerdict | undefined,
    proposal: p,
    refs: (row.evidence_refs as StudioRef[]) ?? [],
    createdAt: iso(row.proposed_at),
    decidedBy: undefined,
  } as unknown as StudioSuggestion & { proposal: Api };
}

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
        const sugId = text(row.suggestion_id);
        revisions.set(sugId, num(row._revision, 1));
        const sug = normalizeSuggestion(row, turn);
        (suggestions[turnKey(ctx.threadId, turn)] ??= []).push(sug);
      }
      return { suggestions, suggestionScopes: {}, suggestionEmpty: {} };
    },
    mutators: {
      async dismissSuggestion(ctx, ...args) {
        const [suggestionId] = args as [string];
        await postCommand(ctx.taskId, "src.workbench.suggestion.resolve", {
          suggestion_id: suggestionId,
          decision: "dismissed",
          base_revision: revisions.get(suggestionId) ?? 1,
        });
      },
      async adoptSuggestions(ctx, ...args) {
        const [, ids] = args as [string, string[], { threadId: string; assignee: string }];
        const state = ctx.state();
        const list = Object.values(state.suggestions).flat().filter((s) => ids.includes(s.id));
        const idMap: Record<string, string> = {};
        let reloadFindings = false;
        let reloadAssets = false;
        let reloadTasks = false;
        let reloadReports = false;
        for (const sug of list) {
          const p = ((sug as unknown as { proposal?: Api }).proposal ?? {}) as Api;
          const kind = sug.kind;
          let appliedRef = text(p.finding_id) || text(p.asset_id) || text(p.report_id);
          if ((kind === "classify" || kind === "recategorize") && p.finding_id) {
            await postCommand(ctx.taskId, "src.workbench.finding.meta.save", {
              finding_id: p.finding_id,
              meta: { ...(p.category ? { category: p.category } : {}), ...(p.severity ? { severity: p.severity } : {}) },
            });
            reloadFindings = true;
          } else if (kind === "new_finding" || kind === "finding") {
            const created = await postCommand<Api>(ctx.taskId, "src.finding.claim.create", {
              text: p.text ?? p.quote ?? p.title ?? sug.title,
              status: "unverified",
              finding_id: text(p.finding_id),
              exchange_refs: Array.isArray(p.exchange_refs) ? p.exchange_refs : [],
              idempotency_key: `wb-sug-find-${sug.id}`,
            });
            appliedRef = text(created.finding_id) || appliedRef;
            reloadFindings = true;
          } else if (kind === "merge" && p.primary_id) {
            await postCommand(ctx.taskId, "src.workbench.finding.merge", {
              kind: p.object_kind === "clue" ? "clue" : "source",
              primary_id: p.primary_id,
              member_ids: p.member_ids ?? [],
              reason: p.reason ?? sug.rationale ?? "",
            });
            reloadFindings = p.object_kind === "clue" || reloadFindings;
            reloadAssets = p.object_kind !== "clue" || reloadAssets;
          } else if (kind === "conflict_resolve" && p.asset_id) {
            await postCommand(ctx.taskId, "src.asset.correct", {
              asset_id: p.asset_id,
              attributes: { [String(p.field)]: p.chosen_value },
              reason: p.basis ?? "采纳建议裁决",
            });
            reloadAssets = true;
          } else if (kind === "plan_update") {
            const saved = await postCommand<Api>(ctx.taskId, "src.research.plan.save", {
              payload: { question: p.summary ?? "", rationale: p.basis ?? "", gaps: p.gaps ?? [], actions: p.actions ?? [] },
              actor_id: ctx.actor,
            });
            appliedRef = text(saved.plan_id) || appliedRef;
            reloadTasks = true;
          } else if (kind === "report_draft" && p.finding_id) {
            const saved = await postCommand<Api>(ctx.taskId, "src.workbench.report.draft.save", {
              payload: { finding_id: p.finding_id, template_id: p.template_id ?? "general_src", sections: p.sections ?? {}, title: p.title ?? "" },
            });
            appliedRef = text(saved.draft_id) || appliedRef;
            reloadReports = true;
          } else if (kind === "retest_record" && p.report_id) {
            const policy = await querySrc<Api>(ctx.taskId, "src.policy.current", {});
            const policyId = text(policy.policy_id) || ctx.taskId;
            const policyVersion = num(policy.version, 1);
            const requested = await postCommand<Api>(ctx.taskId, "src.retest.request", {
              report_id: p.report_id,
              finding_id: p.finding_id ?? "",
              repair_statement: p.basis ?? sug.rationale ?? "采纳复测建议",
              expected_target_version: p.verdict ?? p.expected_target_version ?? "current",
              authorization_ref: `${policyId}:v${policyVersion}`,
              requester: ctx.actor,
              idempotency_key: `wb-sug-${sug.id}`,
            });
            appliedRef = text(requested.retest_id) || appliedRef;
            reloadReports = true;
          } else if (kind === "supplement_evidence") {
            if (p.draft_id && p.variants) {
              const probe = await postCommand<Api>(ctx.taskId, "src.exchange.differential_probe", {
                draft_id: p.draft_id,
                variants: p.variants,
                use_identity_jar: true,
                idempotency_key: `wb-sug-probe-${sug.id}`,
              });
              appliedRef = text(probe.probe_id) || appliedRef;
              reloadFindings = true;
            } else if (p.finding_id) {
              const run = await postCommand<Api>(ctx.taskId, "src.run.ensure", {
                target_url: text(p.target_url) || undefined,
                question: text(p.question) || `补全发现 ${text(p.finding_id)} 缺失对照证据`,
              });
              const runId = text((run.binding as Api | undefined)?.run_id) || text(run.run_id);
              if (runId) {
                await postCommand(ctx.taskId, "src.run.start", { run_id: runId });
                appliedRef = runId;
              }
              reloadTasks = true;
            }
          } else if (kind === "intel_match" && (p.advisory_id || p.cve_id || p.asset_id)) {
            const todo = await postCommand<Api>(ctx.taskId, "src.workbench.todo.save", {
              payload: {
                title: `核验 ${text(p.advisory_id) || text(p.cve_id) || "高危情报"} (${text(p.asset_key) || text(p.asset_id) || text(p.package_name)})`,
                status: "open",
                origin_kind: "ai",
                notes: text(p.summary) || sug.rationale,
              },
            });
            appliedRef = text(todo.todo_id) || appliedRef;
            reloadTasks = true;
          }
          const rev = revisions.get(sug.id) ?? 1;
          await postCommand(ctx.taskId, "src.workbench.suggestion.resolve", {
            suggestion_id: sug.id,
            decision: "accepted",
            base_revision: rev,
            ...(appliedRef ? { applied_ref: appliedRef } : {}),
          });
          if (appliedRef) idMap[sug.id] = appliedRef;
        }
        const reload = (globalThis as unknown as { __mutekiReloadDomains?: () => Promise<void> }).__mutekiReloadDomains;
        if ((reloadFindings || reloadAssets || reloadTasks || reloadReports) && typeof reload === "function") {
          void reload();
        }
        return Object.keys(idMap).length ? { idMap } : undefined;
      },
    },
  };
}
