"use client";

import { useMemo, useState } from "react";
import { Badge, IconButton, SearchInput } from "@/components/chat/ui";
import { Icon } from "@/components/Icon";
import { cn } from "@/lib/cn";
import type { ChatDefaultModel } from "@/lib/conversationDefaults";
import { modelEffortLevels, modelServiceTiers } from "@/lib/modelReasoning";
import { isModelHidden } from "@/lib/modelVisibility";
import type { ConversationCredentialModel } from "@/lib/useConversation";

export interface AgentModelRow {
  key: string;
  credentialId: string;
  credentialName: string;
  model: ConversationCredentialModel;
  verified: boolean;
}

export function ModelDirectory({
  rows,
  chatDefault,
  hidden,
  onSetDefault,
  onToggleHidden,
}: {
  rows: AgentModelRow[];
  chatDefault: ChatDefaultModel | null;
  hidden: ReadonlySet<string>;
  onSetDefault: (credentialId: string, modelId: string) => void;
  onToggleHidden: (credentialId: string, modelId: string, hide: boolean) => void;
}) {
  const [query, setQuery] = useState("");
  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return rows;
    return rows.filter((row) =>
      `${row.model.id} ${row.model.label} ${row.credentialName}`.toLowerCase().includes(needle));
  }, [rows, query]);

  if (!rows.length) {
    return <p className="px-5 py-8 text-center text-[13px] text-cx-fg-3">尚未登记模型。可在凭据里更新候选模型，或重新探测运行环境。</p>;
  }

  return (
    <div>
      {rows.length > 8 ? (
        <div className="border-b border-cx-border-subtle px-4 py-2.5">
          <SearchInput value={query} onValueChange={setQuery} placeholder={`过滤 ${rows.length} 个模型…`} aria-label="过滤模型" />
        </div>
      ) : null}
      {!visible.length ? <p className="px-5 py-6 text-center text-[13px] text-cx-fg-3">没有匹配的模型。</p> : null}
      {visible.map((row) => {
        const isDefault = chatDefault?.credentialId === row.credentialId && chatDefault?.modelId === row.model.id;
        const isHidden = isModelHidden(hidden, row.credentialId, row.model.id);
        const fast = modelServiceTiers(row.model).length > 0;
        const reasoning = modelEffortLevels(row.model).length > 0;
        return (
          <div key={row.key} className={cn("flex items-center gap-2.5 border-b border-cx-border-subtle px-4 py-2.5 last:border-b-0", isHidden && "opacity-55")}>
            <button
              type="button"
              aria-pressed={isDefault}
              title={isDefault ? "新对话默认模型" : "设为新对话默认模型"}
              aria-label={isDefault ? `${row.model.id} 是新对话默认模型` : `将 ${row.model.id} 设为新对话默认模型`}
              onClick={() => onSetDefault(row.credentialId, row.model.id)}
              className={cn(
                "grid size-7 shrink-0 place-items-center rounded-lg outline-none transition-colors focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
                isDefault ? "text-cx-warning" : "text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg",
              )}
            >
              <Icon name="star" size={15} filled={isDefault} />
            </button>
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                <code className="truncate text-[12.5px] text-cx-fg">{row.model.id}</code>
                {row.verified ? <Badge tone="success">已测通</Badge> : null}
                {fast ? <Badge tone="accent" icon="zap">快速</Badge> : null}
                {reasoning ? <Badge tone="accent" icon="brain">推理</Badge> : null}
                {isHidden ? <Badge tone="neutral" icon="eyeOff">选择器中已隐藏</Badge> : null}
              </div>
              <p className="mt-0.5 truncate text-[11.5px] leading-4 text-cx-fg-3">
                {row.model.label !== row.model.id ? `${row.model.label} · ` : ""}{row.credentialName}
              </p>
            </div>
            <IconButton
              icon={isHidden ? "eyeOff" : "eye"}
              label={isHidden ? "在模型选择器中显示" : "在模型选择器中隐藏"}
              onClick={() => onToggleHidden(row.credentialId, row.model.id, !isHidden)}
            />
          </div>
        );
      })}
    </div>
  );
}
