import type { ConversationEvent } from "@/lib/useConversation";

/* Scripted Codex-style turns for the motion lab. Every step is a real
   Conversation event that goes through the production fold, so the lab
   exercises exactly what a live turn renders, without spending model quota. */

export type MockBranch = "approve" | "decline";

export type MockStep =
  | { kind: "event"; delay: number; event: ConversationEvent; branch?: MockBranch }
  | { kind: "approval"; delay: number; toolId: string; command: string; reason: string };

export interface MockScenario {
  id: string;
  title: string;
  description: string;
  prompt: string;
  steps: MockStep[];
}

function seeded(seed: number) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

interface ToolDone {
  status?: string;
  exit_code?: number;
  output?: string;
  error?: string;
  duration_ms?: number;
}

class TurnScript {
  readonly steps: MockStep[] = [];
  private seq = 0;
  private gap = 0;
  private branch: MockBranch | undefined;
  private readonly random: () => number;

  constructor(readonly turnId: string, seed: number) {
    this.random = seeded(seed);
  }

  private push(delay: number, eventType: string, payload: Record<string, unknown>) {
    this.seq += 1;
    this.steps.push({
      kind: "event",
      delay: delay + this.gap,
      event: { event_type: eventType, payload: { turn_id: this.turnId, ...payload }, seq: this.seq },
      ...(this.branch ? { branch: this.branch } : {}),
    });
    this.gap = 0;
    return this;
  }

  /** Extra quiet time before the next event. */
  wait(ms: number) {
    this.gap += ms;
    return this;
  }

  /** Runs `build` only on one side of the scenario's approval. */
  onBranch(branch: MockBranch, build: (script: this) => void) {
    const previous = this.branch;
    this.branch = branch;
    build(this);
    this.branch = previous;
    return this;
  }

  private chunks(text: string, min: number, max: number): string[] {
    const out: string[] = [];
    let at = 0;
    while (at < text.length) {
      const size = Math.max(1, Math.round(min + this.random() * (max - min)));
      out.push(text.slice(at, at + size));
      at += size;
    }
    return out;
  }

  /** Reasoning summary streamed as partial deltas; its bold title narrates the live state. */
  think(itemId: string, text: string, charsPerSecond = 120) {
    for (const chunk of this.chunks(text, 4, 16)) {
      this.push((chunk.length / charsPerSecond) * 1000, "core.reasoning.summary", {
        item_id: itemId,
        reasoning_summary: chunk,
        partial: true,
      });
    }
    return this;
  }

  /**
   * Assistant text in network-like bursts: mostly small chunks, now and then
   * a large one after a stall, the way real deltas arrive.
   */
  say(text: string, phase: "commentary" | "final_answer", messageId: string, charsPerSecond = 70) {
    for (const chunk of this.chunks(text, 2, 18)) {
      const stall = this.random() < 0.08 ? 260 + this.random() * 380 : 0;
      this.push(stall + (chunk.length / charsPerSecond) * 1000 * (0.4 + this.random() * 1.2), "core.message.delta", {
        role: "assistant",
        text: chunk,
        phase,
        message_id: messageId,
      });
    }
    return this;
  }

  toolStart(id: string, name: string, input: Record<string, unknown>, delay = 120, native?: Record<string, unknown>) {
    return this.push(delay, "core.tool.started", {
      tool_call_id: id,
      name,
      input,
      status: "running",
      ...(native ? { native } : {}),
    });
  }

  toolDone(id: string, name: string, delay: number, done: ToolDone = {}) {
    return this.push(delay, "core.tool.completed", { tool_call_id: id, name, ...done });
  }

  /** A shell command Codex parsed into reads, listings and searches. */
  explore(id: string, command: string, actions: Array<Record<string, unknown>>, duration = 420) {
    this.toolStart(id, "shell", { command }, 160, { command_actions: actions });
    return this.toolDone(id, "shell", duration, { exit_code: 0, output: "", duration_ms: duration });
  }

