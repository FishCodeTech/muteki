import { cn } from "@/lib/cn";

/**
 * Circular progress. `value` in [0, 1]; `null` renders an indeterminate arc.
 * Colors come from `currentColor` so callers tint via text utilities.
 */
export function ProgressRing({
  value,
  size = 16,
  stroke = 2,
  className,
  trackOpacity = 0.18,
  label,
}: {
  value: number | null;
  size?: number;
  stroke?: number;
  className?: string;
  trackOpacity?: number;
  label?: string;
}) {
  const radius = (size - stroke) / 2;
  const circumference = 2 * Math.PI * radius;
  const clamped = value === null ? 0.28 : Math.max(0, Math.min(1, value));
  return (
    <svg
      width={size}
      height={size}
      viewBox={`0 0 ${size} ${size}`}
      role={label ? "img" : undefined}
      aria-label={label}
      aria-hidden={label ? undefined : true}
      className={cn("shrink-0 -rotate-90", value === null && "cx-spin", className)}
    >
      <circle
        cx={size / 2}
        cy={size / 2}
        r={radius}
        fill="none"
        stroke="currentColor"
        strokeOpacity={trackOpacity}
        strokeWidth={stroke}
      />
      <circle
        cx={size / 2}
        cy={size / 2}
        r={radius}
        fill="none"
        stroke="currentColor"
        strokeWidth={stroke}
        strokeLinecap="round"
        strokeDasharray={circumference}
        strokeDashoffset={circumference * (1 - clamped)}
        className="transition-[stroke-dashoffset] duration-300 ease-cx-out"
      />
    </svg>
  );
}
