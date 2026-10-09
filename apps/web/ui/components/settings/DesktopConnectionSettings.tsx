import { Badge, Button, StatusDot } from '@/components/chat/ui';
import { SettingsNote, SettingsRow, SettingsSection } from '@/components/settings/primitives';
import type { NativeCapabilityId } from '@/lib/desktopChatBridge';
import { useLang } from '@/lib/i18n';
import { NATIVE_CAPABILITIES, nativeDisplayMessage, nativeManifest, useNativeDesktopState } from '@/lib/nativeDesktop';

function useCopy() {
  const { lang } = useLang(); const en = lang === 'en';
  return { en, t: (zh: string, english: string) => en ? english : zh };
}

const CAPABILITY_LABELS: Record<NativeCapabilityId, [string, string]> = {
  pathSelection: ['原生文件与目录选择', 'Native file and directory picker'], workspaceFileActions: ['在访达中显示与用默认应用打开', 'Reveal files and open with default app'],
  preview: ['独立网页预览', 'Isolated web preview'], attachmentCache: ['附件恢复存储', 'Attachment recovery storage'], terminal: ['终端连接', 'Terminal connection'],
  microphone: ['麦克风', 'Microphone'], notifications: ['系统通知', 'System notifications'], deepLinks: ['桌面链接', 'Desktop links'],
};

export function DesktopPage({ origin, connected, onConfigure, onHelp }: { origin: string; connected: boolean; onConfigure: () => void; onHelp: () => void }) {
  const { en, t } = useCopy();
  const { desktop, state, error } = useNativeDesktopState();
  let manifest: ReturnType<typeof nativeManifest> | null = null; let manifestError = '';
  if (desktop) {
    try { manifest = nativeManifest(state?.capabilities); }
    catch (cause) { manifestError = error || nativeDisplayMessage(cause instanceof Error ? cause : { message: String(cause) }, en); }
  }
  const host = (() => { try { return origin ? new URL(origin).host : ''; } catch { return origin; } })();
  return <>
    <SettingsSection anchor="desktop-connection" title={t('服务连接', 'Service connection')}>
      <SettingsRow title={host || t('未连接', 'Not connected')} description={origin || t('连接一个 Muteki 工作台后即可使用对话与设置。', 'Connect a Muteki workspace to use chat and settings.')}
        control={<Badge tone={connected ? 'success' : 'warning'}>{connected ? t('已连接', 'Connected') : t('未连接', 'Disconnected')}</Badge>} />
      <SettingsRow title={t('更换工作台', 'Switch workspace')} description={t('草稿会先保存在本机，再切换到新的服务地址。', 'Drafts are saved locally before switching to another service address.')}
        control={<Button variant="secondary" size="sm" icon="plug" onClick={onConfigure}>{t('连接设置', 'Connection')}</Button>} />
      <SettingsRow title={t('桌面端说明', 'About the desktop client')} description={t('预览隔离、本机路径与服务端路径的区别。', 'How previews are isolated and how client paths differ from service paths.')}
        control={<Button variant="ghost" size="sm" icon="help" onClick={onHelp}>{t('查看', 'Open')}</Button>} />
    </SettingsSection>

    <SettingsSection anchor="desktop-capabilities" title={t('本机能力', 'Native capabilities')} description={t('清单来自桌面客户端；Agent 自身的能力由服务宿主的运行环境提供。', 'Reported by the desktop client. Agent capabilities come from the runtime on the service host.')}>
      {manifest ? NATIVE_CAPABILITIES.map(id => {
        const entry = manifest.entries[id];
        return <div key={id} className="flex items-center gap-3 border-b border-cx-border-subtle px-5 py-3 last:border-b-0">
          <StatusDot tone={entry.supported ? 'success' : 'neutral'} className="size-2" />
          <div className="min-w-0 flex-1">
            <div className="text-[14px] leading-5 text-cx-fg">{CAPABILITY_LABELS[id][en ? 1 : 0]}</div>
            {entry.reason ? <div className="mt-0.5 text-[12.5px] leading-5 text-cx-fg-3">{nativeDisplayMessage(entry, en)}</div> : null}
          </div>
          <span className="shrink-0 text-[12.5px] text-cx-fg-3">{entry.supported ? (entry.host === 'desktop-client' ? t('本机', 'This computer') : t('服务宿主', 'Service host')) : t('不可用', 'Unavailable')}</span>
        </div>;
      }) : <div className="px-5 py-4 text-[13px] text-cx-fg-3" role="status">{manifestError || t('正在读取能力清单…', 'Reading capability manifest…')}</div>}
    </SettingsSection>
    {manifest && NATIVE_CAPABILITIES.some(id => manifest.entries[id].code) ? <details className="px-1 text-[12.5px] text-cx-fg-3">
      <summary className="cursor-pointer select-none">{t('诊断信息', 'Diagnostics')}</summary>
      <div className="mt-2 flex flex-col gap-1 font-mono text-[12px]">
        <span>{t('能力清单版本', 'Manifest version')}: {manifest.version}</span>
        {NATIVE_CAPABILITIES.filter(id => manifest.entries[id].code).map(id => <span key={id}>{CAPABILITY_LABELS[id][en ? 1 : 0]}: {manifest.entries[id].code}{manifest.entries[id].reason ? ` · ${manifest.entries[id].reason}` : ''}</span>)}
      </div>
    </details> : null}
    {!desktop ? <SettingsNote>{t('当前环境不是桌面客户端。', 'This is not the desktop client.')}</SettingsNote> : null}
  </>;
}
