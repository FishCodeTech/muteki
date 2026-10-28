"use client";

import { useEffect, useRef, useState } from "react";
import { Button, Collapse, IconButton, TextField } from "@/components/chat/ui";
import { Icon } from "@/components/Icon";
import { ModelTestTerminal } from "@/components/ModelTestTerminal";
import { cn } from "@/lib/cn";
import type { GlobalCredential, WorkerModelTestResult } from "@/lib/useRun";
import { credentialName, formatTestTime } from "./shared";

/** Brings a panel that just opened into view; panels open inside long, scrolling lists. */
function useReveal() {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => { ref.current?.scrollIntoView({ block: "nearest", behavior: "smooth" }); }, []);
  return ref;
}

/** In-card confirmation for deleting a stored credential. */
export function DeleteConfirm({ credential, busy, error, onCancel, onConfirm }: {
  credential: GlobalCredential; busy: boolean; error: string; onCancel: () => void; onConfirm: () => void;
}) {
  const usage = credential.usage || [];
  const reveal = useReveal();
  return (
    <div ref={reveal} role="alertdialog" aria-label="确认删除接入方式" className="flex flex-col gap-3 border-b border-cx-border-subtle bg-cx-danger-soft/40 px-4 py-3.5">
      <p className="flex items-start gap-2 text-[13px] text-cx-fg">
        <Icon name="trash" size={15} className="mt-0.5 shrink-0 text-cx-danger" />
        <span>删除 <b className="font-medium">{credentialName(credential)}</b>？不会删除对应的 Agent 引擎。</span>
      </p>
      <p className="text-[12.5px] leading-5 text-cx-fg-3">
        {usage.length ? `将同步解除以下配置引用：${usage.map(item => item.label || item.id).join("、")}。关联对话和产物会保留，相关 Worker 会停用，系统角色会恢复默认接入。` : "当前没有配置引用，可以直接删除。"}
      </p>
      {error ? <p role="alert" className="rounded-lg bg-cx-danger-soft px-3 py-2 text-[12.5px] text-cx-danger">{error}</p> : null}
      <div className="flex justify-end gap-2">
        <Button size="sm" variant="ghost" disabled={busy} onClick={onCancel}>取消</Button>
        <Button size="sm" variant="danger" loading={busy} onClick={onConfirm}>{usage.length ? `解除 ${usage.length} 处引用并删除` : "确认删除"}</Button>
      </div>
    </div>
  );
}

/** In-card form that copies the host login into a standalone credential (container Workers cannot read the host login). */
export function ImportRow({ initialId, busy, error, onCancel, onImport }: {
  initialId: string; busy: boolean; error: string; onCancel: () => void; onImport: (accountId: string) => void;
}) {
  const [accountId, setAccountId] = useState(initialId);
  const reveal = useReveal();
  return (
    <div ref={reveal} role="group" aria-label="导入为独立凭据" className="flex flex-col gap-2 border-b border-cx-border-subtle bg-cx-bg-subtle px-4 py-3.5">
      <p className="text-[12.5px] text-cx-fg-3">容器 Worker 无法读取宿主登录，需要复制为独立凭据后使用。</p>
      <div className="flex flex-wrap items-end gap-2">
        <TextField label="导入后的凭据 ID" autoComplete="off" value={accountId} onChange={event => setAccountId(event.target.value)} containerClassName="min-w-[200px] flex-1" />
        <Button size="sm" variant="ghost" disabled={busy} onClick={onCancel}>取消</Button>
        <Button size="sm" variant="primary" icon="upload" loading={busy} disabled={!accountId.trim()} onClick={() => onImport(accountId.trim())}>导入</Button>
      </div>
      {error ? <p role="alert" className="text-[12.5px] text-cx-danger">{error}</p> : null}
    </div>
  );
}

/** Result of the real connectivity test, as a summary line with the terminal log one click away. */
export function TestSummary({ result, testing, onClose }: { result: WorkerModelTestResult | null; testing: boolean; onClose: () => void }) {
  const [open, setOpen] = useState(false);
  const failed = Boolean(result && !result.ok);
  const reveal = useReveal();
  const showLog = open || failed;
  const meta = result ? [result.model, result.backend === "container" ? "容器环境" : "本地环境", formatTestTime(result.tested_at)].filter(Boolean).join(" · ") : "";
  return (
    <div ref={reveal} role="status" aria-live="polite" className="border-b border-cx-border-subtle bg-cx-bg-subtle">
      <div className="flex items-center gap-2 px-4 py-2.5">
        <Icon name={testing ? "loader" : result?.ok ? "checkCircle" : "alert"} size={15}
          className={cn("shrink-0", testing ? "cx-spin text-cx-fg-3" : result?.ok ? "text-cx-success" : "text-cx-danger")} />
        <span className="min-w-0 flex-1 truncate text-[12.5px] text-cx-fg">
          {testing ? "正在向模型发起真实请求…" : <><b className="font-medium">{result?.ok ? "连通可用" : "连通失败"}</b>{meta ? <span className="text-cx-fg-3"> · {meta}</span> : null}</>}
        </span>
        {!testing && result ? <Button size="xs" variant="ghost" onClick={() => setOpen(value => !value)} aria-expanded={showLog}>{showLog ? "收起日志" : "查看日志"}</Button> : null}
        <IconButton icon="x" label="关闭测试结果" disabled={testing} onClick={onClose} />
      </div>
      <Collapse open={showLog || testing}>
        <div className="px-4 pb-3"><ModelTestTerminal testing={testing} result={result} title="连通测试终端" /></div>
      </Collapse>
    </div>
  );
}
