import {
  compressIdle,
  finalizeScenario,
  type EventKind,
  type SpanKind,
  type ToolKind,
  type TraceEventSpec,
  type TraceScenario,
  type TraceSpanSpec,
} from "@/components/activity-timeline/traceModel";

type SpanInput = Omit<TraceSpanSpec, "id" | "parentId"> & { body?: string };

class ScenarioBuilder {
  spans: TraceSpanSpec[] = [];
  events: TraceEventSpec[] = [];
  private seq = 0;

  span(parentId: string | null, input: SpanInput): string {
    const id = `s${++this.seq}`;
    const { body, ...spec } = input;
    this.spans.push({ ...spec, id, parentId });
    if (spec.kind === "llm" || spec.kind === "tool") {
      this.event(id, spec.start, "call", spec.detail ? `${spec.name} ${spec.detail}` : spec.name, body ?? spec.output);
    }
    return id;
  }

  event(spanId: string, ts: number, kind: EventKind, text: string, body?: string) {
    this.events.push({ id: `e${++this.seq}`, spanId, ts, kind, text, body });
  }

  tool(parentId: string, tool: ToolKind, name: string, detail: string, start: number, end: number, extra: Partial<SpanInput> = {}) {
    return this.span(parentId, { kind: "tool", tool, name, detail, start, end, cost: 0, ...extra });
  }

  llm(parentId: string, name: string, model: string, start: number, end: number, tokensIn: number, tokensOut: number, extra: Partial<SpanInput> = {}) {
    const cost = +(tokensIn * 0.000003 + tokensOut * 0.000015).toFixed(4);
    return this.span(parentId, { kind: "llm", name, detail: model, start, end, tokensIn, tokensOut, cost, ...extra });
  }
}

