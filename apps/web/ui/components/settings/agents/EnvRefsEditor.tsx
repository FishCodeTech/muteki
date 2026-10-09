"use client";

import { Button, IconButton, Input } from "@/components/chat/ui";

export type EnvRefRow = { id: string; name: string; ref: string };

const REF_PREFIXES = ["secret://", "env:"];

export function envRefRows(refs: Record<string, string> | undefined): EnvRefRow[] {
  return Object.entries(refs || {}).map(([name, ref], index) => ({ id: `${index}:${name}`, name, ref }));
}

/** Mirrors the service-side `_normalize_env_refs` rules so errors show before saving. */
export function envRefRowError(row: EnvRefRow): string {
  const name = row.name.trim(), ref = row.ref.trim();
  if (!name && !ref) return "";
  if (!name) return "缺少变量名";
  if (!/^[A-Z0-9_]+$/.test(name) || !/[A-Z]/.test(name)) return "变量名须为大写字母、数字和下划线";
  if (ref && !REF_PREFIXES.some(prefix => ref.startsWith(prefix))) return "只接受 secret:// 或 env: 引用，不接受明文值";
  return "";
}

export function envRefsPayload(rows: EnvRefRow[]): { refs: Record<string, string>; error: string } {
  const refs: Record<string, string> = {};
  for (const row of rows) {
    const error = envRefRowError(row);
    if (error) return { refs, error: `${row.name.trim() || "环境变量"}：${error}` };
    const name = row.name.trim();
    if (!name) continue;
    if (name in refs) return { refs, error: `${name}：变量名重复` };
    refs[name] = row.ref.trim();
  }
  return { refs, error: "" };
}

export function EnvRefsEditor({ rows, onChange }: { rows: EnvRefRow[]; onChange: (rows: EnvRefRow[]) => void }) {
  const update = (id: string, patch: Partial<EnvRefRow>) => onChange(rows.map(row => row.id === id ? { ...row, ...patch } : row));
  return <div className="flex flex-col gap-2" data-testid="runtime-env-refs">
    <div className="flex items-center justify-between gap-3">
      <div className="min-w-0">
        <p className="text-[13px] font-medium text-cx-fg">环境变量</p>
        <p className="mt-0.5 text-[12px] leading-5 text-cx-fg-3">启动该引擎子进程时注入。值必须是引用：<code>env:宿主变量名</code> 或 <code>secret://platform/…</code>；服务端不保存明文值。</p>
      </div>
      <Button size="xs" variant="outline" icon="plus" onClick={() => onChange([...rows, { id: `new:${Date.now().toString(36)}`, name: "", ref: "" }])}>添加</Button>
    </div>
    {rows.map(row => {
      const error = envRefRowError(row);
      return <div key={row.id} className="flex flex-col gap-1">
        <div className="flex items-center gap-2">
          <Input size="sm" className="w-[38%] font-cx-mono" placeholder="VARIABLE_NAME" aria-label="变量名" value={row.name} invalid={Boolean(error)} spellCheck={false} autoComplete="off"
            onChange={event => update(row.id, { name: event.target.value.toUpperCase() })} />
          <span className="text-cx-fg-4">=</span>
          <Input size="sm" className="min-w-0 flex-1 font-cx-mono" placeholder="env:HOST_VAR 或 secret://platform/…" aria-label={`${row.name || "变量"} 的引用`} value={row.ref} invalid={Boolean(error)} spellCheck={false} autoComplete="off"
            onChange={event => update(row.id, { ref: event.target.value })} />
          <IconButton size="sm" icon="trash" label={`删除 ${row.name || "该行"}`} onClick={() => onChange(rows.filter(item => item.id !== row.id))} />
        </div>
        {error ? <p className="text-[12px] text-cx-danger" role="alert">{error}</p> : null}
      </div>;
    })}
  </div>;
}
