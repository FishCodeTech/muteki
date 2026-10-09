"use client";

import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from 'react';
import { Button, Switch } from '@/components/chat/ui';
import { cn } from '@/lib/cn';
import { desktopChatBridge } from '@/lib/desktopChatBridge';
import { conversationStorageScope, subscribeConversationStorageScope } from '@/lib/conversationStorageScope';
import { useNativeDesktopState } from '@/lib/nativeDesktop';
import { useLang } from '@/lib/i18n';
import {
  defaultNotificationPrefs, readNotificationDelivery, readNotificationPermissionStatus, readNotificationPrefs,
  requestNotificationPermissionStatus, subscribeNotificationDelivery, subscribeNotificationPrefs, unlockNotificationAudio,
  updateNotificationPrefs, type NotificationDeliveryDiagnostic, type NotificationMode, type NotificationPrefs,
} from '@/lib/threadNotifications';
import { SettingsNote, SettingsRow, SettingsSection } from './primitives';

const MODES: Array<{ value: NotificationMode; label: [string, string]; hint: [string, string] }> = [
  { value: 'notifications', label: ['系统通知', 'System notifications'], hint: ['会话完成、需要审批或等待输入时弹出系统通知。', 'Show a desktop notification when a chat finishes, needs approval, or waits for input.'] },
  { value: 'notifications-and-sound', label: ['通知和提示音', 'Notifications and sound'], hint: ['同时播放一声简短提示音。', 'Also play a short sound.'] },
  { value: 'sound', label: ['仅提示音', 'Sound only'], hint: ['不弹通知，只播放提示音。', 'Play a sound without a notification.'] },
  { value: 'off', label: ['关闭', 'Off'], hint: ['只在侧栏活动视图里提示。', 'Only show items in the sidebar activity view.'] },
];

const DELIVERY: Record<NotificationDeliveryDiagnostic['status'], [string, string]> = {
  submitting: ['正在提交通知', 'Submitting notification'], submitted: ['已提交到桌面通知', 'Submitted to desktop notifications'],
  awaiting_show: ['等待系统接收回执', 'Awaiting system acceptance'], shown: ['系统已接收通知', 'Accepted by the system'],
  failed: ['通知投递失败', 'Notification delivery failed'], outcome_unknown: ['投递结果未知', 'Delivery outcome unknown'],
  clicked: ['已点击通知', 'Notification clicked'], closed: ['通知已关闭', 'Notification closed'],
};

