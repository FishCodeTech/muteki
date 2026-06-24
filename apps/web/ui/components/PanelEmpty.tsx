"use client";

import { Icon, type IconName } from "@/components/Icon";

/**
 * The unified "no data yet" state for every secondary panel + the inspector.
 * A centred muted icon, a short primary line, and a fainter hint line — so an
 * empty panel reads as intentional and calm, not broken. Theme-token-driven;
 * the layout/look lives in `.panel-empty` (globals.css).
 */
export function PanelEmpty({
  icon,
  title,
  hint,
}: {
  icon: IconName;
  title: string;
  hint?: string;
}) {
  return (
    <div className="panel-empty">
      <span className="panel-empty-ico" aria-hidden="true"><Icon name={icon} size={26} /></span>
      <span className="panel-empty-title">{title}</span>
      {hint && <span className="panel-empty-hint">{hint}</span>}
    </div>
  );
}

export default PanelEmpty;
