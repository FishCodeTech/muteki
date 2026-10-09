"use client";

import { useEffect, useState } from "react";
import { Button, Input, Switch } from "@/components/chat/ui";
import { SettingsNote, SettingsPage, SettingsRow, SettingsSection } from "@/components/settings/primitives";
import { apiFetch } from "@/lib/useRun";

type Mode = "ctf" | "pentest";
type Policy = { archive_enabled: boolean; archive_after_days: number; delete_enabled: boolean; delete_after_days: number };
const initialPolicy: Policy = { archive_enabled: false, archive_after_days: 15, delete_enabled: false, delete_after_days: 30 };

export function RetentionSettings({ mode }: { mode: Mode }) {
  const [policy, setPolicy] = useState<Policy>(initialPolicy);
  const [days, setDays] = useState({ archive: "15", delete: "30" });
  const [loading, setLoading] = useState(true);
  const [loadFailed, setLoadFailed] = useState(false);
  const [loadRevision, setLoadRevision] = useState(0);
  const [saving, setSaving] = useState(false);
  const [feedback, setFeedback] = useState("");
  const [error, setError] = useState("");

  useEffect(() => {
    let active = true;
    setLoading(true);
    setLoadFailed(false);
    setError("");
    void apiFetch("/api/settings/run-retention").then(async (response) => {
      if (!response.ok) throw new Error(`读取设置失败（${response.status}）`);
      const data = await response.json() as { policies: Record<Mode, Policy> };
      if (!active) return;
      const value = data.policies[mode];
      setPolicy(value);
      setDays({ archive: String(value.archive_after_days), delete: String(value.delete_after_days) });
    }).catch((cause) => { if (active) { setLoadFailed(true); setError(cause instanceof Error ? cause.message : "读取保留设置失败"); } })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [mode, loadRevision]);

  const setDaysValue = (key: "archive" | "delete", value: string) => {
    setDays((current) => ({ ...current, [key]: value }));
    setError("");
    const parsed = Number(value);
    if (value !== "" && Number.isInteger(parsed) && parsed > 0) {
      setPolicy((current) => ({ ...current, [key === "archive" ? "archive_after_days" : "delete_after_days"]: parsed }));
    }
    setFeedback("");
  };

  const save = async () => {
    const archiveDays = Number(days.archive);
    const deleteDays = Number(days.delete);
    if (!Number.isInteger(archiveDays) || archiveDays < 1 || archiveDays > 3650 || !Number.isInteger(deleteDays) || deleteDays < 1 || deleteDays > 3650) {
      setError("天数必须是 1 到 3650 之间的整数。");
      return;
    }
    if (policy.archive_enabled && policy.delete_enabled && deleteDays <= archiveDays) {
      setError("同时启用归档和删除时，删除天数必须大于归档天数。");
      return;
    }
    const next = { ...policy, archive_after_days: archiveDays, delete_after_days: deleteDays };
    setSaving(true); setError(""); setFeedback("");
    try {
      const response = await apiFetch(`/api/settings/run-retention/${mode}`, {
        method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify(next),
      });
      if (!response.ok) throw new Error(`保存失败（${response.status}）`);
      const data = await response.json() as { policy: Policy };
      setPolicy(data.policy);
      setDays({ archive: String(data.policy.archive_after_days), delete: String(data.policy.delete_after_days) });
      setFeedback("保留设置已保存");
    } catch (cause) { setError(cause instanceof Error ? cause.message : "保存失败，请重试。"); }
    finally { setSaving(false); }
  };

  return <div className="cx-root cx-settings-content h-full w-full" aria-busy={loading || saving}>
    <SettingsPage
      title="任务保留"
      description={`配置${mode === "ctf" ? " CTF" : "渗透测试"}任务的自动归档和删除规则。`}
      actions={<>
        {loadFailed ? <Button variant="secondary" onClick={() => setLoadRevision((revision) => revision + 1)}>重试读取</Button> : null}
        <Button variant="primary" loading={saving} disabled={loading || saving || loadFailed} onClick={() => void save()}>保存设置</Button>
      </>}
    >
      <SettingsSection title="归档">
        <SettingsRow
          title="自动归档"
          description="按任务最后活动时间计算。归档后可在对应模式的任务侧栏「查看归档」中恢复，恢复不会继续执行任务。"
          control={<Switch ariaLabel="启用自动归档" checked={policy.archive_enabled} disabled={loading || loadFailed} onCheckedChange={(checked) => { setPolicy((current) => ({ ...current, archive_enabled: checked })); setError(""); setFeedback(""); }} />}
        />
        <SettingsRow
          title="归档期限"
          description="任务最后活动达到此期限后自动归档。"
          control={<div className="flex items-center gap-2"><Input aria-label="自动归档天数" type="number" min="1" max="3650" step="1" className="w-24 text-right tabular-nums" value={days.archive} onChange={(event) => setDaysValue("archive", event.target.value)} disabled={loading || loadFailed} /><span className="text-[13px] text-cx-fg-3">天</span></div>}
        />
      </SettingsSection>
      <SettingsSection title="删除">
        <SettingsRow
          title="自动删除"
          description="仅自动删除已归档任务，按最后活动时间计算。关闭自动归档后，手动归档的任务仍受此规则约束。删除后无法恢复。"
          control={<Switch ariaLabel="启用自动删除" checked={policy.delete_enabled} disabled={loading || loadFailed} onCheckedChange={(checked) => { setPolicy((current) => ({ ...current, delete_enabled: checked })); setError(""); setFeedback(""); }} />}
        />
        <SettingsRow
          title="删除期限"
          description="已归档任务最后活动达到此期限后自动删除。若同时启用归档，删除期限必须更长。"
          control={<div className="flex items-center gap-2"><Input aria-label="自动删除天数" type="number" min="1" max="3650" step="1" className="w-24 text-right tabular-nums" value={days.delete} onChange={(event) => setDaysValue("delete", event.target.value)} disabled={loading || loadFailed} /><span className="text-[13px] text-cx-fg-3">天</span></div>}
        />
      </SettingsSection>
      {error ? <SettingsNote tone="danger">{error}</SettingsNote> : null}
      {feedback ? <SettingsNote>{feedback}</SettingsNote> : null}
    </SettingsPage>
  </div>;
}
