"use client";

import { useEffect, useState } from "react";
import { Button, SegmentedControl, Select, Switch } from "@/components/chat/ui";
import { accessModeMeta } from "@/components/conversation/ConversationComposerModes";
import { EDITOR_CHOICES, useChatPreferences, writeChatPreferences, type DiffViewDefault, type EditorChoice, type RunningSendDefault, type SendKey } from "@/lib/chatPreferences";
import { useConversationServerPreferences, writeConversationServerPreferences } from "@/lib/conversationServerPreferences";
import { useLang } from "@/lib/i18n";
import { apiFetch, type LlmProfile } from "@/lib/useRun";
import { isMacPlatform } from "@/lib/shortcutBindings";
import { SettingsNote, SettingsRow, SettingsSection } from "./primitives";

const ACCESS_OPTIONS = ["supervised", "auto-accept-edits", "auto", "full-access"] as const;

const EDITOR_LABELS: Record<EditorChoice, [string, string]> = {
  auto: ["自动（依次尝试 VS Code、Cursor、Windsurf、Zed）", "Automatic (VS Code, Cursor, Windsurf, Zed)"],
  code: ["VS Code", "VS Code"],
  cursor: ["Cursor", "Cursor"],
  windsurf: ["Windsurf", "Windsurf"],
  zed: ["Zed", "Zed"],
  system: ["系统默认应用", "System default application"],
};

type TitlerState = { status: "loading" } | { status: "ready"; profile: LlmProfile | null } | { status: "error"; message: string };

function useTitlerProfile(): TitlerState {
  const [state, setState] = useState<TitlerState>({ status: "loading" });
  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        const response = await apiFetch("/api/settings/workers");
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        const body = await response.json() as { config?: { llm_profiles?: { titler?: LlmProfile } } };
        if (alive) setState({ status: "ready", profile: body.config?.llm_profiles?.titler ?? null });
      } catch (error) {
        if (alive) setState({ status: "error", message: error instanceof Error ? error.message : String(error) });
      }
    })();
    return () => { alive = false; };
  }, []);
  return state;
}

