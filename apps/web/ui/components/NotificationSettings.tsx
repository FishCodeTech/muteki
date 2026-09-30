"use client";

import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react";
import { Button, Radio, RadioGroup } from "@heroui/react";
import { desktopChatBridge } from "@/lib/desktopChatBridge";
import { conversationStorageScope, subscribeConversationStorageScope } from "@/lib/conversationStorageScope";
import { useNativeDesktopState } from "@/lib/nativeDesktop";
import { Icon } from "@/components/Icon";
import { useLang, useT } from "@/lib/i18n";
import {
  defaultNotificationPrefs,
  readNotificationPrefs,
  readNotificationPermissionStatus,
  readNotificationDelivery,
  requestNotificationPermissionStatus,
  unlockNotificationAudio,
  updateNotificationPrefs,
  subscribeNotificationPrefs,
  subscribeNotificationDelivery,
  type NotificationDeliveryDiagnostic,
  type NotificationMode,
  type NotificationPrefs,
} from "@/lib/threadNotifications";

const MODE_OPTIONS: Array<{ value: NotificationMode; labelKey: string; hintKey: string }> = [
  {
    value: "off",
    labelKey: "settingsHub.notifications.mode.off",
    hintKey: "settingsHub.notifications.mode.offHint",
  },
  {
    value: "notifications",
    labelKey: "settingsHub.notifications.mode.notifications",
    hintKey: "settingsHub.notifications.mode.notificationsHint",
  },
  {
    value: "sound",
    labelKey: "settingsHub.notifications.mode.sound",
    hintKey: "settingsHub.notifications.mode.soundHint",
  },
  {
    value: "notifications-and-sound",
    labelKey: "settingsHub.notifications.mode.both",
    hintKey: "settingsHub.notifications.mode.bothHint",
  },
];

const DELIVERY_LABELS: Record<NotificationDeliveryDiagnostic["status"], [string, string]> = {
  submitting: ["正在提交通知", "Submitting notification"],
  submitted: ["已提交到桌面通知", "Submitted to desktop notifications"],
  awaiting_show: ["等待系统显示回执", "Awaiting the system display receipt"],
  shown: ["系统已报告显示", "System reported display"],
  failed: ["通知投递失败", "Notification delivery failed"],
  outcome_unknown: ["通知投递结果未知", "Notification delivery outcome unknown"],
  clicked: ["已点击通知", "Notification clicked"],
  closed: ["通知已关闭", "Notification closed"],
};

