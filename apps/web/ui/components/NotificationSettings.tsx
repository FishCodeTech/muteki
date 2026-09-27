"use client";

import { useCallback, useEffect, useState } from "react";
import { Button, Radio, RadioGroup } from "@heroui/react";
import { Icon } from "@/components/Icon";
import { useT } from "@/lib/i18n";
import {
  defaultNotificationPrefs,
  readNotificationPrefs,
  requestNotificationPermission,
  unlockNotificationAudio,
  writeNotificationPrefs,
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

export function NotificationSettings() {
  const t = useT();
  const [prefs, setPrefs] = useState<NotificationPrefs>(() => defaultNotificationPrefs());
  const [permission, setPermission] = useState<NotificationPermission | "unsupported">("default");
  const [permissionNote, setPermissionNote] = useState("");

  useEffect(() => {
    setPrefs(readNotificationPrefs());
    if (typeof window === "undefined" || !("Notification" in window)) {
      setPermission("unsupported");
      return;
    }
    setPermission(Notification.permission);
  }, []);

  const persist = useCallback((next: NotificationPrefs) => {
    setPrefs(next);
    writeNotificationPrefs(next);
  }, []);

  const onModeChange = useCallback((value: string | number | null) => {
    const mode = String(value || "off") as NotificationMode;
    unlockNotificationAudio();
    persist({ ...prefs, mode });
  }, [persist, prefs]);

  const onRequestPermission = useCallback(async () => {
    unlockNotificationAudio();
    const next = await requestNotificationPermission();
    setPermission(next);
    if (next === "denied") {
      setPermissionNote(t("settingsHub.notifications.permissionDenied"));
    } else if (next === "granted") {
      setPermissionNote(t("settingsHub.notifications.permissionGranted"));
    } else if (next === "unsupported") {
      setPermissionNote(t("settingsHub.notifications.permissionUnsupported"));
    } else {
      setPermissionNote("");
    }
  }, [t]);

  return (
    <div className="wsettings-simple-page wnotifications-page" data-page="notifications">
      <header className="wsettings-section-head">
        <div className="wsettings-section-copy">
          <h2>{t("settingsHub.notifications")}</h2>
          <p>{t("settingsHub.notificationsDesc")}</p>
        </div>
      </header>

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
              ? t("settingsHub.notifications.permissionGrantedShort")
              : permission === "denied"
                ? t("settingsHub.notifications.permissionDeniedShort")
                : permission === "unsupported"
                  ? t("settingsHub.notifications.permissionUnsupportedShort")
                  : t("settingsHub.notifications.permissionDefaultShort")}
          </span>
        </header>
        <p>{t("settingsHub.notifications.permissionHint")}</p>
        <div className="wnotifications-actions">
          <Button
            type="button"
            variant="primary"
            isDisabled={permission === "granted" || permission === "unsupported"}
            onPress={() => void onRequestPermission()}
          >
            <Icon name="bell" size={14} />
            {t("settingsHub.notifications.requestPermission")}
          </Button>
        </div>
        {permissionNote ? <p className="wsettings-inline-note"><Icon name="info" size={13} />{permissionNote}</p> : null}
        {permission === "denied" || prefs.mode === "off" ? (
          <p className="wsettings-inline-note">
            <Icon name="info" size={13} />
            {t("settingsHub.notifications.inAppFallback")}
          </p>
        ) : null}
      </section>

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
              ...prefs,
              quietHours: { ...prefs.quietHours, enabled: event.target.checked },
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
                ...prefs,
                quietHours: { ...prefs.quietHours, start: event.target.value || "22:00" },
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
                ...prefs,
                quietHours: { ...prefs.quietHours, end: event.target.value || "08:00" },
              })}
            />
          </label>
        </div>
      </section>
    </div>
  );
}
