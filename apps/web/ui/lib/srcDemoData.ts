import type { SrcTask, SrcRun } from "./srcTypes";
import type { ConversationThread, ConversationView } from "./useConversation";

export const SRC_DEMO_TASK_ID = "src-demo-baidu";
export const SRC_DEMO_THREAD_ID = "src-demo-thread-baidu";
export const SRC_DEMO_DISCLAIMER = "演示数据，不代表对 baidu.com 的真实安全结论";
const at = "2026-10-27T10:42:00+08:00";
const digest = "d3b07384d113edec49eaa6238ad5ff00d3b07384d113edec49eaa6238ad5ff00";
export const srcDemoTask: SrcTask = {
  task_id: SRC_DEMO_TASK_ID, title: "baidu.com 长期安全研究", kind: "src.bounty", revision: 46,
  input: { product_mode: "src", company: "baidu.com", goal: `整理 1,284 项虚构资产与 118 条虚构高风险记录。${SRC_DEMO_DISCLAIMER}`, authorization: { targets: ["*.example.invalid（仅离线演示）"], exclusions: ["真实网络请求、主动测试、写入操作"] } },
  binding: { task_id: SRC_DEMO_TASK_ID, thread_id: SRC_DEMO_THREAD_ID, principal_id: "demo-reviewer", revision: 1 },
  run_count: 46, watermarks: [{ source_id: "导入资产快照（演示）", next_seq: 1284, generation: 46, state: "completed", version: "snapshot-v46", observed_at: at, total: 1284 }, { source_id: "发现队列（演示）", next_seq: 118, generation: 18, state: "completed", total: 118, reviewed: 37, reviewing: 52, awaiting_evidence: 29 }],
};
export const srcDemoTasks: SrcTask[] = [srcDemoTask, ...["API 与服务边界研究", "公开资料与变更监控"].map((title, index) => ({ ...srcDemoTask, task_id: `src-demo-${index + 2}`, title, binding: { ...srcDemoTask.binding, task_id: `src-demo-${index + 2}`, thread_id: `src-demo-thread-${index + 2}` } }))];
const families = ["subdomain", "service", "api", "cdn"];
export const srcDemoThreads: ConversationThread[] = srcDemoTasks.map(task => ({ thread_id: task.binding.thread_id, title: task.title, summary: SRC_DEMO_DISCLAIMER, mode: "src", created_at: "2026-09-12T09:00:00+08:00", updated_at: at, state: { status: "idle" } }));
export function srcDemoConversation(taskId: string): ConversationView {
  const thread = srcDemoThreads.find(row => srcDemoTasks.find(task => task.task_id === taskId)?.binding.thread_id === row.thread_id) || srcDemoThreads[0];
  const texts = [
    "汇总长期研究的资产变化和发现队列，按证据完整度安排下一轮人工复核。仅使用导入的演示资料。",
    "已整理第 46 版导入快照。资产库共 1,284 项：486 项子域名、392 项 Web 与服务、247 项 API、159 项 CDN。最近一版新增 18 项、变更 7 项，全部保留来源记录。\n\n研究资料、证据快照与报告在同一工作台持续保留。",
    "优先梳理高风险队列，区分已复核、复核中和待补证。报告里保留来源与授权边界。",
    `118 条虚构高风险记录已分组：严重 12 条、高危 106 条；37 条已复核、52 条复核中、29 条待补证。\n\n### Finding F-052 · ${srcDemoFindings[51].text}\n导入的两份服务说明对边界的描述不同，仍需资料负责人确认。已关联 EX-014 响应快照、ART-008 原文与来源版本 snapshot-v46。\n\n37 条已复核示例已进入报告候选，待负责人确认后冻结。其余 81 条不会混入本次交付。\n\n${SRC_DEMO_DISCLAIMER}`,
  ];
  return { thread, runtime: { adapter_id: "demo", instance_id: "offline-demo", model: "演示数据", access_mode: "read_only" }, state: { status: "idle" }, messages: texts.map((text, index) => ({ message_id: `demo-message-${index + 1}`, turn_id: `demo-turn-${Math.floor(index / 2) + 1}`, role: index % 2 ? "assistant" : "user", text, stream_seq: index + 1, created_at: at })), turns: [1, 2].map((value, index) => ({ turn_id: `demo-turn-${value}`, seq: value, kind: "message", status: "completed", text: texts[index * 2], runtime_snapshot: { adapter_id: "demo", instance_id: "offline-demo", model: "演示数据", effort: "high", access_mode: "read_only" }, created_at: at, completed_at: at })), queue: [], watermark: 4, event_watermark: 46 };
}
export const srcDemoAssets = Array.from({ length: 1284 }, (_, index) => {
  const family = index < 486 ? 0 : index < 878 ? 1 : index < 1125 ? 2 : 3;
  return { asset_id: `A-${String(index + 1).padStart(4, "0")}`, asset_type: families[family], canonical_key: `${families[family]}-${index + 1}.example.invalid`, display_name: `${["子域名", "Web 服务", "API 服务", "CDN 节点"][family]} ${index + 1}`, revision: 46, first_seen_at: "2026-09-12T09:00:00+08:00", last_observed_at: at, profile: { source: "synthetic-fixture", owner: "演示资料组", environment: "offline-demo" }, tags: ["演示数据", families[family]] };
});
const kinds = ["访问边界一致性审阅", "服务策略资料复核", "会话生命周期说明核对", "接口角色模型审阅", "公开配置差异核对", "数据保留策略复核"];
const demoAssetId = (index: number) => `A-${String(index + 1).padStart(4, "0")}`;
// A few same-category findings deliberately share one fictional API asset so grouping suggestions have real overlaps to propose.
const sharedAssetClusters = [[12, 18, 24], [51, 57, 63], [88, 94], [100, 112]];
const findingAssets = (index: number) => {
  const cluster = sharedAssetClusters.findIndex(rows => rows.includes(index));
  const primary = demoAssetId(cluster >= 0 ? 886 + cluster * 3 : (index * 37) % 1284);
  return index % 4 === 0 ? [primary, demoAssetId((index * 37 + 486) % 1284)] : [primary];
};
export const srcDemoFindings = Array.from({ length: 118 }, (_, index) => ({ finding_id: `F-${String(index + 1).padStart(3, "0")}`, text: `${kinds[index % kinds.length]} · ${index + 1}（虚构）`, severity: index < 12 ? "critical" : "high", readiness: index < 37 ? "GREEN" : index < 89 ? "YELLOW" : "RED", state: index < 37 ? "已复核" : index < 89 ? "复核中" : "待补证", source_run_id: `demo-run-0${46 - (index % 6)}`, version: 1, affected_assets: findingAssets(index), target_version: "snapshot-v46" }));
const evidence = [{ exchange_id: "EX-014", label: "离线响应快照（演示）", artifact_id: "ART-008", sha256: digest, task_id: SRC_DEMO_TASK_ID, media_type: "text/plain", size: 128, name: "synthetic-evidence.txt" }];
const record = (kind: string, id: string, payload: Record<string, unknown>) => ({ record_id: id, kind, task_id: SRC_DEMO_TASK_ID, revision: 1, parent_id: "", natural_key: id, generation: 1, created_at: at, updated_at: at, payload });
const claimFor = (findingId: string) => ({ claim_id: `CL-${findingId}`, ...srcDemoFindings.find(item => item.finding_id === findingId), status: "observed", text: srcDemoFindings.find(item => item.finding_id === findingId)?.text || "资料一致性审阅（虚构）", evidence_refs: evidence, limitations: [SRC_DEMO_DISCLAIMER, "仅基于导入的虚构资料，未执行真实请求"], target_version: "snapshot-v46" });
export const srcDemoRuns: SrcRun[] = Array.from({ length: 46 }, (_, index) => 46 - index).map((value, index) => ({ binding: { binding_id: `demo-binding-${value}`, task_id: SRC_DEMO_TASK_ID, thread_id: SRC_DEMO_THREAD_ID, run_id: `demo-run-0${value}`, binding_key: `snapshot-${value}`, binding_revision: 1, product_mode: "src", execution_mode: "pentest", target_contract_ref: "demo-contract", plan_ref: "PLAN-046", authorization_ref: "AUTH-001", source_message_id: "demo-message-1", related_run_ids: [], target_url: "https://archive.example.invalid/docs", contract: { target: "https://archive.example.invalid/docs", question: ["离线资产快照比较", "证据关联复核", "研究资料归档"][index % 3] } }, run: { run_id: `demo-run-0${value}`, state: "completed", generation: 1, task_kind: "src.bounty", created_at: at, updated_at: at, detail: { summary: "离线演示资料整理，未执行网络请求" } } }));