export function NotificationSettings({ hideIntro = false }: { hideIntro?: boolean }) {
  const t = useT();
  const { lang } = useLang();
  const label = useCallback((zh: string, en: string) => lang === "en" ? en : zh, [lang]);
  const scope = useSyncExternalStore(subscribeConversationStorageScope, conversationStorageScope, () => "");
  const nativeState = useNativeDesktopState();
  const native = nativeState.desktop;
  const nativeScope = `${nativeState.state?.connectionVersion || 0}:${nativeState.state?.serviceId || ""}:${nativeState.state?.identityId || ""}`;
  const delivery = useSyncExternalStore(subscribeNotificationDelivery, readNotificationDelivery, () => null);
  const currentDelivery = native && delivery
    && (delivery.connectionVersion === undefined || delivery.connectionVersion === nativeState.state?.connectionVersion)
    && (delivery.serviceId === undefined || delivery.serviceId === nativeState.state?.serviceId)
    && (delivery.identityId === undefined || delivery.identityId === nativeState.state?.identityId)
    ? delivery : null;
  const [prefs, setPrefs] = useState<NotificationPrefs>(() => defaultNotificationPrefs());
  const [permission, setPermission] = useState<NotificationPermission | "unsupported">("default");
  const [permissionNote, setPermissionNote] = useState("");
  const [requesting, setRequesting] = useState(false);
  const permissionGeneration = useRef(0);
  const permissionBusy = useRef(false);
  const permissionMounted = useRef(false);
  const [saveError, setSaveError] = useState("");

  useEffect(() => {
    setPrefs(readNotificationPrefs());
  }, []);
  useEffect(() => {
    permissionMounted.current = true;
    const generation = ++permissionGeneration.current;
    permissionBusy.current = false; setRequesting(false);
    setPermission("default"); setPermissionNote("");
    void readNotificationPermissionStatus().then(status => {
      if (!permissionMounted.current || generation !== permissionGeneration.current || scope !== conversationStorageScope()) return;
      setPermission(status.permission);
    }).catch(error => {
      if (permissionMounted.current && generation === permissionGeneration.current && scope === conversationStorageScope()) setPermissionNote(error instanceof Error ? error.message : String(error));
    });
    return () => { permissionMounted.current = false; ++permissionGeneration.current; };
  }, [scope, nativeScope]);

  useEffect(() => subscribeNotificationPrefs(setPrefs), []);
  const persist = useCallback((patch: Parameters<typeof updateNotificationPrefs>[0]) => {
    const result = updateNotificationPrefs(patch);
    setPrefs(result.prefs);
    setSaveError(result.persisted ? "" : `通知设置未保存，当前窗口按新选择生效；重启前请重试。${result.error || ""}`);
  }, []);

  const onModeChange = useCallback((value: string | number | null) => {
    const mode = String(value || "off") as NotificationMode;
    unlockNotificationAudio();
    persist({ mode });
  }, [persist]);

  const onRequestPermission = useCallback(async () => {
    if (permissionBusy.current || !permissionMounted.current || scope !== conversationStorageScope()) return;
    permissionBusy.current = true; setRequesting(true); setPermissionNote("");
    const generation = ++permissionGeneration.current, owner = scope;
    unlockNotificationAudio();
    try {
      const status = await requestNotificationPermissionStatus();
      if (!permissionMounted.current || generation !== permissionGeneration.current || owner !== conversationStorageScope()) return;
      const next = status.permission;
      setPermission(next);
      if (status.host === "desktop-client") {
        setPermissionNote(next === "denied"
          ? label("你未允许此工作台发送通知，本次连接不会重复询问。重新连接后可再次选择；系统权限单独管理。", "You did not allow notifications for this workspace. This connection will not ask again. Reconnect to choose again; system permission is managed separately.")
          : next === "granted" ? label("此工作台已允许发送通知。系统授权状态未知；若收不到通知，请检查系统通知设置。", "This workspace may send notifications. System permission is unknown; check system notification settings if notifications do not arrive.")
            : next === "unsupported" ? t("settingsHub.notifications.permissionUnsupported") : "");
      } else if (next === "denied") setPermissionNote(t("settingsHub.notifications.permissionDenied"));
      else if (next === "granted") setPermissionNote(t("settingsHub.notifications.permissionGranted"));
      else if (next === "unsupported") setPermissionNote(t("settingsHub.notifications.permissionUnsupported"));
    } catch (error) {
      if (permissionMounted.current && generation === permissionGeneration.current && owner === conversationStorageScope()) setPermissionNote(error instanceof Error ? error.message : String(error));
    } finally {
      if (permissionMounted.current && generation === permissionGeneration.current && owner === conversationStorageScope()) { permissionBusy.current = false; setRequesting(false); }
    }
  }, [label, scope, t]);

  return (
    <div className="wsettings-simple-page wnotifications-page" data-page="notifications">
      {hideIntro ? null : (
        <header className="wsettings-section-head">
          <div className="wsettings-section-copy">
            <h2>{t("settingsHub.notifications")}</h2>
            <p>{t("settingsHub.notificationsDesc")}</p>
          </div>
        </header>
      )}

      {saveError ? <p role="alert" className="wsettings-inline-note">{saveError}</p> : null}
      <section className="wappearance-card wappearance-choice-card" aria-labelledby="wnotifications-mode">
        <header>
          <h3 id="wnotifications-mode">{t("settingsHub.notifications.mode")}</h3>
          <span>{t(`settingsHub.notifications.mode.${prefs.mode === "notifications-and-sound" ? "both" : prefs.mode}`)}</span>
        </header>
        <p>{t("settingsHub.notifications.modeHint")}</p>
        <RadioGroup
          orientation="vertical"
          value={prefs.mode}
          onChange={onModeChange}
          className="wnotifications-modes"
          aria-label={t("settingsHub.notifications.mode")}
        >
          {MODE_OPTIONS.map((option) => (
            <Radio
              key={option.value}
              value={option.value}
              className={prefs.mode === option.value ? "on" : ""}
            >
              <Radio.Content>
                <Radio.Control><Radio.Indicator /></Radio.Control>
                <span>
                  <strong>{t(option.labelKey)}</strong>
                  <em>{t(option.hintKey)}</em>
                </span>
              </Radio.Content>
            </Radio>
          ))}
        </RadioGroup>
      </section>

      <section className="wappearance-card" aria-labelledby="wnotifications-permission">
        <header>
          <h3 id="wnotifications-permission">{t("settingsHub.notifications.permission")}</h3>
          <span>
            {permission === "granted"
              ? native ? label("工作台已允许", "Workspace allowed") : t("settingsHub.notifications.permissionGrantedShort")
              : permission === "denied"
                ? native ? label("工作台已拒绝", "Workspace denied") : t("settingsHub.notifications.permissionDeniedShort")
                : permission === "unsupported"
                  ? t("settingsHub.notifications.permissionUnsupportedShort")
                  : native ? label("未请求工作台授权", "Workspace permission not requested") : t("settingsHub.notifications.permissionDefaultShort")}
          </span>
        </header>
        <p>{native ? label("工作台授权与系统通知权限分别管理。当前系统授权状态：未知。", "Workspace consent and system notification permission are separate. Current system permission: unknown.") : t("settingsHub.notifications.permissionHint")}</p>
        <div className="wnotifications-actions">
          <Button
            type="button"
            variant="primary"
            isDisabled={requesting || permission === "granted" || permission === "unsupported" || (native && permission === "denied")}
            onPress={() => void onRequestPermission()}
          >
            <Icon name="bell" size={14} />
            {native ? label("允许此工作台通知", "Allow notifications for this workspace") : t("settingsHub.notifications.requestPermission")}
          </Button>
          {native && desktopChatBridge()?.openPermissionSettings ? <Button type="button" variant="secondary" onPress={() => { const owner = scope; void desktopChatBridge()?.openPermissionSettings?.("notifications").catch(error => { if (permissionMounted.current && owner === conversationStorageScope()) setPermissionNote(error instanceof Error ? error.message : String(error)); }); }}>{label("打开系统通知设置", "Open system notification settings")}</Button> : null}
        </div>
        {permissionNote ? <p className="wsettings-inline-note"><Icon name="info" size={13} />{permissionNote}</p> : null}
        {permission === "denied" || prefs.mode === "off" ? (
          <p className="wsettings-inline-note">
            <Icon name="info" size={13} />
            {t("settingsHub.notifications.inAppFallback")}
          </p>
        ) : null}
      </section>

      {native ? (
        <section className="wappearance-card" aria-labelledby="wnotifications-delivery" data-testid="notification-delivery-diagnostic">
          <header><h3 id="wnotifications-delivery">{label("最近一次通知投递", "Latest notification delivery")}</h3></header>
          <p role={currentDelivery?.status === "failed" ? "alert" : "status"} data-testid="notification-delivery-stage">
            {currentDelivery
              ? currentDelivery.status === "failed" && currentDelivery.shown
                ? label("通知已显示，后续操作失败", "Notification displayed; a later operation failed")
                : currentDelivery.status === "closed" && !currentDelivery.shown
                ? label("通知已关闭，显示未确认", "Notification closed; display unconfirmed")
                : label(...DELIVERY_LABELS[currentDelivery.status])
              : label("尚无本次连接的投递记录", "No delivery recorded for this connection")}
          </p>
          {currentDelivery?.status === "outcome_unknown" ? <p>{label("尚未收到系统显示或失败回执，不会自动重发。", "No system display or failure receipt has arrived. The notification will not be resent automatically.")}</p> : null}
          <p>{label("工作台授权不代表系统通知已显示。展开详情可查看完整错误和投递阶段历史。", "Workspace consent does not confirm system display. Expand the details for complete errors and delivery history.")}</p>
          {currentDelivery ? (
            <details>
              <summary>{label("完整投递诊断", "Complete delivery diagnostic")}</summary>
              <pre className="whitespace-pre-wrap break-all">{JSON.stringify(currentDelivery, null, 2)}</pre>
            </details>
          ) : null}
        </section>
      ) : null}

      <section className="wappearance-card" aria-labelledby="wnotifications-quiet">
        <header>
          <h3 id="wnotifications-quiet">{t("settingsHub.notifications.quietHours")}</h3>
          <span>{prefs.quietHours.enabled ? t("settingsHub.notifications.quietOn") : t("settingsHub.notifications.quietOff")}</span>
        </header>
        <p>{t("settingsHub.notifications.quietHint")}</p>
        <label className="wnotifications-quiet-toggle">
          <input
            type="checkbox"
            checked={prefs.quietHours.enabled}
            onChange={(event) => persist({
              quietHours: { enabled: event.target.checked },
            })}
          />
          <span>{t("settingsHub.notifications.quietEnable")}</span>
        </label>
        <div className="wnotifications-quiet-range">
          <label>
            <span>{t("settingsHub.notifications.quietStart")}</span>
            <input
              type="time"
              value={prefs.quietHours.start}
              disabled={!prefs.quietHours.enabled}
              onChange={(event) => persist({
                quietHours: { start: event.target.value || "22:00" },
              })}
            />
          </label>
          <label>
            <span>{t("settingsHub.notifications.quietEnd")}</span>
            <input
              type="time"
              value={prefs.quietHours.end}
              disabled={!prefs.quietHours.enabled}
              onChange={(event) => persist({
                quietHours: { end: event.target.value || "08:00" },
              })}
            />
          </label>
        </div>
      </section>
    </div>
  );
}