  /** A command whose output streams in line by line, sometimes several at once. */
  run(id: string, command: string, lines: string[], options: { lineDelay?: number; done?: ToolDone; startDelay?: number } = {}) {
    this.toolStart(id, "shell", { command }, options.startDelay ?? 160);
    return this.stream(id, lines, options);
  }

  /** Output of an already started command, then its completion. */
  stream(id: string, lines: string[], options: { lineDelay?: number; done?: ToolDone } = {}) {
    const lineDelay = options.lineDelay ?? 140;
    let at = 0;
    let elapsed = 0;
    while (at < lines.length) {
      const take = this.random() < 0.25 ? 2 + Math.floor(this.random() * 4) : 1;
      const chunk = `${lines.slice(at, at + take).join("\n")}\n`;
      const delay = lineDelay * (0.5 + this.random());
      elapsed += delay;
      this.push(delay, "core.tool.progress", { tool_call_id: id, name: "shell", chunk });
      at += take;
    }
    const done = options.done ?? { exit_code: 0 };
    return this.toolDone(id, "shell", 220, { duration_ms: Math.round(elapsed + 600), ...done });
  }

  edit(id: string, changes: Array<{ path: string; kind?: string; diff: string }>, duration = 520) {
    const input = { changes: changes.map((change) => ({ kind: "update", ...change })) };
    this.toolStart(id, "file_change", input, 180);
    return this.toolDone(id, "file_change", duration, { status: "completed", duration_ms: duration });
  }

  /** A web search: the results land all at once when the call completes. */
  search(id: string, query: string, results: Array<{ title: string; url: string }>, duration = 1400) {
    this.toolStart(id, "web_search", { query }, 160);
    return this.toolDone(id, "web_search", duration, { status: "completed", output: JSON.stringify({ query, results }), duration_ms: duration });
  }

  /** Opening one result page to read it. */
  browse(id: string, url: string, duration = 800) {
    this.toolStart(id, "web_fetch", { url }, 140);
    return this.toolDone(id, "web_fetch", duration, { status: "completed", output: `Fetched ${url}`, duration_ms: duration });
  }

  approval(toolId: string, command: string, reason: string, delay = 300) {
    this.steps.push({ kind: "approval", delay: delay + this.gap, toolId, command, reason });
    this.gap = 0;
    return this;
  }

  compaction(status: "started" | "completed", delay: number, tokens?: { before: number; after?: number }) {
    return this.push(delay, "core.context.compaction", {
      status,
      trigger: "auto",
      ...(tokens ? { tokens_before: tokens.before, tokens_after: tokens.after } : {}),
    });
  }

  finish(kind: "completed" | "failed" | "interrupted", delay = 260, payload: Record<string, unknown> = {}) {
    return this.push(delay, `core.turn.${kind}`, { status: kind, ...payload });
  }
}

const TEST_LINES_FAIL = [
  "> calc@1.0.0 test",
  "> vitest run",
  "",
  " RUN  v2.1.8 /workspace/calc",
  "",
  " ✓ tests/parse.test.ts (12 tests) 18ms",
  " ✓ tests/tokens.test.ts (9 tests) 11ms",
  " ❯ tests/eval.test.ts (14 tests | 2 failed) 24ms",
  "   × divides by zero safely",
  "     → expected Infinity to throw error 'division by zero'",
  "   × respects operator precedence for unary minus",
  "     → expected 9 to be -9",
  "",
  " Test Files  1 failed | 2 passed (3)",
  "      Tests  2 failed | 33 passed (35)",
  "   Duration  612ms",
];

const TEST_LINES_PASS = [
  "> calc@1.0.0 test",
  "> vitest run",
  "",
  " RUN  v2.1.8 /workspace/calc",
  "",
  " ✓ tests/parse.test.ts (12 tests) 17ms",
  " ✓ tests/tokens.test.ts (9 tests) 10ms",
  " ✓ tests/eval.test.ts (14 tests) 21ms",
  "",
  " Test Files  3 passed (3)",
  "      Tests  35 passed (35)",
  "   Duration  588ms",
];

