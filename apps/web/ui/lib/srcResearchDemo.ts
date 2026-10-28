import type { ResearchAsset, ResearchOverlay, ResearchRef } from "./srcResearch";

/**
 * User-triggered progress script for the offline demo task only. Each step is a
 * local overlay on the fixture snapshot: no request, MCP call or Agent turn runs.
 */
export type SrcResearchDemoStep = { label: string; overlay: (at: string) => ResearchOverlay };

const importRef: ResearchRef = { kind: "import", id: "IMPORT-047", label: "IMPORT-047 · 模拟导入" };
const runRef: ResearchRef = { kind: "run", id: "demo-run-047", label: "demo-run-047 · 模拟评估" };
const finding052: ResearchRef = { kind: "finding", id: "F-052", label: "F-052" };
const finding090: ResearchRef = { kind: "finding", id: "F-090", label: "F-090" };

const newAssets = (at: string): ResearchAsset[] => [1285, 1286, 1287].map((value, index) => ({
  id: `A-${value}`,
  key: `api-${value}.example.invalid`,
  name: `API 服务 ${value}（模拟新增）`,
  type: "api",
  revision: 47,
  firstSeenAt: at,
  lastObservedAt: at,
  source: "synthetic-fixture · IMPORT-047",
  owner: "演示资料组",
  environment: "offline-demo",
  tags: ["演示数据", "模拟新增", index === 0 ? "待关联" : "api"],
  findingIds: [],
  origin: "simulated",
}));
const assetRefs: ResearchRef[] = [1285, 1286, 1287].map(value => ({ kind: "asset", id: `A-${value}`, label: `A-${value}` }));

export const SRC_RESEARCH_DEMO_STEPS: SrcResearchDemoStep[] = [
  {
    label: "开始模拟导入 IMPORT-047",
    overlay: at => ({
      jobs: [{ id: "IMPORT-047", kind: "import", title: "资产导入 · asset-snapshot-v47.csv（虚构）", state: "running", done: 0, total: 40, detail: "本地模拟：读取虚构快照，不访问网络", updatedAt: at, refs: [importRef], origin: "simulated" }],
      changes: [{ id: `sim-${at}-import-start`, at, summary: "IMPORT-047 开始解析 40 行虚构资产快照", refs: [importRef], origin: "simulated" }],
    }),
  },
  {
    label: "完成 IMPORT-047 解析",
    overlay: at => ({
      assets: newAssets(at),
      jobs: [{ id: "IMPORT-047", kind: "import", title: "资产导入 · asset-snapshot-v47.csv（虚构）", state: "completed", done: 40, total: 40, counts: { processed: 40, created: 3, updated: 1, skipped: 36, failed: 0 }, detail: "新增 3 项、变更 1 项，原始行保留在导入记录中", updatedAt: at, refs: [importRef, ...assetRefs], origin: "simulated" }],
      changes: [{ id: `sim-${at}-import-done`, at, summary: "IMPORT-047 完成：新增 A-1285、A-1286、A-1287，变更 1 项", refs: [importRef, ...assetRefs], origin: "simulated" }],
    }),
  },
  {
    label: "开始模拟评估 demo-run-047",
    overlay: at => ({
      jobs: [{ id: "demo-run-047", kind: "run", title: "离线评估 · 核对新增 API 资产与 F-052 资料", state: "running", done: 1, total: 3, detail: "第 1/3 步：比对导入资料版本，不发送请求", updatedAt: at, refs: [runRef, assetRefs[0], finding052], origin: "simulated" }],
      changes: [{ id: `sim-${at}-run-start`, at, summary: "demo-run-047 开始比对 A-1285 与 F-052 的导入资料", refs: [runRef, assetRefs[0], finding052], origin: "simulated" }],
    }),
  },
  {
    label: "完成 demo-run-047",
    overlay: at => ({
      findingPatches: {
        "F-052": { addAssetIds: ["A-1285"], stateNote: "模拟：demo-run-047 将 A-1285 关联到此发现" },
        "F-090": { state: "reviewing", stateNote: "模拟：补充 ART-008 引用后由待补证转入复核中" },
      },
      jobs: [{ id: "demo-run-047", kind: "run", title: "离线评估 · 核对新增 API 资产与 F-052 资料", state: "completed", done: 3, total: 3, detail: "F-052 关联 A-1285；F-090 转入复核中", updatedAt: at, refs: [runRef, assetRefs[0], finding052, finding090], origin: "simulated" }],
      changes: [{ id: `sim-${at}-run-done`, at, summary: "demo-run-047 完成：F-052 关联 A-1285；F-090 待补证 → 复核中", refs: [runRef, assetRefs[0], finding052, finding090], origin: "simulated" }],
    }),
  },
];
