"use client";

import { useEffect, useState } from "react";
import { apiFetch } from "@/lib/useRun";
import styles from "./FollowupWorkerSelect.module.css";

type Participant = { worker_id: string; profile_label?: string; engine: string; model?: string; account_id?: string };

export function FollowupWorkerSelect({ runId, value, onChange, disabled = false }: {
  runId: string; value: string; onChange: (workerId: string) => void; disabled?: boolean;
}) {
  const [workers, setWorkers] = useState<Participant[]>([]);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const [retry, setRetry] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    setWorkers([]);
    setError("");
    setLoading(true);
    onChange("");
    const load = async () => {
      try {
        const res = await apiFetch(`/api/runs/${encodeURIComponent(runId)}/followup/workers`, { signal: controller.signal });
        if (!res.ok) throw new Error(`出战 Agent 读取失败 (${res.status})`);
        const data = await res.json();
        if (!controller.signal.aborted) {
          setWorkers(data.workers);
          setError("");
        }
      } catch (err) {
        if (!controller.signal.aborted) setError(err instanceof Error ? err.message : "出战 Agent 读取失败");
      } finally {
        if (!controller.signal.aborted) setLoading(false);
      }
    };
    void load();
    const timer = window.setInterval(() => void load(), 10000);
    return () => { controller.abort(); window.clearInterval(timer); };
  }, [runId, retry, onChange]);
  useEffect(() => {
    if (!workers.some((worker) => worker.worker_id === value)) onChange(workers[0]?.worker_id || "");
  }, [workers, value, onChange]);
  return <div className={styles.field}>
    <label>
      <span>回答 Agent · 本次已出战</span>
      <select aria-label="回答 Agent · 本次已出战" value={value} onChange={(event) => onChange(event.target.value)} disabled={disabled || loading || workers.length === 0}>
        {workers.length === 0 && <option value="">{loading ? "正在读取…" : "尚无已出战 Agent"}</option>}
        {workers.map((worker) => <option key={worker.worker_id} value={worker.worker_id}>
          {worker.profile_label || worker.engine} · {worker.model || worker.engine} · {worker.worker_id}{worker.account_id ? ` · ${worker.account_id}` : ""}
        </option>)}
      </select>
    </label>
    {error && <div role="alert">{error} <button type="button" onClick={() => setRetry((current) => current + 1)}>重试</button></div>}
  </div>;
}
