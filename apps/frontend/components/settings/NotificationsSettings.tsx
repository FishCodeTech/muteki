"use client";

import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from 'react';
import { Button, Input, Select, Switch } from "@muteki/ui";
import { cn } from '@/lib/cn';
import { desktopChatBridge } from '@/lib/desktopChatBridge';
import { conversationStorageScope, subscribeConversationStorageScope } from '@/lib/conversationStorageScope';
import { useNativeDesktopState } from '@/lib/nativeDesktop';
import { useLang } from '@/lib/i18n';
import {
  defaultNotificationPrefs, readNotificationDelivery, readNotificationPermissionStatus, readNotificationPrefs,
  requestNotificationPermissionStatus, subscribeNotificationDelivery, subscribeNotificationPrefs, unlockNotificationAudio,
  sendTestNotification, updateNotificationPrefs, type NotificationDeliveryDiagnostic, type NotificationMode, type NotificationPrefs,
} from '@/lib/threadNotifications';
import { SettingsNote, SettingsRow, SettingsSection } from './primitives';

const MODES: Array<{ value: NotificationMode; label: [string, string]; hint: [string, string] }> = [
  { value: 'notifications', label: ['系统通知', 'System notifications'], hint: ['对话完成、需要审批或等待输入时弹出系统通知。', 'Show a desktop notification when a chat finishes, needs approval, or waits for input.'] },
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
  const [testing, setTesting] = useState(false);
  const [testFeedback, setTestFeedback] = useState<{ error: boolean; message: string } | null>(null);
  const [saveError, setSaveError] = useState('');
  const [srcTasks, setSrcTasks] = useState<Array<{ task_id: string; title: string }>>([]);
  const [whTaskId, setWhTaskId] = useState('');
  const [whKind, setWhKind] = useState<'dingtalk' | 'feishu' | 'wecom' | 'telegram' | 'smtp' | 'slack' | 'discord' | 'webhook'>('dingtalk');
  const [whUrl, setWhUrl] = useState('');
  const [whSecret, setWhSecret] = useState('');
  const [whChatId, setWhChatId] = useState('');
  const [whSmtpFrom, setWhSmtpFrom] = useState('');
  const [whSmtpTo, setWhSmtpTo] = useState('');
  const [whSmtpUser, setWhSmtpUser] = useState('');
  const [whChannels, setWhChannels] = useState<Array<Record<string, unknown>>>([]);
  const [whBusy, setWhBusy] = useState(false);
  const [whNotice, setWhNotice] = useState('');
  const generation = useRef(0); const busy = useRef(false); const mounted = useRef(false);

  const loadWhChannels = useCallback(async (tid: string) => {
    if (!tid) return;
    try {
      const resp = await fetch(`/api/src/tasks/${encodeURIComponent(tid)}/queries/src.notify.channel.list`, {
        method: 'POST', headers: { 'Content-Type': 'application/json', 'x-muteki-csrf': '1' }, body: JSON.stringify({}),
      });
      if (!resp.ok) return;
      const d = await resp.json() as { channels?: Array<Record<string, unknown>> };
      setWhChannels(d.channels ?? []);
    } catch {  }
  }, []);

  useEffect(() => {
    void fetch('/api/src/tasks').then((r) => (r.ok ? r.json() : null)).then((d: Array<{ task_id: string; title: string }> | { tasks?: Array<{ task_id: string; title: string }> } | null) => {
      const list = Array.isArray(d) ? d : (d?.tasks ?? []);
      setSrcTasks(list);
      if (list[0]?.task_id) {
        setWhTaskId(list[0].task_id);
        void loadWhChannels(list[0].task_id);
      }
    }).catch(() => {});
  }, [loadWhChannels]);

  useEffect(() => { setPrefs(readNotificationPrefs()); return subscribeNotificationPrefs(setPrefs); }, []);
  useEffect(() => {
    mounted.current = true;
    const gen = generation;
    busy.current = false; setRequesting(false); setTesting(false); setTestFeedback(null); setPermission('default'); setPermissionNote('');
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

  const testNotification = useCallback(async () => {
    if (busy.current || !mounted.current || scope !== conversationStorageScope()) return;
    busy.current = true; setTesting(true); setTestFeedback(null);
    const id = ++generation.current, owner = scope;
    const current = () => mounted.current && id === generation.current && owner === conversationStorageScope();
    try {
      const result = await sendTestNotification();
      if (current()) {
        if (result.permissionStatus) {
          setPermission(result.permissionStatus.permission);
          setSystemPermission(result.permissionStatus.systemPermission || 'unknown');
          setSystemDiagnostic(result.permissionStatus.systemPermissionError || result.permissionStatus.systemNotificationSettings || null);
        }
        setTestFeedback({ error: false, message: result.host === 'desktop-client'
          ? t('测试请求已提交，实际投递状态见“最近一次投递”。', 'Test requested. See Latest delivery for the delivery status.')
          : t('测试通知已提交到浏览器，实际显示由系统决定。', 'Test notification submitted to the browser; the system controls its display.') });
      }
    } catch (error) {
      if (current()) setTestFeedback({ error: true, message: error instanceof Error ? error.message : String(error) });
    } finally {
      if (current()) { busy.current = false; setTesting(false); }
    }
  }, [scope, t]);

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
              <span className="block text-cx-sm font-medium text-cx-fg">{mode.label[en ? 1 : 0]}</span>
              <span className="mt-0.5 block text-cx-sm text-cx-fg-3">{mode.hint[en ? 1 : 0]}</span>
            </span>
          </button>;
        })}
      </div>
    </SettingsSection>

    <SettingsSection anchor="notifications-permission" title={t('系统权限', 'Permission')}>
      <SettingsRow title={t('工作台通知授权', 'Workspace permission')} description={`${permissionLabel} · ${native ? t('工作台授权与系统通知设置分别管理。', 'Workspace consent and system settings are separate.') : t('由浏览器管理，可随时在网站设置中更改。', 'Managed by the browser; change it any time in site settings.')}`}
        control={<div className="flex items-center gap-2">
          {native && bridge?.openPermissionSettings ? <Button variant="ghost" onClick={() => { const owner = scope; void bridge.openPermissionSettings?.('notifications').catch(error => { if (mounted.current && owner === conversationStorageScope()) setPermissionNote(error instanceof Error ? error.message : String(error)); }); }}>{t('系统设置', 'System settings')}</Button> : null}
          <Button variant="secondary" loading={requesting} disabled={requesting || testing || permission === 'unsupported' || (!native && permission === 'granted')} onClick={() => void requestPermission()}>{native && (permission === 'granted' || permission === 'denied') ? t('更改选择', 'Change choice') : t('允许通知', 'Allow')}</Button>
        </div>} />
      {native ? <SettingsRow title={t('系统通知权限', 'System notification permission')} description={systemPermission === 'granted' ? t('系统已允许', 'Allowed by the system') : systemPermission === 'denied' ? t('系统已拒绝，请在系统设置中更改', 'Denied by the system; change it in system settings') : systemPermission === 'default' ? t('系统尚未收到授权选择', 'No system authorization choice yet') : systemPermission === 'provisional' ? t('系统仅允许静默通知', 'Only quiet notifications are allowed') : systemPermission === 'ephemeral' ? t('系统临时允许通知', 'Notifications are temporarily allowed') : t('系统状态未知', 'System status unknown')} >{systemDiagnostic ? <details><summary>{t('系统诊断', 'System diagnostic')}</summary><pre className="whitespace-pre-wrap break-all">{JSON.stringify(systemDiagnostic, null, 2)}</pre></details> : null}</SettingsRow> : null}
      <SettingsRow title={t('测试系统通知', 'Test system notifications')}
        description={t('发送一条测试通知，不受提醒方式和静默时段影响。请先允许此工作台发送通知。', 'Send a test notification regardless of alert style or quiet hours. Allow workspace notifications first.')}
        control={<Button variant="secondary" loading={testing} disabled={testing || requesting || permission !== 'granted'} onClick={() => void testNotification()}>{t('发送测试通知', 'Send test notification')}</Button>} />
      {native ? <SettingsRow title={t('最近一次投递', 'Latest delivery')} description={currentDelivery
        ? currentDelivery.code === 'desktop.notifications_system_denied' ? t('系统未允许通知，本次未投递', 'Denied by the system; not delivered') : currentDelivery.code === 'desktop.notifications_workspace_denied' ? t('当前工作台未允许通知', 'Not allowed by this workspace') : currentDelivery.code === 'desktop.notifications_unsupported' ? t('当前设备不支持通知', 'Notifications are unsupported') : currentDelivery.status === 'failed' && currentDelivery.shown ? t('系统已接收通知，后续操作失败', 'Accepted; a later step failed')
          : currentDelivery.status === 'closed' && !currentDelivery.shown ? t('通知已关闭，接收未确认', 'Closed; acceptance unconfirmed')
            : DELIVERY[currentDelivery.status][en ? 1 : 0]
        : t('本次连接尚无投递记录', 'No delivery for this connection yet')}>
        {currentDelivery ? <details className="text-cx-xs text-cx-fg-3"><summary className="cursor-pointer select-none">{t('完整诊断', 'Full diagnostic')}</summary><pre className="mt-2 whitespace-pre-wrap break-all rounded-lg bg-cx-code p-3 font-cx-mono text-cx-xs text-cx-fg-2">{JSON.stringify(currentDelivery, null, 2)}</pre></details> : null}
      </SettingsRow> : null}
    </SettingsSection>
    {permissionNote ? <SettingsNote>{permissionNote}</SettingsNote> : null}
    {testFeedback ? <SettingsNote tone={testFeedback.error ? 'danger' : 'neutral'}>{testFeedback.message}</SettingsNote> : null}
    {permission === 'denied' || prefs.mode === 'off' ? <SettingsNote>{t('不弹通知时，待办仍会出现在侧栏活动视图里。', 'Without notifications, items still appear in the sidebar activity view.')}</SettingsNote> : null}

    <SettingsSection anchor="notifications-quiet" title={t('静默时段', 'Quiet hours')}>
      <SettingsRow title={t('启用静默时段', 'Enable quiet hours')} description={t('时段内不弹通知、不播放提示音，活动视图照常更新。', 'No notifications or sounds during these hours; the activity view still updates.')}
        control={<Switch ariaLabel={t('启用静默时段', 'Enable quiet hours')} checked={prefs.quietHours.enabled} onCheckedChange={enabled => persist({ quietHours: { enabled } })} />} />
      <SettingsRow title={t('时间', 'Hours')} control={<div className="flex items-center gap-2 text-cx-sm text-cx-fg-3">
        <Input type="time" aria-label={t('开始时间', 'Start')} disabled={!prefs.quietHours.enabled} value={prefs.quietHours.start} onChange={event => persist({ quietHours: { start: event.target.value || '22:00' } })} className="w-28 tabular-nums" />
        <span>{t('至', 'to')}</span>
        <Input type="time" aria-label={t('结束时间', 'End')} disabled={!prefs.quietHours.enabled} value={prefs.quietHours.end} onChange={event => persist({ quietHours: { end: event.target.value || '08:00' } })} className="w-28 tabular-nums" />
      </div>} />
    </SettingsSection>

    <SettingsSection anchor="notifications-src-webhooks" title={t('SRC 8 通道告警通知网关', 'SRC 8-Channel Notification Gateway')}>
      <div className="flex flex-col gap-3 px-5 py-4">
        <div className="grid grid-cols-1 gap-2 sm:grid-cols-2 lg:grid-cols-4">
          <Select<string>
            value={whTaskId}
            onChange={(tid) => { setWhTaskId(tid); void loadWhChannels(tid); }}
            ariaLabel={t('关联 SRC 任务', 'SRC Task')}
            options={srcTasks.length ? srcTasks.map((item) => ({ value: item.task_id, label: item.title || item.task_id })) : [{ value: '', label: t('暂无 SRC 任务', 'No SRC task') }]}
          />
          <Select<'dingtalk' | 'feishu' | 'wecom' | 'telegram' | 'smtp' | 'slack' | 'discord' | 'webhook'>
            value={whKind}
            onChange={(next) => {
              setWhKind(next);
              if (next === 'telegram' && !whUrl.trim()) setWhUrl('https://api.telegram.org');
            }}
            ariaLabel={t('通知渠道', 'Channel')}
            options={[
              { value: 'dingtalk', label: '钉钉 DingTalk' },
              { value: 'feishu', label: '飞书 Feishu' },
              { value: 'wecom', label: '企业微信 WeCom' },
              { value: 'telegram', label: 'Telegram' },
              { value: 'smtp', label: 'SMTP 邮件' },
              { value: 'slack', label: 'Slack' },
              { value: 'discord', label: 'Discord' },
              { value: 'webhook', label: '通用 Webhook' },
            ]}
          />
          <Input
            value={whUrl}
            onChange={(e) => setWhUrl(e.target.value)}
            placeholder={whKind === 'smtp' ? 'smtps://mail.example.com:465' : whKind === 'telegram' ? 'https://api.telegram.org' : 'https://oapi.dingtalk.com/robot/send?...'}
            aria-label={t('通道地址', 'Channel URL')}
          />
          <Input
            type="password"
            value={whSecret}
            onChange={(e) => setWhSecret(e.target.value)}
            placeholder={whKind === 'telegram' ? 'Bot Token' : whKind === 'smtp' ? t('SMTP 密码（可选）', 'SMTP password (optional)') : t('加签密钥 / •••••••• 留空保留', 'HMAC secret / keep masked')}
            aria-label={t('通道密钥', 'Channel secret')}
          />
        </div>
        {whKind === 'telegram' ? (
          <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
            <Input value={whChatId} onChange={(e) => setWhChatId(e.target.value)} placeholder="Telegram Chat ID" aria-label="Telegram Chat ID" />
          </div>
        ) : null}
        {whKind === 'smtp' ? (
          <div className="grid grid-cols-1 gap-2 sm:grid-cols-3">
            <Input value={whSmtpFrom} onChange={(e) => setWhSmtpFrom(e.target.value)} placeholder="From (muteki@example.com)" aria-label={t('发件人', 'SMTP from')} />
            <Input value={whSmtpTo} onChange={(e) => setWhSmtpTo(e.target.value)} placeholder="To (secops@example.com)" aria-label={t('收件人', 'SMTP to')} />
            <Input value={whSmtpUser} onChange={(e) => setWhSmtpUser(e.target.value)} placeholder={t('SMTP 用户名（可选）', 'SMTP username (optional)')} aria-label={t('SMTP 用户名', 'SMTP username')} />
          </div>
        ) : null}
        <div className="flex flex-wrap items-center gap-2">
          <Button
            size="sm"
            variant="primary"
            disabled={!whTaskId || !whUrl.trim() || whBusy}
            onClick={async () => {
              setWhBusy(true);
              setWhNotice('');
              try {
                const payload: Record<string, unknown> = {
                  kind: whKind,
                  url: whUrl.trim(),
                  ...(whSecret.trim() ? { secret: whSecret.trim() } : {}),
                  ...(whKind === 'telegram' && whChatId.trim() ? { chat_id: whChatId.trim() } : {}),
                  ...(whKind === 'smtp' ? {
                    ...(whSmtpFrom.trim() ? { smtp_from: whSmtpFrom.trim() } : {}),
                    ...(whSmtpTo.trim() ? { smtp_to: whSmtpTo.trim() } : {}),
                    ...(whSmtpUser.trim() ? { smtp_username: whSmtpUser.trim() } : {}),
                  } : {}),
                };
                const resp = await fetch(`/api/src/tasks/${encodeURIComponent(whTaskId)}/commands/src.notify.channel.save`, {
                  method: 'POST',
                  headers: { 'Content-Type': 'application/json', 'x-muteki-csrf': '1' },
                  body: JSON.stringify(payload),
                });
                const d = await resp.json().catch(() => ({})) as { state?: string; error?: { message?: string } };
                if (!resp.ok || d.state === 'failed') throw new Error(d.error?.message ?? `HTTP ${resp.status}`);
                setWhUrl('');
                setWhSecret('');
                setWhChatId('');
                await loadWhChannels(whTaskId);
                setWhNotice(t('已保存通道配置', 'Channel saved'));
              } catch (err) {
                setWhNotice(err instanceof Error ? err.message : String(err));
              } finally {
                setWhBusy(false);
              }
            }}
          >
            {t('保存通知通道', 'Save channel')}
          </Button>
          {whNotice ? <span className="text-cx-xs text-cx-fg-3">{whNotice}</span> : null}
        </div>
        {whChannels.map((ch) => (
          <div key={String(ch.channel_id)} className="flex items-center justify-between gap-2 rounded-lg border border-cx-border-subtle px-3 py-2 text-cx-xs">
            <div className="flex min-w-0 flex-1 flex-wrap items-center gap-x-2">
              <span className="shrink-0 font-medium text-cx-fg">{String(ch.kind)} · {String(ch.label || ch.channel_id)}</span>
              <span className="min-w-0 max-w-full truncate font-cx-mono text-cx-fg-3">{String(ch.url)}</span>
              {Number(ch.deferred_count ?? 0) > 0 ? (
                <span className="shrink-0 text-cx-warning">顺延 {String(ch.deferred_count)}</span>
              ) : null}
            </div>
            <div className="flex items-center gap-1.5">
              {Number(ch.deferred_count ?? 0) > 0 ? (
                <Button
                  size="sm"
                  variant="secondary"
                  disabled={whBusy}
                  onClick={async () => {
                    setWhBusy(true);
                    try {
                      const resp = await fetch(`/api/src/tasks/${encodeURIComponent(whTaskId)}/commands/src.notify.flush_carryover`, {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json', 'x-muteki-csrf': '1' },
                        body: JSON.stringify({ channel_id: ch.channel_id }),
                      });
                      const d = await resp.json().catch(() => ({})) as { state?: string; error?: { message?: string }; output?: { flushed_count?: number }; result?: { flushed_count?: number } };
                      if (!resp.ok || d.state === 'failed') throw new Error(d.error?.message ?? `HTTP ${resp.status}`);
                      const out = d.output ?? d.result;
                      await loadWhChannels(whTaskId);
                      setWhNotice(`已补发 ${String(out?.flushed_count ?? 0)} 条`);
                    } catch (err) {
                      setWhNotice(err instanceof Error ? err.message : String(err));
                    } finally {
                      setWhBusy(false);
                    }
                  }}
                >
                  补发
                </Button>
              ) : null}
              <Button
                size="sm"
                variant="secondary"
                disabled={whBusy}
                onClick={async () => {
                  setWhBusy(true);
                  setWhNotice('');
                  try {
                    const resp = await fetch(`/api/src/tasks/${encodeURIComponent(whTaskId)}/commands/src.notify.test`, {
                      method: 'POST',
                      headers: { 'Content-Type': 'application/json', 'x-muteki-csrf': '1' },
                      body: JSON.stringify({ channel_id: ch.channel_id }),
                    });
                    const d = await resp.json().catch(() => ({})) as { state?: string; error?: { message?: string }; output?: { status_code?: number }; result?: { status_code?: number } };
                    if (!resp.ok || d.state === 'failed') throw new Error(d.error?.message ?? `HTTP ${resp.status}`);
                    const out = d.output ?? d.result;
                    await loadWhChannels(whTaskId);
                    setWhNotice(`HTTP ${String(out?.status_code ?? 200)}`);
                  } catch (err) {
                    setWhNotice(err instanceof Error ? err.message : String(err));
                  } finally {
                    setWhBusy(false);
                  }
                }}
              >
                {t('测试投递', 'Test')}
              </Button>
            </div>
          </div>
        ))}
      </div>
    </SettingsSection>
  </>;
}
