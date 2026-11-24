"use client";

import { useState } from "react";
import { SegmentedControl, Select, Switch } from "@muteki/ui";
import { accessModeMeta } from "@/components/conversation/ConversationComposerModes";
import { EDITOR_CHOICES, useChatPreferences, writeChatPreferences, type DiffViewDefault, type EditorChoice, type SendKey, type RunningSendDefault } from "@/lib/chatPreferences";
import { useConversationServerPreferences, writeConversationServerPreferences } from "@/lib/conversationServerPreferences";
import { useLang } from "@/lib/i18n";
import { isMacPlatform } from "@/lib/shortcutBindings";
import { SettlementSettings } from "./ControlPolicySettings";
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

export function ChatSettings() {
  const { lang } = useLang(); const en = lang === "en";
  const t = (zh: string, english: string) => en ? english : zh;
  const prefs = useChatPreferences();
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
  const quotaSupported = serverPrefs.status === "ready" && serverPrefs.prefs.autoResumeOnQuotaResetSupported;
  const quotaDescription = serverPrefs.status === "ready" && !quotaSupported
    ? t("当前服务尚不支持额度重置后自动继续。", "The current service does not yet support automatic continuation after quota reset.")
    : quotaFailure
    ? t(`无法读取或保存服务端设置：${quotaFailure}`, `Could not load or save the service setting: ${quotaFailure}`)
    : t(
      "引擎报告额度用尽并给出重置时间时（目前为 Claude 与 Codex），Muteki 服务会安排在重置后自动继续被中断的对话。关闭窗口也会执行，只需服务保持运行；对话页横幅可单独取消某个对话的安排。",
      "When the engine reports an exhausted quota with a reset time (currently Claude and Codex), the Muteki service schedules the interrupted chat to continue after the reset. This runs with no window open as long as the service is running; the chat banner can cancel it for one chat.");
  const accessValue = prefs.defaultAccessMode || "supervised";

  return <>
    <SettingsSection title={t("输入与发送", "Compose and send")}>
      <SettingsRow anchor="chat-send-key" title={t("发送键", "Send key")}
        description={prefs.sendKey === "enter"
          ? t("Enter 发送；空闲时 Shift+Enter 换行，运行中 Alt+Enter 换行。", "Enter sends. Shift+Enter adds a line when idle; Alt+Enter adds a line during a run.")
          : t(`${mod}+Enter 发送，Enter 换行。`, `${mod}+Enter sends; Enter inserts a new line.`)}
        control={<SegmentedControl<SendKey> size="md" ariaLabel={t("发送键", "Send key")} value={prefs.sendKey} onChange={sendKey => save({ sendKey })}
          options={[{ value: "enter", label: t("Enter 发送", "Enter") }, { value: "mod-enter", label: t(`${mod}+Enter 发送`, `${mod}+Enter`) }]} />} />
      <SettingsRow anchor="chat-running-send" title={t("后续消息处理方式", "Follow-up messages")}
        description={t("Agent 运行时，新消息默认排队等待下一轮，或直接引导当前运行。Shift+Enter 对单条消息执行相反操作；Alt+Enter 换行。附件随消息一起发送。", "During a run, queue new messages for the next turn or steer the current run. Shift+Enter uses the opposite action for one message; Alt+Enter adds a line. Attachments are included.")}
        control={<SegmentedControl<RunningSendDefault> size="md" ariaLabel={t("后续消息处理方式", "Follow-up messages")} value={prefs.runningSend} onChange={runningSend => save({ runningSend })}
          options={[{ value: "queue", label: t("排队", "Queue") }, { value: "steer", label: t("引导", "Steer") }]} />} />
      <SettingsRow anchor="chat-quota-resume" title={t("额度重置后自动继续", "Continue after quota reset")}
        description={quotaDescription}
        control={<Switch ariaLabel={t("额度重置后自动继续", "Continue after quota reset")}
          checked={serverPrefs.status === "ready" && serverPrefs.prefs.autoResumeOnQuotaReset}
          disabled={!quotaSupported || quotaSaving}
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
          "所有项目新建对话时统一预选的 Agent 操作权限。当前 Agent 不支持所选模式时回到“严格监督”。",
          "Permission preselected for new chats across all projects. If the Agent does not support the mode, new chats use Supervised.")}
        control={<Select<string> size="md" ariaLabel={t("默认权限模式", "Default permission mode")} value={accessValue} onChange={mode => save({ defaultAccessMode: mode === "supervised" ? "" : mode })}
          popoverClassName="w-[300px]"
          options={ACCESS_OPTIONS.map(mode => { const meta = accessModeMeta(mode); return { value: mode, label: meta.label, textValue: meta.label, description: meta.detail, icon: meta.icon }; })} />} />
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
    <SettingsSection anchor="chat-thread-display" title={t("线程显示", "Thread display")}>
      <SettingsRow title={t("发送消息时滚动到最新", "Scroll to latest on send")}
        description={t("线程未停在最新处时，你发送的消息送达后自动滚动到最新内容。", "When the thread is scrolled up, jump to the latest content once your message is sent.")}
        control={<Switch ariaLabel={t("发送消息时滚动到最新", "Scroll to latest on send")} checked={prefs.scrollToLatestOnSend} onCheckedChange={scrollToLatestOnSend => save({scrollToLatestOnSend})} />} />
      <SettingsRow title={t("折叠过长的命令", "Shorten long commands")}
        description={t("时间线中过长的命令压缩成一行摘要；关闭后显示完整命令。展开命令仍可看到全文。", "Long commands in the timeline are compacted to one line; turn off to show the full command. Expanding a command still shows all of it.")}
        control={<Switch ariaLabel={t("折叠过长的命令", "Shorten long commands")} checked={prefs.shortenLongCommands} onCheckedChange={shortenLongCommands => save({shortenLongCommands})} />} />
      <SettingsRow title={t("实时命令面板", "Live command panel")}
        description={t("命令运行时，在它下方显示最新的几行输出。", "While a command runs, show its newest output lines under it.")}
        control={<Switch ariaLabel={t("实时命令面板", "Live command panel")} checked={!prefs.liveCommandPanelDisabled} onCheckedChange={on => save({liveCommandPanelDisabled: !on})} />} />
      <SettingsRow title={t("后台降低刷新频率", "Throttle updates in background")}
        description={t("窗口在后台时，按固定间隔合并对话的实时更新以节省能耗；关闭后在后台也立即更新。", "While the window is in the background, batch live chat updates at a fixed interval to save energy; turn off to update immediately.")}
        control={<Switch ariaLabel={t("后台降低刷新频率", "Throttle updates in background")} checked={!prefs.inactiveFrameThrottleDisabled} onCheckedChange={on => save({inactiveFrameThrottleDisabled: !on})} />} />
    </SettingsSection>
    <SettlementSettings />
    {storageError ? <SettingsNote tone="danger">{storageError}</SettingsNote> : null}
  </>;
}