const EVAL_DIFF = [
  "@@ -38,12 +38,18 @@ export function evaluate(node: Node): number {",
  "   switch (node.type) {",
  "     case \"unary\":",
  "-      return evaluate(node.operand);",
  "+      return node.op === \"-\" ? -evaluate(node.operand) : evaluate(node.operand);",
  "     case \"binary\": {",
  "       const left = evaluate(node.left);",
  "       const right = evaluate(node.right);",
  "-      if (node.op === \"/\") return left / right;",
  "+      if (node.op === \"/\") {",
  "+        if (right === 0) throw new EvalError(\"division by zero\");",
  "+        return left / right;",
  "+      }",
  "       return apply(node.op, left, right);",
  "     }",
  "   }",
].join("\n");

const PARSER_DIFF = [
  "@@ -71,7 +71,9 @@ function parseUnary(tokens: TokenStream): Node {",
  "   const token = tokens.peek();",
  "-  if (token.value === \"-\") {",
  "+  if (token.value === \"-\" || token.value === \"+\") {",
  "     tokens.next();",
  "-    return { type: \"unary\", op: \"-\", operand: parseUnary(tokens) };",
  "+    const operand = parseUnary(tokens);",
  "+    return { type: \"unary\", op: token.value, operand };",
  "   }",
  "   return parsePrimary(tokens);",
].join("\n");

const FINAL_ANSWER = `已修复计算器里的两个求值错误，测试全部通过（35/35）。

## 改了什么

1. **一元负号**：\`evaluate\` 之前直接返回操作数，\`-(3*3)\` 得到 \`9\`。现在按运算符取负。
2. **除以零**：\`a / 0\` 之前静默得到 \`Infinity\`，现在抛出 \`EvalError("division by zero")\`，与测试约定一致。
3. **解析器**：\`parseUnary\` 同时接受 \`+\` 与 \`-\`，并把真实运算符写进语法树。

\`\`\`ts
if (node.op === "/") {
  if (right === 0) throw new EvalError("division by zero");
  return left / right;
}
\`\`\`

| 文件 | 变更 |
| --- | --- |
| \`src/eval.ts\` | +7 −2 |
| \`src/parser.ts\` | +3 −2 |

如果希望除以零返回 \`NaN\` 而不是抛错，告诉我即可，改动只在一处。`;

const SEARCH_HEARTS = [
  { title: "Why do octopuses have three hearts? | Natural History Museum", url: "https://www.nhm.ac.uk/discover/why-do-octopuses-have-three-hearts.html" },
  { title: "章魚的三個心臟 | 香港科學館", url: "https://www.museums.gov.hk/zh_TW/web/portal/octopus-hearts.html" },
  { title: "Cephalopod circulation | Anatomical Society", url: "https://www.anatsoc.org.uk/news/cephalopod-circulation" },
  { title: "Why do octopuses have 3 hearts? | Live Science", url: "https://www.livescience.com/animals/octopuses/why-do-octopuses-have-3-hearts" },
  { title: "Octopus facts | SEA LIFE", url: "https://www.visitsealife.com/blog/octopus-facts/" },
  { title: "Cephalopod sentience and welfare | gov.scot", url: "https://www.gov.scot/publications/cephalopod-welfare/" },
  { title: "Octopus | National Wildlife Federation", url: "https://www.nwf.org/Educational-Resources/Wildlife-Guide/Invertebrates/Octopus" },
  { title: "The octopus heart | Smithsonian Ocean", url: "https://ocean.si.edu/ocean-life/invertebrates/octopus-hearts" },
  { title: "Octopus | Britannica", url: "https://www.britannica.com/animal/octopus-mollusk" },
  { title: "Octopus - Wikipedia", url: "https://en.wikipedia.org/wiki/Octopus" },
  { title: "Giant Pacific octopus | Monterey Bay Aquarium", url: "https://www.montereybayaquarium.org/animals/animals-a-to-z/giant-pacific-octopus" },
  { title: "Octopuses: three hearts, blue blood | Smithsonian Magazine", url: "https://www.smithsonianmag.com/science-nature/octopus-three-hearts" },
  { title: "Octopus | National Geographic", url: "https://www.nationalgeographic.com/animals/invertebrates/facts/octopus" },
  { title: "Cephalopods | MBARI", url: "https://www.mbari.org/animal/octopus/" },
  { title: "Octopus facts | NOAA Ocean Service", url: "https://oceanservice.noaa.gov/facts/octopus.html" },
  { title: "Octopus circulation | BBC Science Focus", url: "https://www.sciencefocus.com/nature/why-do-octopuses-have-three-hearts" },
  { title: "Cardiac physiology of cephalopods | Nature", url: "https://www.nature.com/articles/cephalopod-heart" },
];

