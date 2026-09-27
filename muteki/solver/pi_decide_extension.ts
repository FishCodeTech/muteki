import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";

import { Type } from "typebox";

type Priority = "high" | "normal" | "low";
type DraftOperation = Record<string, unknown> & { op: string };

const outputPath = process.env.MUTEKI_DECIDE_DRAFT_PATH || "";
const graphPath = process.env.MUTEKI_DECIDE_GRAPH_PATH || "";
const configuredMaxSteps = Number.parseInt(
  process.env.MUTEKI_DECIDE_MAX_STEPS || "4",
  10,
);
const maxSteps = Math.max(0, Number.isFinite(configuredMaxSteps) ? configuredMaxSteps : 4);
const operations: DraftOperation[] = [];
let committed = false;
let nextDraftId = 0;

function persist() {
  if (!outputPath) {
    throw new Error("MUTEKI_DECIDE_DRAFT_PATH is required");
  }
  writeFileSync(
    outputPath,
    JSON.stringify({ schema: "muteki.ctf-decide-draft.v1", committed, operations }),
    { encoding: "utf8", mode: 0o600 },
  );
}

function text(value: unknown) {
  return {
    content: [{ type: "text", text: String(value) }],
    details: { operationCount: operations.length, committed },
  };
}

function contractError(detail: string) {
  return {
    isError: true,
    content: [{ type: "text", text: `open_step contract error: ${detail}` }],
  };
}

function knownReferences(): { facts: Map<string, number>; resources: Set<string> } {
  if (!graphPath) throw new Error("MUTEKI_DECIDE_GRAPH_PATH is required");
  const graph = JSON.parse(readFileSync(graphPath, "utf8"));
  return {
    facts: new Map(Object.entries(graph.fact_id_map || {}).map(
      ([id, seq]) => [id, Number(seq)],
    )),
    resources: new Set((graph.resource_ids || []).map(String)),
  };
}

function add(operation: DraftOperation) {
  if (committed) {
    throw new Error("draft is already committed");
  }
  operations.push(operation);
  persist();
  return text(`已记入：${operation.op}`);
}

