import { apiFetch } from "./useRun";

/** Explicit reserved demo namespace; ordinary SRC requests retain their transport. */
export async function srcFetch(path: string, init?: RequestInit): Promise<Response> {
  if (/^\/api\/runs\/demo-/.test(path) || /[?&]src_task_id=src-demo-/.test(path)) return Response.json({ error: { code: "src.demo_read_only", message: "演示模式不执行真实 Run 操作。" } }, { status: 409 });
  if (/^\/api\/src\/tasks\/src-demo-[^/]+(?:\/|$)/.test(path)) {
    const { srcDemoFetch } = await import("./srcDemoData");
    const response = srcDemoFetch(path, init);
    if (response) return response;
    throw new Error("无效的演示数据路径");
  }
  return apiFetch(path, init);
}
