"use client";

import { NATIVE_CAPABILITIES, nativeDisplayMessage, nativeManifest, useNativeDesktopState } from "@/lib/nativeDesktop";
import { useLang } from "@/lib/i18n";
import type { NativeCapabilityId } from "@/lib/desktopChatBridge";

const LABELS: Record<NativeCapabilityId, [string, string]> = {
  pathSelection: ["原生文件与目录选择", "Native file and directory picker"], workspaceFileActions: ["本机文件显示与默认应用打开", "Client file reveal and default application"],
  preview: ["独立网页预览", "Isolated web preview"], attachmentCache: ["附件恢复存储", "Attachment recovery storage"], terminal: ["终端连接", "Terminal connection"],
  microphone: ["麦克风权限申请", "Microphone permission request"], notifications: ["系统通知", "System notifications"], deepLinks: ["桌面链接", "Desktop links"],
};

export function NativeCapabilitiesPanel() {
  const { lang } = useLang(); const en = lang === "en"; const t = (zh: string, english: string) => en ? english : zh;
  const { desktop, state, error } = useNativeDesktopState();
  if (!desktop) return null;
  let manifest;
  try { manifest = nativeManifest(state?.capabilities); }
  catch (cause) { return <section aria-label={t("桌面本机能力", "Desktop native capabilities")} className="rounded-control border border-line p-3"><h3 className="text-[13px] font-medium text-ink">{t("桌面本机能力", "Desktop native capabilities")}</h3><p role="status" className="text-[12px] text-ink-3">{error || nativeDisplayMessage(cause instanceof Error ? cause : { message: String(cause) }, en)}</p></section>; }
  return <section aria-label={t("桌面本机能力", "Desktop native capabilities")} className="rounded-control border border-line p-3">
    <h3 className="text-[13px] font-medium text-ink">{t("桌面本机能力", "Desktop native capabilities")}</h3>
    <p className="my-2 text-[12px] text-ink-3">{t("清单来自桌面客户端。服务宿主的 Agent 能力由相应运行环境提供。", "This manifest comes from the desktop client. Service Agent capabilities are supplied by the selected runtime.")}</p>
    <dl className="flex flex-col gap-2">{NATIVE_CAPABILITIES.map(id => {
      const entry = manifest.entries[id];
      return <div key={id} className="text-[12px]"><dt className="font-medium text-ink-2">{LABELS[id][en ? 1 : 0]}</dt><dd className="text-ink-3">{entry.supported ? t("支持", "Supported") : t("未提供", "Unavailable")} · {entry.host === "desktop-client" ? t("桌面本机", "Desktop client") : t("服务宿主", "Service host")}{entry.reason ? ` · ${nativeDisplayMessage(entry, en)}` : ""}</dd></div>;
    })}</dl>
    <details className="mt-3 text-[11px] text-ink-3"><summary>{t("诊断信息", "Diagnostics")}</summary><p>{t("能力清单版本：", "Manifest version: ")}{manifest.version}</p>{NATIVE_CAPABILITIES.filter(id => manifest.entries[id].code).map(id => <p key={id}>{LABELS[id][en ? 1 : 0]}: <code>{manifest.entries[id].code}</code>{manifest.entries[id].reason ? ` · ${manifest.entries[id].reason}` : ""}</p>)}</details>
  </section>;
}
