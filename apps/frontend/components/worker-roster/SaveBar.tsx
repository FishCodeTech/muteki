"use client";

import { useEffect, useState, type ReactNode } from "react";
import { AnimatePresence, motion } from "motion/react";
import { Icon } from "@muteki/ui";
import { EASE_OUT } from "@muteki/ui";
import { Button, Shortcut } from "@muteki/ui";
import { usePrefersReducedMotion } from "@/lib/usePrefersReducedMotion";

export function SaveBar({
  dirty, saving, failed, notice, onSave, onDiscard,
}: {
  dirty: boolean;
  saving: boolean;
  failed: boolean;
  notice?: ReactNode;
  onSave: () => void;
  onDiscard: () => void;
}) {
  const reduced = usePrefersReducedMotion();
  const [confirming, setConfirming] = useState(false);
  useEffect(() => {
    if (!confirming) return;
    const timer = window.setTimeout(() => setConfirming(false), 3000);
    return () => window.clearTimeout(timer);
  }, [confirming]);
  useEffect(() => { if (!dirty) setConfirming(false); }, [dirty]);

  return (
    <div className="pointer-events-none sticky bottom-[-32px] z-20 mt-auto -mb-8 mx-auto flex w-fit max-w-full flex-col items-center gap-2 px-4">
      <AnimatePresence>
        {notice ? (
          <motion.div
            key="notice"
            initial={reduced ? { opacity: 0 } : { opacity: 0, transform: "translateY(6px)" }}
            animate={reduced ? { opacity: 1 } : { opacity: 1, transform: "translateY(0px)" }}
            exit={reduced ? { opacity: 0 } : { opacity: 0, transform: "translateY(6px)" }}
            transition={reduced ? { duration: 0.14 } : { duration: 0.16, ease: EASE_OUT }}
            role="status"
            className="pointer-events-auto flex max-w-[min(560px,100%)] items-center gap-2 rounded-lg ring-1 ring-cx-ring bg-cx-overlay px-3 py-2 text-cx-xs text-cx-fg shadow-cx-md"
          >
            <Icon name={failed ? "alert" : "check"} size={14} className={failed ? "text-cx-danger" : "text-cx-success"} />
            <span className="min-w-0">{notice}</span>
          </motion.div>
        ) : null}
        {dirty ? (
          <motion.div
            key="bar"
            initial={reduced ? { opacity: 0 } : { opacity: 0, transform: "translateY(12px)" }}
            animate={reduced ? { opacity: 1 } : { opacity: 1, transform: "translateY(0px)" }}
            exit={reduced ? { opacity: 0 } : { opacity: 0, transform: "translateY(12px)" }}
            transition={reduced ? { duration: 0.14 } : { duration: 0.18, ease: EASE_OUT }}
            className="pointer-events-auto flex items-center gap-3 rounded-xl bg-cx-overlay py-2 pl-4 pr-2 shadow-cx-pop"
          >
            <span className="flex items-center gap-2 text-cx-sm text-cx-fg-2">
              <span className="size-1.5 rounded-full bg-cx-warning" />
              有未保存的修改
            </span>
            <div className="flex items-center gap-1.5">
              <Button
                size="sm"
                variant={confirming ? "danger-soft" : "ghost"}
                disabled={saving}
                onClick={() => (confirming ? (setConfirming(false), onDiscard()) : setConfirming(true))}
              >
                {confirming ? "确认放弃？" : "放弃"}
              </Button>
              <Button size="sm" variant="primary" loading={saving} onClick={onSave}>
                {saving ? "保存中…" : "保存配置"}
                {saving ? null : <Shortcut keys="mod+s" tone="inverse" className="ml-0.5" />}
              </Button>
            </div>
          </motion.div>
        ) : null}
      </AnimatePresence>
    </div>
  );
}