export function standardScenario(): TraceScenario {
  const b = new ScenarioBuilder();
  const task = "为结算页增加优惠码功能，并补齐测试";
  const root = b.span(null, {
    kind: "agent", name: "协调器", engine: "codex", detail: "GPT-6 Luna", start: 0, end: 96_000,
    input: task, output: "优惠码校验、金额下限保护和 38 个测试已完成。",
  });
  b.event(root, 0, "message", `收到任务：${task}`);
  b.llm(root, "拆解任务", "GPT-6 Luna", 600, 7_400, 4_200, 900, {
    output: "拆成 3 个并行子任务：梳理现有结算流程、实现优惠码校验、查阅支付网关限制。",
  });
  b.event(root, 7_400, "reasoning", "拆成 3 个子任务并行推进", "1. 梳理现有结算流程（Claude Code）\n2. 实现优惠码校验（Codex）\n3. 查阅支付网关限制（Kimi）");

  const w1 = b.span(root, {
    kind: "worker", name: "梳理结算流程", engine: "claude", detail: "Claude Code · Opus", start: 8_000, end: 41_000,
    input: "找出折扣在结算链路中的计算位置和测试覆盖情况。", output: "调用链与风险点已整理。",
  });
  b.event(w1, 8_000, "message", "开始：梳理结算链路中的折扣计算");
  b.tool(w1, "read", "read_file", "checkout/service.ts", 9_000, 10_400, { output: "export async function checkout(cart) {\n  const totals = computeTotals(cart);\n  return gateway.charge(totals.payable);\n}" });
  b.tool(w1, "search", "search_code", "applyDiscount", 10_800, 12_600, { output: "4 处匹配：pricing/rules.ts:41, checkout/totals.ts:18, checkout/totals.ts:57, tests/checkout.spec.ts:112" });
  b.tool(w1, "read", "read_file", "pricing/rules.ts", 13_000, 14_100, { output: "export const applyDiscount = (amount, rule) => …" });
  b.tool(w1, "read", "read_file", "checkout/totals.ts", 14_300, 15_200, { output: "const subtotal = applyDiscount(sum(items), cart.rule);\n…\nconst payable = applyDiscount(subtotal, cart.rule);" });
  b.llm(w1, "梳理调用链", "Opus", 15_600, 29_000, 18_400, 2_100, { output: "折扣在 totals.ts 中被计算了两次。" });
  b.event(w1, 29_000, "finding", "折扣在 totals.ts 与 rules.ts 中各算一次，存在重复扣减风险", "checkout/totals.ts:18 与 :57 都调用了 applyDiscount，第二次的输入已经是折后金额。");
  b.tool(w1, "read", "read_file", "tests/checkout.spec.ts", 29_800, 31_000, { output: "describe('checkout', () => { … 36 cases … })" });
  b.llm(w1, "整理结论", "Opus", 31_400, 40_200, 9_600, 1_400, { output: "建议在 totals.ts 只保留一次折扣计算。" });
  b.event(w1, 41_000, "result", "调用链与风险点已整理，交给“实现优惠码校验”");

  const w2 = b.span(root, {
    kind: "worker", name: "实现优惠码校验", engine: "codex", detail: "Codex · GPT-6 Sol", start: 9_000, end: 88_000,
    input: "实现优惠码校验与金额计算，并让测试全部通过。", output: "实现完成，测试 38/38 通过。",
  });
  b.event(w2, 9_000, "message", "开始：实现优惠码校验");
  b.llm(w2, "设计数据结构", "GPT-6 Sol", 9_600, 18_800, 7_800, 1_900, { output: "Coupon { code, kind, value, expiresAt, minSpend }" });
  const edit1 = b.tool(w2, "edit", "edit_file", "coupon/validate.ts", 19_200, 22_600, { output: "+48 −0" });
  b.event(edit1, 19_300, "approval", "等待批准：写入 coupon/validate.ts");
  b.event(edit1, 20_400, "approval", "已批准写入 coupon/validate.ts");
  b.tool(w2, "edit", "edit_file", "checkout/totals.ts", 23_000, 25_400, { output: "+6 −9" });
  const test1 = b.tool(w2, "test", "run_tests", "checkout", 26_000, 38_600, {
    failed: true, error: "2 个用例失败：过期优惠码没有被拒绝", output: "36 passed, 2 failed\n✗ rejects expired coupon\n✗ rejects coupon below min spend",
  });
  b.llm(w2, "分析失败原因", "GPT-6 Sol", 39_000, 47_400, 12_300, 1_700, { output: "校验顺序错误：先计算金额再判断有效期。" });
  b.event(w2, 47_400, "reasoning", "校验顺序错误，先判断有效期和门槛再计算金额");
  b.tool(w2, "edit", "edit_file", "coupon/validate.ts", 47_800, 49_600, { output: "+7 −4" });
  b.tool(w2, "test", "run_tests", "checkout", 50_000, 61_800, { retryOf: test1, output: "38 passed" });
  b.tool(w2, "git", "git_commit", "feat: coupon codes", 62_400, 63_600, { output: "[main 4f2c9e1] feat: coupon codes" });
  b.llm(w2, "撰写变更说明", "GPT-6 Sol", 64_000, 76_000, 6_200, 2_600, { output: "新增优惠码校验；修复重复扣减；补充 2 个测试。" });
  b.tool(w2, "shell", "run_command", "pnpm lint", 76_400, 81_800, { output: "✓ 0 problems" });
  b.llm(w2, "自检", "GPT-6 Sol", 82_200, 87_400, 5_100, 700, { output: "变更与需求一致。" });
  b.event(w2, 88_000, "result", "实现完成，测试 38/38 通过");

  const w3 = b.span(root, {
    kind: "worker", name: "查阅支付网关限制", engine: "kimi", detail: "Kimi · K3", start: 8_600, end: 36_000,
    input: "确认支付网关对折扣金额和优惠码数量的限制。", output: "折后金额不低于 0.01，每单一个优惠码。",
  });
  b.event(w3, 8_600, "message", "开始：查阅支付网关文档");
  b.tool(w3, "web", "web_search", "gateway coupon limits", 9_400, 12_800, { output: "6 条结果" });
  b.tool(w3, "web", "open_page", "docs.pay.example/limits", 13_200, 19_800, { output: "Minimum charge amount: 0.01. One promotion per order." });
  const search2 = b.tool(w3, "web", "web_search", "stacked discount rounding", 20_200, 23_200, { failed: true, error: "请求超时（3s）" });
  b.tool(w3, "web", "web_search", "stacked discount rounding", 23_600, 26_600, { retryOf: search2, output: "4 条结果" });
  b.llm(w3, "汇总限制", "K3", 27_000, 35_200, 11_200, 1_300, { output: "金额下限 0.01；单笔订单仅一个优惠码；四舍五入到分。" });
  b.event(w3, 35_200, "finding", "网关要求折后金额不低于 0.01，且每单只能使用一个优惠码");
  b.event(w3, 36_000, "result", "网关限制已确认");

  b.llm(root, "汇总并回复", "GPT-6 Luna", 88_400, 95_200, 9_800, 1_600, { output: "已完成优惠码校验、金额下限保护与 38 个测试。" });
  b.event(root, 96_000, "result", "任务完成：优惠码校验、金额下限保护与 38 个测试");
  return finalizeScenario({ id: "standard", title: "标准运行", task, spans: b.spans, events: b.events });
}