const reports = srcDemoFindings.slice(0, 37).map((finding, index) => ({ report_id: `R-${String(index + 1).padStart(3, "0")}`, finding_id: finding.finding_id, report_seq: 1, source_run_ids: ["demo-run-046"], document: { title: `${finding.text} · 演示报告`, scope: "离线演示资料", mechanism: "导入资料的描述存在差异，需要负责人确认适用范围。", steps: ["打开导入快照", "核对来源日期与引用位置", "由资料负责人确认"], decisive_evidence: evidence, limitations: [SRC_DEMO_DISCLAIMER], remediation: ["补充资料说明并复核版本"] }, review: { readiness: "GREEN" }, frozen: true, created_at: at, report_markdown: `# 演示研究报告\n\n${SRC_DEMO_DISCLAIMER}\n\n不包含真实漏洞或利用步骤。` }));
const researchRecords = [
  record("execution_approval", "APP-001", { approval_id: "APP-001", invocation_id: "demo-invocation-047", input_digest: digest, request_snapshot: { invocation_id: "demo-invocation-047", task_id: SRC_DEMO_TASK_ID, thread_id: SRC_DEMO_THREAD_ID, run_id: "demo-run-046", worker_id: null, tool_name: "file", action: "read", arguments: { path: "/demo/imports/supplemental-notes.txt" }, arguments_hash: digest, resource_kind: "file", resource_facts: { path: { value: "/demo/imports/supplemental-notes.txt", origin: "user", state: "known" } }, identity_ref: "demo-reviewer", identity_revision: 1, cwd: "/demo", environment_id: "offline-demo", environment_generation: 1, policy_version: 1 }, matched_rule_refs: ["offline-read"], policy_version: 1, status: "pending", requested_at: at, expires_at: "2099-12-31T23:59:59Z", decided_by: "", decided_at: null, decision: null, grant_id: null, consumed_at: null, revision: 1 }),
  record("task_grant", "GRANT-001", { grant_id: "GRANT-001", task_id: SRC_DEMO_TASK_ID, rule_refs: ["offline-read"], resource_selector: { path: { prefix: "/demo/imports/" } }, actions: ["read"], identity_constraints: {}, environment_constraints: { environment_id: "offline-demo" }, input_constraints: {}, source_approval_id: "APP-000", created_by: "demo-reviewer", created_at: at, expires_at: "2099-12-31T23:59:59Z", version: 1, policy_digest: digest }),
  record("import_batch", "IMPORT-046", { batch_id: "IMPORT-046", status: "completed", source_kind: "file", source_name: "asset-snapshot-v46.csv（演示）", created_at: at, completeness: "complete", counts: { processed: 1284, created: 18, updated: 7, skipped: 1259, failed: 0 }, artifact: evidence[0], field_mapping: { host: "canonical_key", family: "asset_type" }, preview: [{ host: "api-001.example.invalid", family: "api" }] }),
  record("saved_query", "QUERY-001", { query_id: "QUERY-001", name: "API 服务家族（演示）", expression: "api", revision: 1, version: 1 }),
  record("research_plan", "PLAN-046", { plan_id: "PLAN-046", version: 46, question: "归档 1,284 项资产，复核 118 条演示发现", rationale: SRC_DEMO_DISCLAIMER, known_gaps: ["29 条待补证", "52 条复核中"], proposed_actions: ["核对来源与快照", "完成人工复核", "冻结 37 条已复核演示报告"], evidence_refs: evidence, created_at: at }),
  record("authorization_snapshot", "AUTH-001", { allowed_targets: ["*.example.invalid"], excluded_targets: ["真实网络目标"], conditions: ["仅离线资料处理", "不得执行主动测试或写入"] }),
  ...["asset-snapshot-v46.csv", "scope-and-policy.pdf", "source-index-v12.json"].map((name, index) => record("skill_material", `MAT-${index + 1}`, { name, version: "46", digest, readable: true, referenced_at: at, selected_fragments: [{ location: "第 12–16 行", text: "仅演示导入记录与来源关系", source_ref: evidence[0] }] })),
  record("business_model", "BUS-001", { roles: ["资料负责人", "审阅人"], object_types: ["导入文件", "证据快照", "冻结报告"], constraints: [{ text: "所有记录为虚构演示，不代表真实安全结论", origin: "demo" }], questions: [] }),
];
const exchange = {
  exchange_id: "EX-014", task_id: SRC_DEMO_TASK_ID, revision: 1,
  source_run_id: "demo-run-046", source_kind: "import", actor_ref: "demo-reviewer",
  context_id: "snapshot-v46", identity_revision: 1, browser_generation: 0,
  declared_target: "https://api.example.invalid/docs/security",
  observed_url: "https://api.example.invalid/docs/security", method: "GET",
  started_at: at, finished_at: at,
  request_raw_ref: { artifact_id: "ART-008", sha256: digest, media_type: "message/http", size: 76, task_id: SRC_DEMO_TASK_ID, source_run_id: "demo-run-046", version: 1, original: true, complete: "complete" },
  response_raw_ref: { artifact_id: "ART-009", sha256: digest, media_type: "message/http", size: 148, task_id: SRC_DEMO_TASK_ID, source_run_id: "demo-run-046", version: 1, original: true, complete: "complete" },
  request_headers: [["Accept", "application/json"]], response_headers: [["Content-Type", "text/plain"]],
  status_code: 200, protocol: "HTTP/1.1", execution_state: "completed", capture_state: "complete",
  capture_limitations: [SRC_DEMO_DISCLAIMER], authorization_ref: "AUTH-001",
  tags: ["演示数据", "离线导入"], metadata: { source: "synthetic-fixture" },
};
const policy = { policy_id: "demo-policy", version: 1, product_mode: "src", task_id: SRC_DEMO_TASK_ID, source_refs: ["AUTH-001"], rules: [
  { rule_id: "offline-read", version: 1, enabled: true, effect: "allow", evaluation_point: "filesystem", mode: "enforce", resource_kind: "file", selector: { path: { prefix: "/demo/imports/" } }, actions: ["read"], conditions: {}, reason: "仅离线资料读取", source_refs: [], reusable_approval: false },
  { rule_id: "network-deny", version: 1, enabled: true, effect: "deny", evaluation_point: "connection", mode: "enforce", resource_kind: "network", selector: {}, actions: ["connect"], conditions: {}, reason: "演示数据不允许真实网络访问", source_refs: [], reusable_approval: false },
], defaults: { effect: "ask" }, authorized_targets: ["*.example.invalid"], excluded_targets: ["所有真实网络目标"], environment_requirements: {}, network_mode: "declared", inspection_requirements: { mode: "record" }, created_by: "demo-reviewer", created_at: at, digest, activation: { state: "demo" } };