const SEARCH_BLOOD = [
  { title: "Hemocyanin - Wikipedia", url: "https://en.wikipedia.org/wiki/Hemocyanin" },
  { title: "Oxygen transport by octopus hemocyanin | PMC", url: "https://www.ncbi.nlm.nih.gov/pmc/articles/octopus-hemocyanin/" },
  { title: "Blue blood in the cold | Journal of Experimental Biology", url: "https://journals.biologists.com/jeb/article/octopus-hemocyanin" },
  { title: "Copper-based oxygen carriers | ACS Publications", url: "https://pubs.acs.org/doi/hemocyanin-oxygen-binding" },
  { title: "Octopus blood helps it survive the cold | ScienceDaily", url: "https://www.sciencedaily.com/releases/octopus-blood.htm" },
  { title: "Respiratory pigments of molluscs | Oxford Academic", url: "https://academic.oup.com/icb/article/mollusc-respiratory-pigments" },
];

const SEARCH_SWIM = [
  { title: "Why octopuses prefer crawling | Monterey Bay Aquarium", url: "https://www.montereybayaquarium.org/stories/octopus-crawling" },
  { title: "章鱼游泳时心脏会停跳吗？ - 知乎", url: "https://www.zhihu.com/question/octopus-heart-swimming" },
  { title: "章鱼 - 百度百科", url: "https://baike.baidu.com/item/章鱼" },
  { title: "Octopus locomotion | Smithsonian Ocean", url: "https://ocean.si.edu/ocean-life/invertebrates/octopus-locomotion" },
  { title: "Systemic heart arrest during jet propulsion | Britannica", url: "https://www.britannica.com/science/cephalopod-circulation" },
];

const RESEARCH_ANSWER = `章鱼有三颗心脏，是因为它的血液运氧效率低，身体又很耗氧，一颗心脏推不动整套循环。

## 三颗心各管什么

- **两颗鳃心**：分别长在两侧鳃的根部，只负责把缺氧的血泵过鳃，完成气体交换。
- **一颗体心**：把鳃里充好氧的血送往全身。

## 为什么需要这么多

1. **血是蓝色的**：章鱼靠含铜的血蓝蛋白运氧，携氧能力明显不如人类的血红蛋白，血液也更黏稠，需要更大的压力推动（[Hemocyanin](https://en.wikipedia.org/wiki/Hemocyanin)）。
2. **鳃的阻力大**：血流过鳃里细小的血管时压力损失很大，鳃心相当于在鳃前加了两台增压泵（[Natural History Museum](https://www.nhm.ac.uk/discover/why-do-octopuses-have-three-hearts.html)）。
3. **游泳很费劲**：喷水游泳时体心会暂停跳动，这也是章鱼更爱爬行而不是游泳的原因之一（[Live Science](https://www.livescience.com/animals/octopuses/why-do-octopuses-have-3-hearts)）。

| 心脏 | 数量 | 作用 |
| --- | --- | --- |
| 鳃心 | 2 | 把血泵过鳃 |
| 体心 | 1 | 把含氧血送往全身 |

想知道乌贼、鱿鱼这些头足类是不是也一样，我可以接着查。`;