export function NotificationsSettings() {
  const { lang } = useLang(); const en = lang === 'en';
  const t = useCallback((zh: string, english: string) => en ? english : zh, [en]);
  const scope = useSyncExternalStore(subscribeConversationStorageScope, conversationStorageScope, () => '');
  const nativeState = useNativeDesktopState();
  const native = nativeState.desktop;
  const nativeScope = `${nativeState.state?.connectionVersion || 0}:${nativeState.state?.serviceId || ''}:${nativeState.state?.identityId || ''}`;
  const delivery = useSyncExternalStore(subscribeNotificationDelivery, readNotificationDelivery, () => null);
  const currentDelivery = native && delivery
    && (delivery.connectionVersion === undefined || delivery.connectionVersion === nativeState.state?.connectionVersion)
    && (delivery.serviceId === undefined || delivery.serviceId === nativeState.state?.serviceId)
    && (delivery.identityId === undefined || delivery.identityId === nativeState.state?.identityId) ? delivery : null;
  const [prefs, setPrefs] = useState<NotificationPrefs>(defaultNotificationPrefs);
  const [permission, setPermission] = useState<NotificationPermission | 'unsupported'>('default');
  const [systemPermission, setSystemPermission] = useState('unknown');
  const [systemDiagnostic, setSystemDiagnostic] = useState<unknown>(null);
  const [permissionNote, setPermissionNote] = useState('');
  const [requesting, setRequesting] = useState(false);
  const [saveError, setSaveError] = useState('');
  const generation = useRef(0); const busy = useRef(false); const mounted = useRef(false);

  useEffect(() => { setPrefs(readNotificationPrefs()); return subscribeNotificationPrefs(setPrefs); }, []);
  useEffect(() => {
    mounted.current = true;
    const gen = generation;
    busy.current = false; setRequesting(false); setPermission('default'); setPermissionNote('');
    const read = () => {
      if (busy.current) return;
      const id = ++gen.current;
      const current = () => mounted.current && id === gen.current && scope === conversationStorageScope();
      void readNotificationPermissionStatus().then(status => { if (current()) { setPermission(status.permission); setSystemPermission(status.systemPermission || 'unknown'); setSystemDiagnostic(status.systemPermissionError || status.systemNotificationSettings || null); } })
        .catch(error => { if (current()) setPermissionNote(error instanceof Error ? error.message : String(error)); });
    };
    read(); window.addEventListener('focus', read);
    return () => { window.removeEventListener('focus', read); mounted.current = false; ++gen.current; };
  }, [scope, nativeScope]);

  const persist = useCallback((patch: Parameters<typeof updateNotificationPrefs>[0]) => {
    const result = updateNotificationPrefs(patch);
    setPrefs(result.prefs);
    setSaveError(result.persisted ? '' : t(`通知设置未保存，当前窗口按新选择生效。${result.error || ''}`, `Notification settings were not saved; this window uses the new choice. ${result.error || ''}`));
  }, [t]);

  const requestPermission = useCallback(async () => {
    if (busy.current || !mounted.current || scope !== conversationStorageScope()) return;
    busy.current = true; setRequesting(true); setPermissionNote('');
    const id = ++generation.current, owner = scope;
    const current = () => mounted.current && id === generation.current && owner === conversationStorageScope();
    unlockNotificationAudio();
    try {
      const status = await requestNotificationPermissionStatus();
      if (!current()) return;
      setPermission(status.permission);
      setSystemPermission(status.systemPermission || 'unknown');
      setSystemDiagnostic(status.systemPermissionError || status.systemNotificationSettings || null);
      setPermissionNote(status.permission === 'denied'
        ? native ? t('此工作台的通知已关闭，你可以在这里更改选择。', 'Notifications are off for this workspace. You can change your choice here.')
          : t('浏览器已拒绝通知。请在浏览器的网站设置中重新允许。', 'The browser blocked notifications. Allow them again in the site settings.')
        : status.permission === 'granted' ? t('已允许。若仍收不到通知，请检查系统通知设置。', 'Allowed. If notifications still do not arrive, check the system notification settings.')
          : status.permission === 'unsupported' ? t('当前环境不支持系统通知。', 'System notifications are not supported here.') : '');
    } catch (error) {
      if (current()) setPermissionNote(error instanceof Error ? error.message : String(error));
    } finally {
      if (current()) { busy.current = false; setRequesting(false); }
    }
  }, [native, scope, t]);

  const permissionLabel = permission === 'granted' ? t('已允许', 'Allowed') : permission === 'denied' ? t('已拒绝', 'Denied')
    : permission === 'unsupported' ? t('不支持', 'Unsupported') : t('尚未授权', 'Not requested');
  const bridge = desktopChatBridge();

  return <>
    {saveError ? <SettingsNote tone="danger">{saveError}</SettingsNote> : null}
    <SettingsSection anchor="notifications-style" title={t('提醒方式', 'Alert style')}>
      <div role="radiogroup" aria-label={t('提醒方式', 'Alert style')}>
        {MODES.map(mode => {
          const on = prefs.mode === mode.value;
          return <button key={mode.value} type="button" role="radio" aria-checked={on}
            onClick={() => { unlockNotificationAudio(); persist({ mode: mode.value }); }}
            className="flex w-full items-center gap-3 border-b border-cx-border-subtle px-5 py-3.5 text-left outline-none last:border-b-0 hover:bg-cx-hover focus-visible:bg-cx-hover">
            <span className={cn('grid size-4 shrink-0 place-items-center rounded-full border', on ? 'border-cx-accent' : 'border-cx-border-strong')}>
              {on ? <span className="size-2 rounded-full bg-cx-accent" /> : null}
            </span>
            <span className="min-w-0 flex-1">
              <span className="block text-[14px] font-medium text-cx-fg">{mode.label[en ? 1 : 0]}</span>
              <span className="mt-0.5 block text-[13px] text-cx-fg-3">{mode.hint[en ? 1 : 0]}</span>
            </span>
          </button>;
        })}
      </div>
    </SettingsSection>

    <SettingsSection anchor="notifications-permission" title={t('系统权限', 'Permission')}>
      <SettingsRow title={t('工作台通知授权', 'Workspace permission')} description={`${permissionLabel} · ${native ? t('工作台授权与系统通知设置分别管理。', 'Workspace consent and system settings are separate.') : t('由浏览器管理，可随时在网站设置中更改。', 'Managed by the browser; change it any time in site settings.')}`}
        control={<div className="flex items-center gap-2">
          {native && bridge?.openPermissionSettings ? <Button variant="ghost" onClick={() => { const owner = scope; void bridge.openPermissionSettings?.('notifications').catch(error => { if (mounted.current && owner === conversationStorageScope()) setPermissionNote(error instanceof Error ? error.message : String(error)); }); }}>{t('系统设置', 'System settings')}</Button> : null}
          <Button variant="secondary" loading={requesting} disabled={requesting || permission === 'unsupported' || (!native && permission === 'granted')} onClick={() => void requestPermission()}>{native && (permission === 'granted' || permission === 'denied') ? t('更改选择', 'Change choice') : t('允许通知', 'Allow')}</Button>
        </div>} />
      {native ? <SettingsRow title={t('系统通知权限', 'System notification permission')} description={systemPermission === 'granted' ? t('系统已允许', 'Allowed by the system') : systemPermission === 'denied' ? t('系统已拒绝，请在系统设置中更改', 'Denied by the system; change it in system settings') : systemPermission === 'default' ? t('系统尚未收到授权选择', 'No system authorization choice yet') : systemPermission === 'provisional' ? t('系统仅允许静默通知', 'Only quiet notifications are allowed') : systemPermission === 'ephemeral' ? t('系统临时允许通知', 'Notifications are temporarily allowed') : t('系统状态未知', 'System status unknown')} >{systemDiagnostic ? <details><summary>{t('系统诊断', 'System diagnostic')}</summary><pre className="whitespace-pre-wrap break-all">{JSON.stringify(systemDiagnostic, null, 2)}</pre></details> : null}</SettingsRow> : null}
      {native ? <SettingsRow title={t('最近一次投递', 'Latest delivery')} description={currentDelivery
        ? currentDelivery.code === 'desktop.notifications_system_denied' ? t('系统未允许通知，本次未投递', 'Denied by the system; not delivered') : currentDelivery.code === 'desktop.notifications_workspace_denied' ? t('当前工作台未允许通知', 'Not allowed by this workspace') : currentDelivery.code === 'desktop.notifications_unsupported' ? t('当前设备不支持通知', 'Notifications are unsupported') : currentDelivery.status === 'failed' && currentDelivery.shown ? t('系统已接收通知，后续操作失败', 'Accepted; a later step failed')
          : currentDelivery.status === 'closed' && !currentDelivery.shown ? t('通知已关闭，接收未确认', 'Closed; acceptance unconfirmed')
            : DELIVERY[currentDelivery.status][en ? 1 : 0]
        : t('本次连接尚无投递记录', 'No delivery for this connection yet')}>
        {currentDelivery ? <details className="text-[12px] text-cx-fg-3"><summary className="cursor-pointer select-none">{t('完整诊断', 'Full diagnostic')}</summary><pre className="mt-2 whitespace-pre-wrap break-all rounded-lg bg-cx-code p-3 font-cx-mono text-[12px] text-cx-fg-2">{JSON.stringify(currentDelivery, null, 2)}</pre></details> : null}
      </SettingsRow> : null}
    </SettingsSection>
    {permissionNote ? <SettingsNote>{permissionNote}</SettingsNote> : null}
    {permission === 'denied' || prefs.mode === 'off' ? <SettingsNote>{t('不弹通知时，待办仍会出现在侧栏活动视图里。', 'Without notifications, items still appear in the sidebar activity view.')}</SettingsNote> : null}

    <SettingsSection anchor="notifications-quiet" title={t('静默时段', 'Quiet hours')}>
      <SettingsRow title={t('启用静默时段', 'Enable quiet hours')} description={t('时段内不弹通知、不播放提示音，活动视图照常更新。', 'No notifications or sounds during these hours; the activity view still updates.')}
        control={<Switch ariaLabel={t('启用静默时段', 'Enable quiet hours')} checked={prefs.quietHours.enabled} onCheckedChange={enabled => persist({ quietHours: { enabled } })} />} />
      <SettingsRow title={t('时间', 'Hours')} control={<div className="flex items-center gap-2 text-[13px] text-cx-fg-3">
        <input type="time" aria-label={t('开始时间', 'Start')} disabled={!prefs.quietHours.enabled} value={prefs.quietHours.start} onChange={event => persist({ quietHours: { start: event.target.value || '22:00' } })} className="cx-settings-time" />
        <span>{t('至', 'to')}</span>
        <input type="time" aria-label={t('结束时间', 'End')} disabled={!prefs.quietHours.enabled} value={prefs.quietHours.end} onChange={event => persist({ quietHours: { end: event.target.value || '08:00' } })} className="cx-settings-time" />
      </div>} />
    </SettingsSection>
  </>;
}