function page(items: unknown[], payload: Record<string, unknown>) {
  const start = Number(payload.cursor || 0); const limit = Math.max(1, Math.min(Number(payload.limit || 25), 1000));
  return { items: items.slice(start, start + limit), total: items.length, returned: Math.min(limit, Math.max(0, items.length - start)), next_cursor: start + limit < items.length ? String(start + limit) : null, complete: true, snapshot: "demo-v46", watermarks: srcDemoTask.watermarks };
}
const allRecords = () => [...researchRecords, ...srcDemoFindings.map(finding => record("finding_claim", `CL-${finding.finding_id}`, { ...claimFor(finding.finding_id), finding_id: finding.finding_id })), ...srcDemoAssets.map(asset => record("asset", asset.asset_id, asset)), record("artifact", "ART-008", { artifact_id: "ART-008", sha256: digest, media_type: "text/plain", name: "offline-response.txt", size: 128, source_run_id: "demo-run-046" })];
function query(type: string, payload: Record<string, unknown>): unknown {
  const findingId = String(payload.finding_id || "F-001");
  if (type === "src.task.get") return srcDemoTask;
  if (type === "src.record.list") { const rows = allRecords().filter(row => !payload.kind || row.kind === payload.kind); const result = page(rows, payload); return { ...result, records: result.items }; }
  if (type === "src.record.get") return allRecords().find(row => row.record_id === payload.record_id) || null;
  if (type === "src.research.records") return researchRecords.filter(row => !payload.kind || row.kind === payload.kind);
  if (type === "src.knowledge.search") return [{ case_id: "CASE-001", title: "资料版本差异复核（演示）", problem_pattern: "来源版本不同", prerequisites: ["具备导入资料"], repair_boundary: "补充文档说明", decisive_evidence_refs: evidence }];
  if (type === "src.finding.claims") return [claimFor(findingId)];
  if (type === "src.finding.reviews") return [{ review_id: `REVIEW-${findingId}`, validation_type: "artifact_review", readiness: srcDemoFindings.find(row => row.finding_id === findingId)?.readiness || "YELLOW", reasons: ["演示资料关联状态"], reviewer: "demo-reviewer", created_at: at, evidence_snapshot: evidence, missing: ["不代表真实目标的验证结果"] }];
  if (type === "src.finding.status") return { state: "demo", review: { readiness: srcDemoFindings.find(row => row.finding_id === findingId)?.readiness || "YELLOW", cvss: { vector: "演示评级", severity: srcDemoFindings.find(row => row.finding_id === findingId)?.severity } } };
  if (type === "src.finding.deduplications") return [{ verdict: "undetermined", reason: "虚构示例，未与真实漏洞库比较", candidates: [], complete: true }];
  if (type === "src.report.list") return reports.filter(row => !payload.finding_id || row.finding_id === payload.finding_id);
  if (type === "src.report.get") return reports.find(row => row.report_id === payload.report_id) || reports[0];
  if (type === "src.report.templates") return [{ template_id: "demo-markdown", platform: "offline", template_version: "1", mapping_version: "1", field_constraints: {} }];
  if (type === "src.feedback.platform_status") return { status: "not_submitted", user_recorded_unverified: true, recorded_at: at };
  if (["src.feedback.history", "src.retest.recover"].includes(type)) return [];
  if (type === "src.feedback.diff") return { changes: [], feedback: [] };
  if (type === "src.asset.query") { const term = String(payload.query || payload.expression || "").toLowerCase(); return page(srcDemoAssets.filter(asset => !term || JSON.stringify(asset).toLowerCase().includes(term)), payload); }
  if (type === "src.asset.detail") { const asset = srcDemoAssets.find(row => row.asset_id === payload.asset_id) || srcDemoAssets[0]; return { asset, observations: [], relations: [], watermarks: srcDemoTask.watermarks }; }
  if (type === "src.asset.pivot") return { items: [], relations: [], total: 0, complete: true };
  if (type === "src.asset.export") {
    if (payload.export_format === "zip") throw new Error("演示资产提供 CSV 与 JSON 导出，不生成 ZIP 包。");
    const csv = payload.export_format === "csv";
    const text = csv ? ["asset_id,canonical_key,asset_type", ...srcDemoAssets.map(asset => `${asset.asset_id},${asset.canonical_key},${asset.asset_type}`)].join("\n") : JSON.stringify({ disclaimer: SRC_DEMO_DISCLAIMER, assets: srcDemoAssets });
    const data = btoa(Array.from(new TextEncoder().encode(text), byte => String.fromCharCode(byte)).join(""));
    return [{ encoding: "base64", data }, csv ? "text/csv" : "application/json"];
  }
  if (type === "src.exchange.query") return { items: [exchange], total: 1, complete: true };
  if (type === "src.exchange.read") return { exchange, record_revision: 1, request_raw: "GET /docs/security HTTP/1.1\r\nHost: api.example.invalid\r\n\r\n", response_raw: `HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n${SRC_DEMO_DISCLAIMER}` };
  if (type === "src.artifact.read") return { encoding: "base64", data: btoa("Synthetic evidence fixture.\nNot a real security finding about baidu.com.\nsource: synthetic-fixture\nversion: snapshot-v46\n"), media_type: "text/plain", sha256: digest };
  if (type === "src.monitor.list") return [{ monitor_id: "MON-001", name: "导入资料变化监控（演示）", chain_type: "asset", version: 46, revision: 46, query_version: "46", source_refs: ["snapshot-v46"], interval_seconds: 86400, timezone: "Asia/Shanghai", status: "paused", last_success_at: at, budget: { max_items: 1284 }, notification: { mode: "changes_only" } }];
  if (type === "src.monitor.get") return { ...(query("src.monitor.list", {}) as Record<string, unknown>[])[0], cycles: [{ cycle_id: "CYCLE-046", status: "completed", started_at: at, finished_at: at, counts: { added: 18, changed: 7 }, completeness: "complete" }], watermarks: srcDemoTask.watermarks };
  if (type === "src.monitor.changes") return { items: [{ change_id: "CHANGE-046", change_type: "added", asset_id: "A-1284", summary: "新增 18 项、变更 7 项（虚构快照差异）", observed_at: at }], complete: true };
  if (type === "src.monitor.notifications") return { items: [{ notification_id: "NOTICE-046", status: "recorded", message: "演示快照变化记录，未发送通知", created_at: at }], complete: true };
  throw new Error(`演示数据尚未提供此查询：${type}`);
}

