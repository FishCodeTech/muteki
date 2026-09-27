import { Icon, type IconName } from "@/components/Icon";

/** Both shapes retain the same slot so swapping an icon never shifts its label. */
export function MotionIcon({ active, from, to, size = 16 }: {
  active: boolean; from: IconName; to: IconName; size?: number;
}) {
  return (
    <span className="t-icon-swap" data-state={active ? "b" : "a"} aria-hidden="true">
      <span className="t-icon" data-icon="a"><Icon name={from} size={size} /></span>
      <span className="t-icon" data-icon="b"><Icon name={to} size={size} /></span>
    </span>
  );
}