export function ChatSettings({ navigate }: { navigate: (href: string) => void }) {
  const { lang } = useLang(); const en = lang === "en";
  const t = (zh: string, english: string) => en ? english : zh;
  const prefs = useChatPreferences();
  const titler = useTitlerProfile();
  const [storageError, setStorageError] = useState("");
  const mac = isMacPlatform();
  const mod = mac ? "⌘" : "Ctrl";
  const save = (patch: Parameters<typeof writeChatPreferences>[0]) => {
    setStorageError(writeChatPreferences(patch) ? "" : t("设置未能提交，请查看共享偏好同步提示或本机存储状态。", "Could not submit the setting. Check preference synchronization or local storage."));
  };
  const serverPrefs = useConversationServerPreferences();
  const [quotaSaving, setQuotaSaving] = useState(false);
  const [quotaError, setQuotaError] = useState("");
  const quotaFailure = quotaError || (serverPrefs.status === "error" ? serverPrefs.message : "");
  const quotaDescription = quotaFailure
    ? t(`无法读取或保存服务端设置：${quotaFailure}`, `Could not load or save the service setting: ${quotaFailure}`)
    : t(
      "引擎报告额度用尽并给出重置时间时（目前为 Claude 与 Codex），Muteki 服务会安排在重置后自动继续被中断的对话。关闭窗口也会执行，只需服务保持运行；对话页横幅可单独取消某个对话的安排。",
      "When the engine reports an exhausted quota with a reset time (currently Claude and Codex), the Muteki service schedules the interrupted chat to continue after the reset. This runs with no window open as long as the service is running; the chat banner can cancel it for one chat.");
  const accessValue = prefs.defaultAccessMode || "supervised";
  const titlerText = titler.status === "loading" ? t("正在读取…", "Loading…")
    : titler.status === "error" ? t(`读取失败：${titler.message}`, `Could not load: ${titler.message}`)
    : titler.profile?.model ? `${titler.profile.provider ? `${titler.profile.provider} · ` : ""}${titler.profile.model}`
    : t("未单独配置", "Not configured");

  return <>
    <SettingsSection title={t("输入与发送", "Compose and send")}>
      <SettingsRow anchor="chat-send-key" title={t("发送键", "Send key")}
        description={prefs.sendKey === "enter"
          ? t("Enter 发送，Shift+Enter 换行。", "Enter sends; Shift+Enter inserts a new line.")
          : t(`${mod}+Enter 发送，Enter 换行。`, `${mod}+Enter sends; Enter inserts a new line.`)}
        control={<SegmentedControl<SendKey> size="md" ariaLabel={t("发送键", "Send key")} value={prefs.sendKey} onChange={sendKey => save({ sendKey })}
          options={[{ value: "enter", label: t("Enter 发送", "Enter") }, { value: "mod-enter", label: t(`${mod}+Enter 发送`, `${mod}+Enter`) }]} />} />
      <SettingsRow anchor="chat-running-send" title={t("运行中发送消息", "Sending during a run")}
        description={t(
          "Agent 正在回答时按发送键的默认行为。“引导”只在当前 Agent 支持且消息不含附件或引用时生效，否则仍加入后续队列。按住 Alt/⌥ 发送或点击输入框旁的按钮可临时改用另一种方式。",
          "What the send key does while the Agent is answering. Steering applies only when the Agent supports it and the message has no attachments or references; otherwise it is queued. Hold Alt/⌥ while sending, or use the button next to the composer, to use the other behaviour once.")}
        control={<SegmentedControl<RunningSendDefault> size="md" ariaLabel={t("运行中发送消息", "Sending during a run")} value={prefs.runningSend} onChange={runningSend => save({ runningSend })}
          options={[{ value: "queue", label: t("加入队列", "Queue") }, { value: "steer", label: t("引导当前回答", "Steer") }]} />} />
      <SettingsRow anchor="chat-quota-resume" title={t("额度重置后自动继续", "Continue after quota reset")}
        description={quotaDescription}
        control={<Switch ariaLabel={t("额度重置后自动继续", "Continue after quota reset")}
          checked={serverPrefs.status === "ready" && serverPrefs.prefs.autoResumeOnQuotaReset}
          disabled={serverPrefs.status !== "ready" || quotaSaving}
          onCheckedChange={autoResumeOnQuotaReset => {
            setQuotaSaving(true);
            setQuotaError("");
            writeConversationServerPreferences({ autoResumeOnQuotaReset })
              .catch(cause => setQuotaError(cause instanceof Error ? cause.message : String(cause)))
              .finally(() => setQuotaSaving(false));
          }} />} />
    </SettingsSection>

    <SettingsSection title={t("新对话", "New chats")}>
      <SettingsRow anchor="chat-access-mode" title={t("默认权限模式", "Default permission mode")}
        description={t(
          "新建对话时预选的 Agent 操作权限。项目设置了默认权限时以项目为准；当前 Agent 不支持所选模式时回到“严格监督”。",
          "Permission preselected for new chats. A project default takes precedence; if the Agent does not support the mode, new chats use Supervised.")}
        control={<Select<string> size="md" ariaLabel={t("默认权限模式", "Default permission mode")} value={accessValue} onChange={mode => save({ defaultAccessMode: mode === "supervised" ? "" : mode })}
          popoverClassName="w-[300px]"
          options={ACCESS_OPTIONS.map(mode => { const meta = accessModeMeta(mode); return { value: mode, label: meta.label, textValue: meta.label, description: meta.detail, icon: meta.icon }; })} />} />
      <SettingsRow anchor="chat-title-model" title={t("标题生成模型", "Title generation model")}
        description={t(
          `当前：${titlerText}。标题与摘要优先使用 Titler 配置；不可用时回退到该对话正在使用的模型。`,
          `Current: ${titlerText}. Titles and summaries use the Titler profile first and fall back to the chat's own model when it is unavailable.`)}
        control={<Button size="sm" variant="secondary" icon="arrowUpRight" onClick={() => navigate("/ctf/workers")}>{t("配置 Titler", "Configure Titler")}</Button>} />
    </SettingsSection>

    <SettingsSection title={t("工作区", "Workspace")}>
      <SettingsRow anchor="chat-editor" title={t("在编辑器中打开", "Open in editor")}
        description={t(
          "桌面端“在编辑器中打开”使用的应用，文件会跳到对应行。指定的编辑器未安装命令行时会直接报错，不会改用其他应用。",
          "App used by the desktop “Open in editor” action; files open at the referenced line. If the chosen editor's command line is missing, the action fails instead of using another app.")}
        control={<Select<EditorChoice> size="md" ariaLabel={t("在编辑器中打开", "Open in editor")} value={prefs.editor} onChange={editor => save({ editor })}
          popoverClassName="w-[300px]"
          options={EDITOR_CHOICES.map(value => ({ value, label: t(EDITOR_LABELS[value][0], EDITOR_LABELS[value][1]), textValue: EDITOR_LABELS[value][1] }))} />} />
    </SettingsSection>

    <SettingsSection anchor="chat-diff" title={t("变更 (Diff)", "Changes (Diff)")} description={t("Diff 的显示方式，修改后已打开的 Diff 立即生效。单个对话里手动切换的布局会被记住，直到这里的默认视图再次改变。", "How diffs are displayed; changes apply to open diffs immediately. A layout chosen inside a chat is remembered until the default layout here changes again.")}>
      <SettingsRow title={t("默认视图", "Default layout")}
        control={<SegmentedControl<DiffViewDefault> size="md" ariaLabel={t("默认视图", "Default layout")} value={prefs.diffView} onChange={diffView => save({ diffView })}
          options={[{ value: "unified", label: t("合并", "Unified"), icon: "alignJustify" }, { value: "split", label: t("分栏", "Split"), icon: "columns" }]} />} />
      <SettingsRow title={t("自动换行", "Wrap lines")}
        control={<Switch ariaLabel={t("自动换行", "Wrap lines")} checked={prefs.diffWrap} onCheckedChange={diffWrap => save({ diffWrap })} />} />
      <SettingsRow title={t("默认折叠未变更的行", "Fold unchanged lines")} description={t("只显示变更附近 3 行上下文。", "Shows 3 lines of context around each change.")}
        control={<Switch ariaLabel={t("默认折叠未变更的行", "Fold unchanged lines")} checked={prefs.diffCollapseUnchanged} onCheckedChange={diffCollapseUnchanged => save({ diffCollapseUnchanged })} />} />
    </SettingsSection>
    {storageError ? <SettingsNote tone="danger">{storageError}</SettingsNote> : null}
  </>;
}
