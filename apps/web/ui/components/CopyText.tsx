"use client";

import type { ReactNode } from "react";
import type { MouseEvent } from "react";
import { useT } from "@/lib/i18n";
import { useCopied } from "@/lib/useCopied";
import { Icon } from "@/components/Icon";

/**
 * A click-to-copy wrapper for a prominent value (a flag, a goal-met answer).
 * Capturing the flag is THE final operator action in a CTF, so every place the
 * flag is shown should copy on click instead of forcing a manual select.
 *
 * Renders a real <button> (keyboard-activatable, role/aria for screen readers)
 * that copies `value` and briefly swaps its content to a check + "已复制",
 * mirroring the worker-session chips (`.iwk-sess` / `.wlane-sess`). The button
 * is presentationally transparent — the caller keeps the existing flag styling
 * via `className` (e.g. `.insp-run-flag`, `.ans-flag`), and `.copytext` only
 * adds the copy affordance (hover icon, cursor, success colour).
 *
 *   <CopyText value={flag} className="insp-run-flag" ariaLabelKey="common.copyFlagAria">
 *     {flag}
 *   </CopyText>
 *
 * `children` defaults to `value` when omitted, so a bare flag string is enough.
 */
export function CopyText({
  value,
  children,
  className = "",
  titleKey = "common.copyFlag",
  ariaLabelKey = "common.copyFlagAria",
}: {
  value: string;
  children?: ReactNode;
  className?: string;
  titleKey?: string;
  ariaLabelKey?: string;
}) {
  const t = useT();
  const [copied, copy] = useCopied();
  const onCopy = (event: MouseEvent<HTMLButtonElement>) => {
    event.stopPropagation();
    copy(value);
  };
  return (
    <button
      type="button"
      className={`copytext ${className} ${copied ? "copied" : ""}`.trim()}
      title={t(titleKey)}
      aria-label={t(ariaLabelKey, { flag: value, text: value })}
      onClick={onCopy}
    >
      <span className="copytext-val">{children ?? value}</span>
      <span className="copytext-aff" aria-hidden="true">
        {copied
          ? <><Icon name="check" size={13} /> {t("common.copied")}</>
          : <><Icon name="copy" size={13} /> {t("common.copyShort")}</>}
      </span>
    </button>
  );
}

export default CopyText;
