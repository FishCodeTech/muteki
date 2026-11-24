"use client";

import { IconButton, Menu, MenuItem, MenuLabel, MenuSeparator } from "@muteki/ui";
import type { OperatorTriageLabel } from "@/lib/useRun";

type Translate = (k: string, v?: Record<string, string | number>) => string;

const TRIAGE: OperatorTriageLabel[] = ["pending_followup", "processed", "ignored"];

export function RunHeaderMenu({
  t,
  runId,
  started,
  theme,
  operatorLabel,
  onUpdateOperatorLabel,
  onOpenWorkspace,
  onToggleTheme,
  onToggleLang,
  copyText,
}: {
  t: Translate;
  runId: string;
  started: boolean;
  theme: "light" | "dark";
  operatorLabel?: OperatorTriageLabel | null;
  onUpdateOperatorLabel?: (label: OperatorTriageLabel | null) => void;
  onOpenWorkspace: () => void;
  onToggleTheme: () => void;
  onToggleLang: () => void;
  copyText: (text: string) => void;
}) {
  return (
    <Menu
      placement="bottom-end"
      ariaLabel={t("rv.more")}
      className="min-w-[220px]"
      trigger={<IconButton icon="more" label={t("rv.more")} className="rv-more" />}
    >
      {started && runId ? (
        <>
          <MenuItem icon="copy" description={`sessions/${runId}`} onSelect={() => copyText(`sessions/${runId}`)}>
            {t("rv.menu.copyPath")}
          </MenuItem>
          <MenuItem icon="folderOpen" onSelect={onOpenWorkspace}>{t("rv.menu.openFolder")}</MenuItem>
          {onUpdateOperatorLabel ? (
            <>
              <MenuSeparator />
              <MenuLabel>{t("rv.menu.triage")}</MenuLabel>
              {TRIAGE.map((label) => (
                <MenuItem
                  key={label}
                  checked={operatorLabel === label}
                  onSelect={() => onUpdateOperatorLabel(operatorLabel === label ? null : label)}
                >
                  {t(`rv.triage.${label}`)}
                </MenuItem>
              ))}
            </>
          ) : null}
          <MenuSeparator />
        </>
      ) : null}
      <MenuItem icon={theme === "dark" ? "sun" : "moon"} onSelect={onToggleTheme}>
        {t(theme === "dark" ? "theme.toLight" : "theme.toDark")}
      </MenuItem>
      <MenuItem icon="globe" onSelect={onToggleLang}>{t("rv.menu.language")}</MenuItem>
    </Menu>
  );
}