/** Only reserved demo task URLs are handled. No network or production data writes. */
export function srcDemoFetch(path: string, init?: RequestInit): Response | null {
  const url = new URL(path, "https://demo.example.invalid");
  const match = url.pathname.match(/^\/api\/src\/tasks\/(src-demo-[^/]+)(?:\/(.*))?$/);
  if (!match) return null;
  const rest = match[2] || "";
  const payload = typeof init?.body === "string" ? JSON.parse(init.body) as Record<string, unknown> : {};
  const respond = (value: unknown, status = 200) => Response.json(value, { status });
  try {
    if (rest.startsWith("queries/")) return respond(query(decodeURIComponent(rest.slice(8)), payload));
    // Commands remain visibly blocked rather than pretending execution succeeded.
    if (rest.startsWith("commands/") || (init?.method && init.method !== "GET")) return respond({ error: { code: "src.demo_read_only", message: "演示数据只读，此操作不会提交到真实工作台。" } }, 409);
    if (!rest) return respond(srcDemoTasks.find(task => task.task_id === match[1]) || srcDemoTask);
    if (rest === "runs") return respond(srcDemoRuns);
    if (rest === "policy") return respond(policy);
    if (rest === "capabilities") return respond({ network_enforced: false, content_enforced: false, state: "demo", tools: { file: { environment_boundary: "仅离线演示导入资料", receipt_support: "demo" } } });
    if (rest === "records") { const rows = allRecords().filter(row => row.kind === url.searchParams.get("kind")); return respond({ records: rows, total: rows.length, returned: rows.length, next_cursor: null, complete: true, snapshot: { event_watermark: 46 }, watermarks: srcDemoTask.watermarks }); }
    if (rest.startsWith("records/")) return respond(allRecords().find(row => row.record_id === decodeURIComponent(rest.split("/")[2])) || null);
    if (["identities", "objects", "protocol-streams", "oast/interactions"].includes(rest)) return respond([]);
    return respond({ error: { code: "src.demo_unavailable", message: `演示数据未提供此资源：${rest}` } }, 404);
  } catch (cause) { return respond({ error: { code: "src.demo_unavailable", message: cause instanceof Error ? cause.message : "演示数据读取失败" } }, 404); }
}