function researchFlow(): MockScenario {
  const s = new TurnScript("turn-mock-research", 31);
  s.wait(400)
    .think("r1", "**Planning the research**\n\nThe user asks why octopuses have three hearts. Find authoritative sources on cephalopod circulation first.")
    .say("这个问题挺有意思，我先找几篇讲章鱼循环系统的权威资料。", "commentary", "m1")
    .wait(200)
    .search("s1", "章鱼为什么有三颗心脏", SEARCH_HEARTS, 1800)
    .wait(300)
    .think("r2", "**Checking blood chemistry**\n\nSeveral sources mention copper-based blood. Look up hemocyanin to explain why the extra hearts are needed.")
    .search("s2", "octopus hemocyanin copper blood oxygen transport", SEARCH_BLOOD, 1400)
    .wait(300)
    .think("r3", "**Reading the key sources**\n\nOpen the museum article, the Live Science explainer and the hemocyanin entry.")
    .browse("b1", SEARCH_HEARTS[0].url, 900)
    .browse("b2", SEARCH_HEARTS[3].url, 760)
    .browse("b3", SEARCH_BLOOD[0].url, 820)
    .wait(300)
    .say("几篇资料都提到体心在游泳时会停跳，我再确认一下这一点。", "commentary", "m2")
    .search("s3", "章鱼 游泳时 体心脏 停止跳动", SEARCH_SWIM, 1300)
    .wait(400)
    .think("r4", "**Writing the answer**\n\nSources agree. Explain the three hearts, the blue blood and the swimming cost, citing the key pages.")
    .say(RESEARCH_ANSWER, "final_answer", "m3", 90)
    .finish("completed");
  return {
    id: "research",
    title: "联网查资料",
    description: "多次网页搜索 → 来源卡片逐个浮现 → 浏览网页 → 带引用的答案",
    prompt: "章鱼为什么有三颗心脏？帮我查一下资料。",
    steps: s.steps,
  };
}
function fullFlow(): MockScenario {
  const s = new TurnScript("turn-mock-full", 7);
  s.wait(500)
    .think("r1", "**Inspecting the calculator project**\n\nThe user reports wrong results for unary minus and division. I should look at the evaluator, the parser, and the existing tests before changing anything.")
    .wait(200)
    .say("我先看一下项目结构和求值逻辑，确认这两个问题出在哪里。", "commentary", "m1")
    .wait(300)
    .explore("t1", "ls -la src tests", [{ type: "listFiles", path: "src" }], 320)
    .explore("t2", "sed -n '1,120p' src/eval.ts", [{ type: "read", path: "src/eval.ts", name: "eval.ts" }], 380)
    .explore("t3", "sed -n '1,160p' src/parser.ts", [{ type: "read", path: "src/parser.ts", name: "parser.ts" }], 360)
    .explore("t4", "rg -n \"division|unary\" tests", [{ type: "search", query: "division|unary", path: "tests" }], 420)
    .explore("t5", "cat tests/eval.test.ts", [{ type: "read", path: "tests/eval.test.ts", name: "eval.test.ts" }], 300)
    .wait(300)
    .think("r2", "**Reproducing the failures**\n\nThe evaluator ignores the unary operator and never guards division. Running the suite first gives a baseline.")
    .say("找到了：一元运算直接返回操作数，除法也没有处理除数为零。先跑一遍测试拿到基线。", "commentary", "m2")
    .run("t6", "npm test", TEST_LINES_FAIL, { lineDelay: 150, done: { exit_code: 1, error: "Process exited with code 1" } })
    .wait(400)
    .think("r3", "**Fixing evaluate and parseUnary**\n\nTwo failing cases match the diagnosis. Patch the evaluator, then let the parser record the real operator.")
    .say("两个失败用例和判断一致。下面修改求值和解析逻辑。", "commentary", "m3")
    .edit("t7", [{ path: "src/eval.ts", diff: EVAL_DIFF }], 640)
    .edit("t8", [{ path: "src/parser.ts", diff: PARSER_DIFF }], 520)
    .run("t9", "npm test", TEST_LINES_PASS, { lineDelay: 120 })
    .wait(500)
    .think("r4", "**Summarizing the fix**\n\nAll 35 tests pass. Explain the changes briefly with the key snippet.")
    .say(FINAL_ANSWER, "final_answer", "m4", 90)
    .finish("completed");
  return {
    id: "full",
    title: "完整流程",
    description: "思考 → 说明 → 探索 → 运行失败 → 编辑 → 通过 → 流式答案",
    prompt: "计算器的一元负号和除以零都算错了，帮我修一下并跑测试。",
    steps: s.steps,
  };
}