export default function (pi: any) {
  pi.registerTool({
    name: "open_step",
    label: "创建步骤",
    description: "从若干已有事实出发，开一条眼前要取的探索方向。",
    parameters: Type.Object({
      action: Type.String({
        description:
          "一句话点明方向，不写分步方法，不复述 origin 或 goal 已有的信息。方向是要取的信息，不是对同一面换一种手段。已写清复用方法的入口或凭据应列入 from，去取其上的新信息。",
      }),
      from: Type.Array(
        Type.Union([Type.String(), Type.Integer({ minimum: 1 })]),
        { description: "本步骤直接依据的 fact id。" },
      ),
      expected_observable: Type.String({
        description: "本 Step 会产生什么可观察结果，以区分成功、失败或仍未判明。",
      }),
      stop_condition: Type.String({
        description: "何时停止本 Step；必须是有界且可检验的条件。",
      }),
      coverage_key: Type.String({
        description: "本 Step 覆盖的具体问题，同一问题不重复开并行 Step。",
      }),
      requires: Type.Optional(Type.Array(Type.String({
        description: "图中已登记的 PoC Resource ID；不需要时省略。",
      }))),
      priority: Type.Optional(
        Type.Union([
          Type.Literal("high"),
          Type.Literal("normal"),
          Type.Literal("low"),
        ]),
      ),
    }),
    async execute(
      _id: string,
      args: {
        action: string; from: Array<string | number>; priority?: Priority;
        expected_observable: string; stop_condition: string;
        coverage_key: string; requires?: string[];
      },
    ) {
      const count = operations.filter((item) => item.op === "open_step").length;
      if (count >= maxSteps) {
        throw new Error(`本轮最多创建 ${maxSteps} 个步骤`);
      }
      if (![args.action, args.expected_observable, args.stop_condition,
             args.coverage_key].every((value) => typeof value === "string" && value.trim())) {
        return contractError("action, expected_observable, stop_condition, coverage_key must be non-empty");
      }
      let refs;
      try {
        refs = knownReferences();
      } catch (error) {
        return contractError(`graph snapshot unavailable: ${String(error)}`);
      }
      const knownSeqs = new Set(refs.facts.values());
      const unknownFact = args.from.find((value) =>
        typeof value === "number"
          ? !knownSeqs.has(value)
          : !(refs.facts.has(String(value)) || knownSeqs.has(Number(value))));
      if (unknownFact !== undefined) {
        return contractError(`unknown Fact reference: ${String(unknownFact)}`);
      }
      const unknownResource = (args.requires || []).find((id) => !refs.resources.has(id));
      if (unknownResource !== undefined) {
        return contractError(`unknown Resource reference: ${unknownResource}`);
      }
      const draftId = `draft-${++nextDraftId}`;
      add({
        op: "open_step",
        draft_id: draftId,
        action: args.action,
        from: args.from,
        expected_observable: args.expected_observable,
        stop_condition: args.stop_condition,
        coverage_key: args.coverage_key,
        requires: args.requires || [],
        priority: args.priority || "normal",
      });
      return text(`已记入：open_step（${draftId}）`);
    },
  });

  pi.registerTool({
    name: "drop_step",
    label: "删除步骤",
    description:
      "删除已经没有必要的开放步骤；也可取消本轮已记入的 open_step 草稿（传 draft-N）。",
    parameters: Type.Object({
      intent_id: Type.String({
        description: "图中开放 Step 的 intent id，或本轮 open_step 回执中的 draft-N。",
      }),
      why: Type.Optional(Type.String()),
    }),
    async execute(_id: string, args: { intent_id: string; why?: string }) {
      const draftIndex = operations.findIndex(
        (item) => item.op === "open_step" && item.draft_id === args.intent_id,
      );
      if (draftIndex >= 0) {
        operations.splice(draftIndex, 1);
        persist();
        return text(`已取消草稿 ${args.intent_id}`);
      }
      return add({ op: "drop_step", ...args });
    },
  });

  pi.registerTool({
    name: "change_step_priority",
    label: "调整优先级",
    description: "调整一个开放步骤的优先级。",
    parameters: Type.Object({
      intent_id: Type.String(),
      priority: Type.Union([
        Type.Literal("high"),
        Type.Literal("normal"),
        Type.Literal("low"),
      ]),
    }),
    async execute(_id: string, args: { intent_id: string; priority: Priority }) {
      return add({ op: "change_step_priority", ...args });
    },
  });

  pi.registerTool({
    name: "satisfy_goal",
    label: "确认完成",
    description: "图中的事实或平台结果已经满足 Goal 时标记完成。",
    parameters: Type.Object({
      reason: Type.String(),
      from: Type.Optional(
        Type.Array(Type.Union([Type.String(), Type.Integer({ minimum: 1 })])),
      ),
    }),
    async execute(
      _id: string,
      args: { reason: string; from?: Array<string | number> },
    ) {
      return add({ op: "satisfy_goal", ...args });
    },
  });

  pi.registerTool({
    name: "preview",
    label: "预演投影",
    description:
      "复核叠加当前草稿后的图投影：只出 step/goal 结构与事实标题。校验不过的条目会标注；提交时仍会剔除。不写图、不提交。对照的是当时快照，commit 以当时图重验为准。简单批次不必调用。",
    parameters: Type.Object({}),
    async execute() {
      persist();
      const python = process.env.MUTEKI_DECIDE_PYTHON || "python3";
      const result = spawnSync(
        python,
        [
          "-c",
          "from muteki.solver.pi_decide import print_decide_preview; print_decide_preview()",
        ],
        { encoding: "utf8", env: process.env, timeout: 8000 },
      );
      const stdout = String(result.stdout || "").trim();
      if (result.status === 0 && stdout) {
        return text(stdout);
      }
      const err = String(result.stderr || result.error || "").trim();
      return text(`preview 不可用：${err.slice(0, 300) || "python projector failed"}`);
    },
  });

  pi.registerTool({
    name: "commit",
    label: "提交规划",
    description:
      "提交本轮决定。操作简单、合法性一眼可辨时直接提交；含 satisfy_goal 或跨多条事实、可能被剔除的操作时先 preview。",
    parameters: Type.Object({
      summary: Type.Optional(Type.String()),
    }),
    async execute(_id: string, args: { summary?: string }) {
      if (committed) {
        return text("本轮已经提交");
      }
      let graph;
      try {
        graph = JSON.parse(readFileSync(graphPath, "utf8"));
      } catch (error) {
        return contractError(`graph snapshot unavailable: ${String(error)}`);
      }
      const dropped = new Set(operations.filter((item) => item.op === "drop_step")
        .map((item) => String(item.intent_id || "")));
      const existing = (graph.open_steps || []).filter(
        (item: { id?: string }) => !dropped.has(String(item.id || "")),
      );
      const proposed = operations.filter((item) => item.op === "open_step");
      const goalSatisfied = operations.some((item) => item.op === "satisfy_goal");
      if (!goalSatisfied && existing.length === 0 && proposed.length === 0) {
        return contractError(
          "Goal is not satisfied and no Step remains; open an executable Step before commit",
        );
      }
      operations.push({ op: "commit", summary: args.summary || "" });
      committed = true;
      persist();
      return text(`已提交 ${operations.length - 1} 项操作`);
    },
  });
}
