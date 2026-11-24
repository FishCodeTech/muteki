"use client";

import { Fragment } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@muteki/ui";
import { ScrollArea } from "@muteki/ui";

export function PathBreadcrumb({
  path,
  onSelect,
  rootLabel = "工作区",
  lastIsFile = false,
  className,
}: {
  path: string;
  onSelect?: (dirPath: string) => void;
  rootLabel?: string;
  lastIsFile?: boolean;
  className?: string;
}) {
  const segments = path.split("/").filter(Boolean);
  const crumb = (label: string, target: string, current: boolean, key: string) => {
    const clickable = Boolean(onSelect) && !(current && lastIsFile);
    return (
      <button
        key={key}
        type="button"
        disabled={!clickable}
        onClick={() => onSelect?.(target)}
        title={target || rootLabel}
        className={cn(
          "inline-flex h-7 max-w-[200px] shrink-0 items-center truncate rounded-lg px-1.5 text-cx-sm outline-none transition-colors",
          current ? "font-medium text-cx-fg" : "text-cx-fg-3",
          clickable && "hover:bg-cx-hover hover:text-cx-fg focus-visible:bg-cx-hover",
          !clickable && "cursor-default",
        )}
      >
        <span className="truncate">{label}</span>
      </button>
    );
  };
  return (
    <ScrollArea axis="x" className={cn("min-w-0 flex-1", className)}>
      <nav aria-label="路径" className="flex w-max items-center">
        {crumb(rootLabel, "", segments.length === 0, "root")}
        {segments.map((segment, index) => (
          <Fragment key={`${segment}-${index}`}>
            <Icon name="chevronRight" size={12} className="shrink-0 text-cx-fg-4" />
            {crumb(segment, segments.slice(0, index + 1).join("/"), index === segments.length - 1, `seg-${index}`)}
          </Fragment>
        ))}
      </nav>
    </ScrollArea>
  );
}
