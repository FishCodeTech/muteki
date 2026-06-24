"use client";

import { GraphNode } from "@/lib/events";
import { useT } from "@/lib/i18n";
import { useCopied } from "@/lib/useCopied";
import { Icon } from "@/components/Icon";

const TYPE_KEY: Record<string, string> = {
  challenge: "insp.challenge",
  solver: "insp.solver",
  fact: "insp.fact",
  candidate: "insp.candidate",
  intent: "insp.intent",
  dead_end: "insp.dead_end",
  flag: "insp.flag",
};

/** Side panel for a clicked graph node — a node-detail drawer, with the
 *  provenance verdict surfaced (verified vs candidate, the verifier, confidence). */
export function NodeInspector({ node, onClose }: { node: GraphNode | null; onClose: () => void }) {
  const t = useT();
  const [copied, copy] = useCopied();
  if (!node) {
    return <div className="inspector empty">{t("insp.clickHint")}</div>;
  }
  const m = node.meta || {};
  // Prefer the untruncated original (m.raw) for the writeup; fall back to the label.
  const copyText = m.raw || node.label || "";
  return (
    <div className={`inspector type-${node.type}`}>
      <div className="insp-head">
        <span className="insp-type">{TYPE_KEY[node.type] ? t(TYPE_KEY[node.type]) : node.type}</span>
        <div className="insp-head-actions">
          {copyText && (
            <button
              type="button"
              className={`insp-copy ${copied ? "copied" : ""}`.trim()}
              onClick={() => copy(copyText)}
              title={t("insp.copy")}
              aria-label={t("insp.copy")}
            >
              <Icon name={copied ? "check" : "copy"} size={13} />
            </button>
          )}
          <button className="insp-close" onClick={onClose} title={t("settings.close")} aria-label={t("settings.close")}><Icon name="x" size={14} /></button>
        </div>
      </div>
      <div className="insp-label">{node.label}</div>
      {m.raw && m.raw !== node.label && (
        <div className="insp-raw">
          <div className="insp-raw-h">{t("insp.raw")}</div>
          <div className="insp-raw-t">{m.raw}</div>
        </div>
      )}
      <dl className="insp-meta">
        {"verified" in m && (
          <>
            <dt>{t("insp.provenance")}</dt>
            <dd className={m.verified ? "ok" : "warn"}>
              {m.verified ? t("insp.verified") : t("insp.unverified")}
            </dd>
          </>
        )}
        {typeof m.confidence === "number" && (
          <>
            <dt>{t("insp.confidence")}</dt>
            <dd>{m.confidence.toFixed(2)}</dd>
          </>
        )}
        {m.verifier && (
          <>
            <dt>{t("insp.verifier")}</dt>
            <dd>{m.verifier}</dd>
          </>
        )}
        {m.workerClass && (
          <>
            <dt>{t("insp.workerClass")}</dt>
            <dd>{m.workerClass}</dd>
          </>
        )}
      </dl>
    </div>
  );
}