function rng(seed: number) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const LARGE_WORKERS: { name: string; engine: string; model: string }[] = [
  { name: "迁移订单服务", engine: "claude", model: "Claude Code · Opus" },
  { name: "重写库存同步", engine: "codex", model: "Codex · GPT-6 Sol" },
  { name: "补齐接口文档", engine: "kimi", model: "Kimi · K3" },
  { name: "性能基准测试", engine: "cursor", model: "Cursor · Composer" },
  { name: "清理废弃配置", engine: "opencode", model: "OpenCode · Qwen" },
  { name: "前端表单改造", engine: "grok", model: "Grok Build" },
];
const FILES = ["orders/service.ts", "orders/repo.ts", "inventory/sync.ts", "api/openapi.yaml", "bench/run.ts", "config/legacy.ts", "web/forms/Checkout.tsx", "lib/money.ts", "tests/orders.spec.ts", "tests/inventory.spec.ts"];
const TOOL_PICKS: { tool: ToolKind; name: string; detail: (r: () => number) => string; dur: [number, number] }[] = [
  { tool: "read", name: "read_file", detail: (r) => FILES[Math.floor(r() * FILES.length)], dur: [600, 2_000] },
  { tool: "search", name: "search_code", detail: () => "TODO(migrate)", dur: [900, 2_600] },
  { tool: "edit", name: "edit_file", detail: (r) => FILES[Math.floor(r() * FILES.length)], dur: [1_200, 3_600] },
  { tool: "test", name: "run_tests", detail: () => "orders", dur: [6_000, 16_000] },
  { tool: "shell", name: "run_command", detail: () => "pnpm build", dur: [4_000, 12_000] },
  { tool: "web", name: "web_search", detail: () => "postgres advisory lock", dur: [2_000, 5_000] },
  { tool: "git", name: "git_diff", detail: () => "--stat", dur: [400, 1_200] },
];

/** About 6 minutes, 6 workers and two nested helpers: checks density, folding and DOM cost. */
export function largeScenario(options: { id?: string; title?: string; workers?: number; steps?: [number, number]; seed?: number } = {}): TraceScenario {
  const workerCount = options.workers ?? LARGE_WORKERS.length;
  const [minSteps, maxSteps] = options.steps ?? [18, 32];
  const random = rng(options.seed ?? 7);
  const pick = <T,>(list: T[]) => list[Math.floor(random() * list.length)];
  const between = ([lo, hi]: [number, number]) => Math.round(lo + random() * (hi - lo));
  const b = new ScenarioBuilder();
  const task = "把订单与库存模块迁移到新的数据层，并保持接口兼容";
  const root = b.span(null, { kind: "agent", name: "协调器", engine: "codex", detail: "GPT-6 Luna", start: 0, end: 0, input: task });
  b.event(root, 0, "message", `收到任务：${task}`);
  b.llm(root, "拆解任务", "GPT-6 Luna", 500, 9_000, 6_400, 1_500);
  let finish = 0;
  Array.from({ length: workerCount }, (_, index) => {
    const base = LARGE_WORKERS[index % LARGE_WORKERS.length];
    const round = Math.floor(index / LARGE_WORKERS.length);
    return round ? { ...base, name: `${base.name} ${round + 1}` } : base;
  }).forEach((worker, index) => {
    const start = 9_500 + index * 2_200;
    const id = b.span(root, { kind: "worker", name: worker.name, engine: worker.engine, detail: worker.model, start, end: 0, input: worker.name });
    b.event(id, start, "message", `开始：${worker.name}`);
    let t = start + 600;
    const steps = minSteps + Math.floor(random() * (maxSteps - minSteps));
    for (let step = 0; step < steps; step++) {
      if (index < 2 && step === 8) {
        const helperStart = t;
        const helper = b.span(id, { kind: "worker", name: `${worker.name} · 子任务`, engine: "claude", detail: "Claude Code · Sonnet", start: helperStart, end: 0 });
        let h = helperStart + 400;
        for (let k = 0; k < 6; k++) {
          const p = pick(TOOL_PICKS);
          const d = between(p.dur);
          b.tool(helper, p.tool, p.name, p.detail(random), h, h + d, { output: "ok" });
          h += d + 300;
        }
        const end = h + 400;
        b.spans.find((s) => s.id === helper)!.end = end;
        b.event(helper, end, "result", "子任务完成");
        t = end + 500;
        continue;
      }
      if (random() < 0.3) {
        const d = between([3_000, 14_000]);
        const tin = 3_000 + Math.floor(random() * 16_000);
        b.llm(id, pick(["分析现状", "制定修改方案", "复核改动", "总结进展"]), worker.model.split(" · ").pop() ?? "", t, t + d, tin, Math.floor(tin / 8));
        t += d + 300;
        continue;
      }
      const p = pick(TOOL_PICKS);
      const d = between(p.dur);
      const failed = random() < 0.07;
      const detail = p.detail(random);
      const sid = b.tool(id, p.tool, p.name, detail, t, t + d, failed ? { failed: true, error: "退出码 1" } : { output: "ok" });
      t += d + 250;
      if (failed) {
        const d2 = between(p.dur);
        b.tool(id, p.tool, p.name, detail, t, t + d2, { retryOf: sid, output: "ok" });
        t += d2 + 250;
      }
      if (random() < 0.06) b.event(id, t, "finding", `${worker.name}：发现一处需要兼容的旧接口`);
    }
    const end = t + 500;
    b.spans.find((s) => s.id === id)!.end = end;
    b.event(id, end, "result", `${worker.name} 完成`);
    finish = Math.max(finish, end);
  });
  b.llm(root, "汇总并回复", "GPT-6 Luna", finish + 400, finish + 8_400, 12_000, 2_200);
  b.spans.find((s) => s.id === root)!.end = finish + 9_000;
  b.event(root, finish + 9_000, "result", "迁移完成");
  return finalizeScenario({ id: options.id ?? "large", title: options.title ?? "大规模运行", task, spans: b.spans, events: b.events });
}

