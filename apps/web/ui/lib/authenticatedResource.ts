/**
 * Authenticated GET for resources that browsers cannot open with Bearer headers
 * (window.open / <a href> / <img src>). Fetch via apiFetch, then use a blob
 * object URL — never put long-lived tokens in the URL (#223 / #129).
 */

import { apiFetch } from "@/lib/useRun";
import { workspaceRawUrl } from "@/lib/resourcePreview";

export type AuthenticatedResourceAction = "open" | "download" | "read";

export type AuthenticatedResourceFailure = {
  ok: false;
  reason: "popup_blocked" | "auth" | "http" | "unknown";
  message: string;
};

export type AuthenticatedResourceSuccess = { ok: true };

export type AuthenticatedResourceResult =
  | AuthenticatedResourceSuccess
  | AuthenticatedResourceFailure;

/** How long a blob: URL stays valid after opening in a new tab. */
const BLOB_REVOKE_AFTER_MS = 60_000;

export function authenticatedResourceHttpError(
  status: number,
  action: AuthenticatedResourceAction = "open",
): string {
  if (status === 401) {
    if (action === "download") return "登录已失效，请重新登录后下载文件";
    if (action === "read") return "登录已失效，请重新登录后预览文件";
    return "登录已失效，请重新登录后打开文件";
  }
  if (action === "download") return `下载失败（HTTP ${status}）`;
  if (action === "read") return `读取失败（HTTP ${status}）`;
  return `打开失败（HTTP ${status}）`;
}

export async function fetchWorkspaceRaw(
  threadId: string,
  path: string,
  options?: { download?: boolean; signal?: AbortSignal },
): Promise<Response> {
  return apiFetch(workspaceRawUrl(threadId, path, { download: options?.download }), {
    cache: "no-store",
    signal: options?.signal,
  });
}

function fail(
  reason: AuthenticatedResourceFailure["reason"],
  message: string,
): AuthenticatedResourceFailure {
  return { ok: false, reason, message };
}

/**
 * Reserve a tab synchronously in the click handler, then fetch authenticated bytes.
 * Clear opener before navigation; no-referrer is set on the blank document.
 * Revokes the URL after a delay
 * so the new tab can load; never embeds Bearer in the navigation URL.
 */
export async function openAuthenticatedResource(
  apiPath: string,
  options?: { signal?: AbortSignal },
): Promise<AuthenticatedResourceResult> {
  let opened: Window | null = null;
  let objectUrl: string | null = null;
  try {
    // noopener window.open returns null even on success, so reserve about:blank
    // while user activation is live and sever the opener before any navigation.
    opened = window.open("about:blank", "_blank");
    if (!opened) return fail("popup_blocked", "浏览器拦截了新标签页，请允许弹窗后重试");
    opened.opener = null;
    const referrer = opened.document.createElement("meta");
    referrer.name = "referrer";
    referrer.content = "no-referrer";
    opened.document.head.appendChild(referrer);
    opened.document.title = "正在读取文件…";
    const response = await apiFetch(apiPath, {
      cache: "no-store",
      signal: options?.signal,
    });
    if (!response.ok) {
      opened.close();
      return fail(
        response.status === 401 ? "auth" : "http",
        authenticatedResourceHttpError(response.status, "open"),
      );
    }
    const blob = await response.blob();
    if (opened.closed || options?.signal?.aborted) {
      opened.close();
      return fail("unknown", "已取消");
    }
    objectUrl = URL.createObjectURL(blob);
    opened.location.replace(objectUrl);
    const url = objectUrl;
    window.setTimeout(() => URL.revokeObjectURL(url), BLOB_REVOKE_AFTER_MS);
    return { ok: true };
  } catch (exc) {
    opened?.close();
    if (objectUrl) URL.revokeObjectURL(objectUrl);
    if (options?.signal?.aborted) return fail("unknown", "已取消");
    return fail("unknown", exc instanceof Error ? exc.message : "打开文件失败");
  }
}

/** apiFetch → Blob → temporary <a download>. Revokes immediately after click. */
export async function downloadAuthenticatedResource(
  apiPath: string,
  filename: string,
  options?: { signal?: AbortSignal },
): Promise<AuthenticatedResourceResult> {
  try {
    const response = await apiFetch(apiPath, {
      cache: "no-store",
      signal: options?.signal,
    });
    if (!response.ok) {
      return fail(
        response.status === 401 ? "auth" : "http",
        authenticatedResourceHttpError(response.status, "download"),
      );
    }
    const blob = await response.blob();
    const objectUrl = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = objectUrl;
    anchor.download = filename || "download";
    anchor.rel = "noopener";
    anchor.click();
    window.setTimeout(() => URL.revokeObjectURL(objectUrl), BLOB_REVOKE_AFTER_MS);
    return { ok: true };
  } catch (exc) {
    if (options?.signal?.aborted) {
      return fail("unknown", "已取消");
    }
    return fail(
      "unknown",
      exc instanceof Error ? exc.message : "下载文件失败",
    );
  }
}

export async function openAuthenticatedWorkspaceRaw(
  threadId: string,
  path: string,
  options?: { signal?: AbortSignal },
): Promise<AuthenticatedResourceResult> {
  return openAuthenticatedResource(workspaceRawUrl(threadId, path), options);
}

export async function downloadAuthenticatedWorkspaceRaw(
  threadId: string,
  path: string,
  options?: { signal?: AbortSignal },
): Promise<AuthenticatedResourceResult> {
  const filename = path.split("/").filter(Boolean).pop() || "download";
  return downloadAuthenticatedResource(
    workspaceRawUrl(threadId, path, { download: true }),
    filename,
    options,
  );
}