function approvalFlow(): MockScenario {
  const s = new TurnScript("turn-mock-approval", 11);
  s.wait(400)
    .think("r1", "**Checking the deploy script**\n\nThe user wants the staging deploy re-run. The script touches the network, so it needs approval under strict supervision.")
    .say("部署脚本会访问网络，需要你批准后才能执行。", "commentary", "m1")
    .explore("t1", "cat scripts/deploy.sh", [{ type: "read", path: "scripts/deploy.sh", name: "deploy.sh" }], 300)
    .toolStart("t2", "shell", { command: "./scripts/deploy.sh --env staging" }, 260)
    .approval("t2", "./scripts/deploy.sh --env staging", "需要网络访问以推送镜像")
    .onBranch("approve", (b) => {
      b.stream("t2", [
        "→ building image muteki/calc:staging",
        "  step 1/6 : FROM node:22-alpine",
        "  step 2/6 : COPY package*.json ./",
        "  step 3/6 : RUN npm ci",
        "  step 4/6 : COPY . .",
        "  step 5/6 : RUN npm run build",
        "  step 6/6 : CMD [\"node\", \"dist/server.js\"]",
        "→ pushing registry.local/calc:staging",
        "  layer 3f2a… pushed",
        "  layer 91bc… pushed",
        "→ rollout staging: 2/2 pods ready",
      ], { lineDelay: 220 });
      b.wait(300).say("已部署到 staging，两个 Pod 均已就绪。", "final_answer", "m2").finish("completed");
    })
    .onBranch("decline", (b) => {
      b.toolDone("t2", "shell", 120, { status: "declined" });
      b.wait(300).say("好的，没有执行部署。需要时告诉我，我再运行这条命令。", "final_answer", "m2").finish("completed");
    });
  return {
    id: "approval",
    title: "等待批准",
    description: "工作暂停等待批准，批准或拒绝走不同分支",
    prompt: "把 staging 重新部署一次。",
    steps: s.steps,
  };
}

function compactionFlow(): MockScenario {
  const s = new TurnScript("turn-mock-compaction", 23);
  s.wait(300)
    .think("r1", "**Surveying the monorepo**\n\nMany packages to scan; read entry points quickly.")
    .say("这个仓库包比较多，我先快速浏览各包的入口。", "commentary", "m1");
  const files = ["core", "cli", "web", "worker", "shared", "auth", "billing", "search", "notify", "metrics", "storage", "queue", "gateway", "admin"];
  files.forEach((name, index) => {
    s.explore(`t${index}`, `sed -n '1,80p' packages/${name}/src/index.ts`, [{ type: "read", path: `packages/${name}/src/index.ts`, name: `${name}/index.ts` }], 140 + (index % 3) * 60);
  });
  s.explore("tq", "rg -n \"createServer\" packages", [{ type: "search", query: "createServer", path: "packages" }], 260)
    .wait(300)
    .compaction("started", 200)
    .wait(1800)
    .compaction("completed", 0, { before: 182_400, after: 41_900 })
    .wait(300)
    .think("r2", "**Mapping service boundaries**\n\nContext compacted; continue with the gateway wiring.")
    .say("上下文已压缩。网关通过 `createServer` 组装各服务，入口在 `packages/gateway`。", "final_answer", "m2")
    .finish("completed");
  return {
    id: "compaction",
    title: "长探索与压缩",
    description: "连续 15 次读取的列表滑动 + 自动压缩上下文",
    prompt: "梳理一下这个 monorepo 的服务边界。",
    steps: s.steps,
  };
}