/** A first attempt that is cancelled, a long idle stretch, then a retry with partly untimed calls. */
export function idleScenario(): TraceScenario {
  const b = new ScenarioBuilder();
  const task = "解析流量包并提交 flag";
  const resume = 520_000;
  const root = b.span(null, { kind: "agent", name: "协调器", engine: "codex", detail: "GPT-6 Luna", start: 0, end: resume + 64_000, input: task });
  b.event(root, 0, "input", `收到任务：${task}`);
  const first = b.span(root, { kind: "worker", name: "首次尝试", engine: "codex", detail: "Codex · GPT-6 Sol", start: 900, end: 14_000, cancelled: true });
  b.tool(first, "shell", "bash", "file capture.pcapng", 1_400, 2_300, { output: "pcapng capture file" });
  b.tool(first, "shell", "bash", "tshark -r capture.pcapng -q -z io,phs", 2_800, 9_600, { output: "…" });
  b.event(first, 14_000, "system", "Worker 已被操作者取消");
  b.event(root, 14_600, "system", "运行暂停：等待操作者处理模型配置");
  b.event(root, resume, "system", "操作者已恢复运行");
  const workers = [
    { name: "协议层分析", engine: "claude", detail: "Claude Code · Opus" },
    { name: "文件还原", engine: "kimi", detail: "Kimi · K3" },
  ];
  workers.forEach((worker, index) => {
    const start = resume + 800 + index * 600;
    const id = b.span(root, { kind: "worker", ...worker, start, end: resume + 60_000 });
    b.event(id, start, "message", `开始：${worker.name}`);
    let t = start + 500;
    for (let step = 0; step < 9; step++) {
      const d = 1_200 + ((step * 1_700 + index * 900) % 4_800);
      // Every third call only recorded when it returned; its start is the previous record.
      b.tool(id, step % 2 ? "read" : "shell", step % 2 ? "read_file" : "bash", step % 2 ? `extract/${step}.bin` : `binwalk -e part${step}`, t, t + d, step % 3 === 2 ? { estimated: true, output: "ok" } : { output: "ok" });
      t += d + 400;
    }
    b.event(id, t, "result", `${worker.name} 完成`);
  });
  b.event(root, resume + 64_000, "result", "已解出 flag");
  const compressed = compressIdle(b.spans, b.events);
  return finalizeScenario({ id: "idle", title: "空闲后重试", task, spans: compressed.spans, events: compressed.events, gaps: compressed.gaps });
}

export const SCENARIOS = [
  { id: "standard", title: "标准运行（3 个 Worker，含失败与重试）", build: standardScenario },
  { id: "large", title: "大规模运行（8 个 Worker，约 6 分钟）", build: () => largeScenario() },
  { id: "huge", title: "超大运行（24 个 Worker，约 2000 个调用）", build: () => largeScenario({ id: "huge", title: "超大运行", workers: 24, steps: [70, 100], seed: 11 }) },
  { id: "idle", title: "空闲后重试（取消、长时间空闲、估算时长）", build: idleScenario },
] as const;

export type ScenarioId = (typeof SCENARIOS)[number]["id"];
export type { SpanKind };
