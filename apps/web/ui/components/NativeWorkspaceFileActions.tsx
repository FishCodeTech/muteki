"use client";

import { useEffect, useRef, useState } from "react";
import { useLang } from "@/lib/i18n";
import { desktopChatBridge } from "@/lib/desktopChatBridge";
import { nativeCapability, nativeDisplayMessage, openNativeWorkspaceFile, selectNativeWorkspaceRoot, useNativeDesktopState, useNativeWorkspaceGrant, workspaceRelativePath } from "@/lib/nativeDesktop";

export function NativeWorkspaceFileActions({ threadId, workspaceId, serviceRoot, relativePath }: { threadId: string; workspaceId: string; serviceRoot: string; relativePath?: string }) {
  const { lang } = useLang(); const en = lang === "en"; const t = (zh: string, english: string) => en ? english : zh;
  const { desktop, state, error: stateError } = useNativeDesktopState();
  const context = { threadId, workspaceId, serviceRoot };
  const grant = useNativeWorkspaceGrant(state, context);
  const capability = nativeCapability(state, "workspaceFileActions");
  const bridge = desktopChatBridge();
  const available = capability.available && Boolean(bridge?.selectWorkspaceRoot && bridge?.openWorkspaceFile);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [status, setStatus] = useState("");
  const ownerKey = JSON.stringify([threadId, workspaceId, serviceRoot, relativePath, state?.connectionVersion, state?.serviceId, state?.identityId]);
  const currentOwnerRef = useRef(ownerKey);
  currentOwnerRef.current = ownerKey;
  const operationRef = useRef(0);
  useEffect(() => { setError(""); setStatus(""); setBusy(false); }, [threadId, workspaceId, serviceRoot, relativePath, state?.connectionVersion, state?.serviceId, state?.identityId]);
  if (!desktop) return null;
  let pathError = "";
  if (relativePath) { try { workspaceRelativePath(relativePath); } catch (cause) { pathError = nativeDisplayMessage(cause instanceof Error ? cause : { message: String(cause) }, en); } }
  const select = async () => {
    const operation = ++operationRef.current;
    const isCurrent = () => currentOwnerRef.current === ownerKey && operationRef.current === operation;
    setBusy(true); setError(""); setStatus("");
    try {
      const selected = await selectNativeWorkspaceRoot(state, context);
      if (!isCurrent()) return;
      setStatus(selected ? t("已建立手动目录映射；文件同步尚未验证。", "Manual directory mapping selected; file synchronization is unverified.") : t("已取消，本机目录映射保持原值。", "Canceled; the client mapping is unchanged."));
    } catch (cause) { if (isCurrent()) setError(nativeDisplayMessage(cause instanceof Error ? cause : { message: String(cause) }, en)); }
    finally { if (isCurrent()) setBusy(false); }
  };
  const open = async (action: "reveal" | "open") => {
    if (!grant || !relativePath) return;
    const operation = ++operationRef.current;
    const isCurrent = () => currentOwnerRef.current === ownerKey && operationRef.current === operation;
    setBusy(true); setError(""); setStatus("");
    try {
      await openNativeWorkspaceFile(state, context, grant, relativePath, action);
      if (!isCurrent()) return;
      setStatus(action === "reveal" ? t("已请求系统文件管理器显示本机文件。", "Requested the system file manager to reveal the client file.") : t("已请求默认应用打开本机文件。", "Requested the default application to open the client file."));
    } catch (cause) { if (isCurrent()) setError(nativeDisplayMessage(cause instanceof Error ? cause : { message: String(cause) }, en)); }
    finally { if (isCurrent()) setBusy(false); }
  };
  return <details className="shrink-0 border-b border-line px-3 py-2 text-[11.5px] text-ink-3" aria-busy={busy || undefined}>
    <summary className="cursor-pointer text-ink-2">{t("本机文件操作", "Client file actions")}{grant ? t(" · 手动映射", " · manual mapping") : t(" · 未连接本机目录", " · no client directory connected")}</summary>
    <p className="mt-2">{t("服务宿主工作区：", "Service host workspace: ")}<code className="break-all">{serviceRoot}</code></p>
    {grant && <p>{t("桌面本机对应目录：", "Corresponding client directory: ")}<code className="break-all">{grant.clientRoot}</code></p>}
    <p className="my-2">{t("选择当前服务工作区在桌面本机的对应目录。系统操作只作用于该目录；文件同步尚未验证。", "Select the client directory corresponding to this service workspace. System actions apply to that directory; file synchronization has not been verified.")}</p>
    <div className="flex flex-wrap gap-2"><button type="button" disabled={busy || !available || !workspaceId || !serviceRoot} onClick={() => void select()} className="rounded-control border border-line px-2 py-1 disabled:opacity-50">{busy ? t("处理中…", "Working…") : grant ? t("更换本机对应目录", "Change client directory") : t("连接本机对应目录", "Connect client directory")}</button>
      {relativePath && <><button type="button" disabled={busy || !available || !grant || Boolean(pathError)} onClick={() => void open("reveal")} className="rounded-control border border-line px-2 py-1 disabled:opacity-50">{t("在系统文件管理器中显示", "Reveal in system file manager")}</button><button type="button" disabled={busy || !available || !grant || Boolean(pathError)} onClick={() => void open("open")} className="rounded-control border border-line px-2 py-1 disabled:opacity-50">{t("用默认应用打开", "Open in default application")}</button></>}
    </div>
    {!available && <p className="mt-2">{stateError || (!capability.available ? nativeDisplayMessage(capability, en) : t("桌面文件操作接口未提供。", "Native file actions are unavailable."))}</p>}
    {relativePath && !grant && <p className="mt-2">{t("连接本机对应目录后，可对该目录内的文件执行系统操作。服务端预览和下载仍可使用。", "Connect a client directory to use system actions on its files. Service previews and downloads remain available.")}</p>}
    {(error || pathError) && <p role="alert" className="mt-2 text-red">{error || pathError}</p>}
    {status && <p role="status" className="mt-2">{status}</p>}
  </details>;
}
