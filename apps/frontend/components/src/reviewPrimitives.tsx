import { srcFetch as apiFetch } from "@/lib/srcFetch";

export function commandUrl(taskId: string, commandType: string): string {
  return `/api/src/tasks/${encodeURIComponent(taskId)}/commands/${encodeURIComponent(commandType)}`;
}

async function responseError(response: Response, fallback: string): Promise<Error> {
  let message = `${fallback}（HTTP ${response.status}）`;
  try {
    const body = await response.json() as { error?: { message?: string; code?: string } };
    if (body.error?.message) message = body.error.message;
    if (body.error?.code) message = `${message} [${body.error.code}]`;
  } catch {
  }
  return new Error(message);
}

export async function postCommand<T = Record<string, unknown>>(
  taskId: string,
  commandType: string,
  payload: Record<string, unknown>,
): Promise<T> {
  const response = await apiFetch(commandUrl(taskId, commandType), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
  const body = await response.json() as {
    output?: T;
    error?: { message?: string; code?: string } | null;
  };
  if (!response.ok || body.error) {
    throw new Error(body.error?.message || `操作失败（HTTP ${response.status}）`);
  }
  const output = (body.output === undefined ? body : body.output) as T & { result?: unknown };
  return (output && typeof output === "object" && "result" in output ? output.result : output) as T;
}

export function queryUrl(taskId: string, queryType: string): string {
  return `/api/src/tasks/${encodeURIComponent(taskId)}/queries/${encodeURIComponent(queryType)}`;
}

export async function querySrc<T>(taskId: string, queryType: string, payload: Record<string, unknown> = {}): Promise<T> {
  const response = await apiFetch(queryUrl(taskId, queryType), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
  if (!response.ok) throw await responseError(response, "查询失败");
  return await response.json() as T;
}