function burstFlow(): MockScenario {
  const s = new TurnScript("turn-mock-burst", 41);
  s.wait(200).say("压力测试：大量快速调用和大段输出。", "commentary", "m1");
  for (let index = 0; index < 28; index += 1) {
    const kind = index % 4;
    if (kind === 0) s.explore(`b${index}`, `cat src/mod${index}.ts`, [{ type: "read", path: `src/mod${index}.ts`, name: `mod${index}.ts` }], 60);
    else if (kind === 1) s.explore(`b${index}`, `rg -n token${index}`, [{ type: "search", query: `token${index}`, path: "src" }], 70);
    else if (kind === 2) s.edit(`b${index}`, [{ path: `src/mod${index}.ts`, diff: `@@ -1,2 +1,3 @@\n-const a = ${index};\n+const a = ${index + 1};\n+const b = a * 2;\n export { a };` }], 90);
    else s.run(`b${index}`, `node scripts/gen.js --part ${index}`, Array.from({ length: 30 }, (_, line) => `[gen ${index}] wrote chunk ${line} · ${(line * 37) % 997} bytes`), { lineDelay: 18, startDelay: 40 });
  }
  s.say("完成：28 次调用，含 7 段长输出。", "final_answer", "m2", 140).finish("completed");
  return {
    id: "burst",
    title: "高频突发",
    description: "28 次极快调用 + 长输出洪峰，检查跟随与掉帧",
    prompt: "（压力测试）",
    steps: s.steps,
  };
}

function failureFlow(): MockScenario {
  const s = new TurnScript("turn-mock-failure", 5);
  s.wait(300)
    .think("r1", "**Running the migration**\n\nApply the pending schema migration on the local database.")
    .say("先执行待处理的数据库迁移。", "commentary", "m1")
    .run("t1", "npm run db:migrate", [
      "> prisma migrate deploy",
      "Datasource \"db\": PostgreSQL database \"calc\" at \"localhost:5432\"",
      "2 migrations found in prisma/migrations",
      "Applying migration `20250101_add_history`",
    ], { lineDelay: 260, done: { exit_code: 1, error: "P1001: Can't reach database server at localhost:5432" } })
    .wait(400)
    .say("数据库没有启动，迁移失败。我检查一下服务状态。", "commentary", "m2")
    .run("t2", "docker compose ps db", ["NAME      STATUS", "calc-db   Exited (1) 3 minutes ago"], { lineDelay: 200 })
    .wait(300)
    .finish("failed", 400, { error: "数据库容器已退出，无法继续迁移" });
  return {
    id: "failure",
    title: "失败的回合",
    description: "命令失败 → 诊断 → 回合以失败结束",
    prompt: "跑一下数据库迁移。",
    steps: s.steps,
  };
}

function interruptFlow(): MockScenario {
  const s = new TurnScript("turn-mock-interrupt", 13);
  s.wait(300)
    .think("r1", "**Profiling the build**\n\nCollect timings for each build step.")
    .say("我收集一下构建各阶段的耗时。", "commentary", "m1")
    .run("t1", "npm run build -- --profile", Array.from({ length: 40 }, (_, index) => `  [${String(index + 1).padStart(2, "0")}/80] compiling src/feature-${index}.tsx … ${(index * 13) % 90 + 10}ms`), { lineDelay: 160, done: { status: "cancelled", error: "已中断" } })
    .finish("interrupted", 120);
  return {
    id: "interrupt",
    title: "中途中断",
    description: "长命令运行中被用户中断",
    prompt: "分析一下构建为什么这么慢。",
    steps: s.steps,
  };
}

export const MOCK_SCENARIOS: MockScenario[] = [fullFlow(), researchFlow(), approvalFlow(), compactionFlow(), burstFlow(), failureFlow(), interruptFlow()];
