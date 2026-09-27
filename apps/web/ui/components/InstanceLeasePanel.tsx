"use client";

import { Button } from "@heroui/react";

/**
 * InstanceLeasePanel：动态实例租约面板（任务书 10.6，设计 9.2/14.2）。
 *
 * 每条租约显示完整地址、TTL 倒计时、续租截止时间、平台
 * generation、fencing token、持有方与状态；操作为释放（instance.stop）。
 * 实例申请入口在题目表行操作上（instance.ensure）。
 */

import { useEffect, useState } from "react";
import type { CSSProperties } from "react";
import {
  CompetitionSnapshotView,
  LeaseView,
} from "@/lib/competition-events";

const muted: CSSProperties = { color: "var(--muted)", fontSize: 11 };
const mono: CSSProperties = { fontFamily: "var(--font-mono)", fontSize: 11 };
const btn: CSSProperties = {
  height: 24,
  padding: "0 8px",
  border: "1px solid var(--line2)",
  borderRadius: 7,
  background: "var(--panel2)",
  color: "var(--text)",
  fontSize: 11,
  fontWeight: 650,
  cursor: "pointer",
};

const LEASE_LABELS: Record<string, { label: string; color: string }> = {
  requested: { label: "已申请", color: "var(--blue)" },
  provisioning: { label: "交付中", color: "var(--amber)" },
  active: { label: "活动", color: "var(--green)" },
  renewing: { label: "续租中", color: "var(--amber)" },
  releasing: { label: "释放中", color: "var(--amber)" },
  released: { label: "已释放", color: "var(--muted)" },
  failed: { label: "失败", color: "var(--red)" },
  expired: { label: "已过期", color: "var(--muted)" },
  lost: { label: "已丢失", color: "var(--red)" },
};

function fmtCountdown(iso?: string | null, now = Date.now()): string {
  if (!iso) return "—";
  const left = Math.max(0, Math.floor((Date.parse(iso) - now) / 1000));
  const m = Math.floor(left / 60);
  const s = left % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}

export function InstanceLeasePanel({
  snapshot,
  busy,
  onStop,
}: {
  snapshot: CompetitionSnapshotView | null;
  busy: boolean;
  onStop: (leaseId: string) => void;
}) {
  // TTL 倒计时每秒重渲染。
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);

  const leases: LeaseView[] = [];
  for (const row of snapshot?.challenges ?? []) {
    if (row.active_lease) leases.push(row.active_lease);
  }
  const nameOf = (challengeId: string) => {
    const row = snapshot?.challenges.find(
      (r) => r.challenge.challenge_id === challengeId,
    );
    return row?.current_revision?.name || row?.challenge.name || challengeId;
  };

  if (!leases.length) {
    return (
      <div style={muted}>
        当前没有活动实例租约。在题目表中为题目申请动态实例。
      </div>
    );
  }

  return (
    <div style={{ display: "grid", gap: 6 }}>
      {leases.map((lease) => {
        const meta = LEASE_LABELS[lease.state] ?? {
          label: lease.state,
          color: "var(--muted)",
        };
        const stoppable = ["active", "renewing", "requested", "releasing"].includes(
          lease.state,
        );
        return (
          <div
            key={lease.lease_id}
            style={{
              border: "1px solid var(--line)",
              borderRadius: 10,
              padding: "8px 10px",
              display: "grid",
              gap: 4,
              background: "var(--panel2)",
            }}
          >
            <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
              <span style={{ fontWeight: 700, fontSize: 12, color: "var(--bright)" }}>
                {nameOf(lease.competition_challenge_id)}
              </span>
              <span style={{ color: meta.color, fontSize: 11, fontWeight: 700 }}>
                {meta.label}
              </span>
              <span style={{ ...mono, color: "var(--muted)" }}>
                {lease.address || "地址待交付"}
              </span>
            </div>
            <div style={{ display: "flex", gap: 12, flexWrap: "wrap", ...muted }}>
              <span style={mono} data-tooltip={`到期时间 ${lease.expires_at ?? "—"}`}>
                TTL {fmtCountdown(lease.expires_at, now)}
              </span>
              <span style={mono} data-tooltip={`续租截止 ${lease.renew_deadline_at ?? "—"}`}>
                续租 {fmtCountdown(lease.renew_deadline_at, now)}
              </span>
              <span style={mono}>generation {lease.generation}</span>
              <span style={mono}>fencing {lease.fencing_token}</span>
              {lease.owner ? <span style={mono}>owner {lease.owner}</span> : null}
              {lease.has_credential ? <span>含实例凭据</span> : null}
            </div>
            {lease.last_error ? (
              <div style={{ ...muted, color: "var(--red)" }}>{lease.last_error}</div>
            ) : null}
            <div>
              <Button
                style={btn}
                isDisabled={busy || !stoppable}
                onClick={() => onStop(lease.lease_id)}
              >
                释放实例
              </Button>
            </div>
          </div>
        );
      })}
    </div>
  );
}
