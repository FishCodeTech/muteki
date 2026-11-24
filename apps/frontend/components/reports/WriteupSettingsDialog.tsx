"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import {
  getGenerationSettings,
  putGenerationSettings,
  renderWriteupTemplate,
  type ReportGenerationSettings,
  type WriteupTemplate,
} from "@/lib/reports";
import { reportErrorText } from "@/lib/reportErrors";
import { useT } from "@/lib/i18n";
import {
  Badge,
  Button,
  Callout,
  Dialog,
  SegmentedControl,
  Skeleton,
  TextArea,
  TextField,
  toast,
} from "@muteki/ui";
import { ChatMarkdown } from "@/components/chat/markdown/ChatMarkdown";

type TabKey = "templates" | "prompt";

const TEMPLATE_VARS = [
  { token: "{{title}}", label: "题目名称" },
  { token: "{{category}}", label: "题目分类" },
  { token: "{{target}}", label: "目标 / 附件" },
  { token: "{{flag}}", label: "Flag" },
] as const;

export function WriteupSettingsDialog({
  open,
  initialTab = "templates",
  onClose,
  onSaved,
}: {
  open: boolean;
  initialTab?: TabKey;
  onClose: () => void;
  onSaved?: (settings: ReportGenerationSettings) => void;
}) {
  const t = useT();
  const [tab, setTab] = useState<TabKey>(initialTab);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");

  const [settings, setSettings] = useState<ReportGenerationSettings | null>(null);
  const [templates, setTemplates] = useState<WriteupTemplate[]>([]);
  const [defaultTemplateId, setDefaultTemplateId] = useState("builtin-standard");
  const [selectedId, setSelectedId] = useState("builtin-standard");
  const [writeupPrompt, setWriteupPrompt] = useState("");
  const [maxConcurrent, setMaxConcurrent] = useState(3);
  const [previewMode, setPreviewMode] = useState(false);

  const mdRef = useRef<HTMLTextAreaElement | null>(null);

  useEffect(() => {
    if (!open) return;
    setTab(initialTab);
    setLoading(true);
    setError("");
    setPreviewMode(false);
    void getGenerationSettings()
      .then((data) => {
        setSettings(data);
        setTemplates(data.templates);
        setDefaultTemplateId(data.default_template_id || data.templates[0]?.id || "builtin-standard");
        setSelectedId(data.default_template_id || data.templates[0]?.id || "builtin-standard");
        setWriteupPrompt(data.writeup_prompt);
        setMaxConcurrent(data.max_concurrent);
      })
      .catch((err) => setError(err instanceof Error ? err.message : String(err)))
      .finally(() => setLoading(false));
  }, [open, initialTab]);

  const selected = useMemo(
    () => templates.find((tpl) => tpl.id === selectedId) ?? templates[0] ?? null,
    [templates, selectedId],
  );

  const updateSelected = (patch: Partial<WriteupTemplate>) => {
    if (!selected) return;
    setTemplates((prev) =>
      prev.map((item) => (item.id === selected.id ? { ...item, ...patch } : item)),
    );
  };

  const addCustomTemplate = () => {
    const id = `custom-${Date.now().toString(36)}`;
    const base = selected ?? templates[0];
    const next: WriteupTemplate = {
      id,
      name: `自定义模板 ${templates.filter((x) => !x.builtin).length + 1}`,
      description: "自定义 CTF Writeup 模板",
      markdown:
        base?.markdown ||
        `# {{title}}\n\n- **分类**：{{category}}\n- **Flag**：\`{{flag}}\`\n\n## 解题思路\n\n## 详细分析\n\n## Exp\n\n\`\`\`python\n# Exp\n\`\`\`\n`,
      prompt: "",
      builtin: false,
    };
    setTemplates((prev) => [...prev, next]);
    setSelectedId(id);
    setPreviewMode(false);
  };

  const duplicateSelected = () => {
    if (!selected) return;
    const id = `custom-${Date.now().toString(36)}`;
    const copy: WriteupTemplate = {
      ...selected,
      id,
      name: `${selected.name}（副本）`,
      builtin: false,
    };
    setTemplates((prev) => [...prev, copy]);
    setSelectedId(id);
    setPreviewMode(false);
  };

  const restoreSelectedBuiltin = () => {
    if (!selected?.builtin || !settings) return;
    const orig = settings.builtin_templates.find((b) => b.id === selected.id);
    if (!orig) return;
    updateSelected({
      name: orig.name,
      description: orig.description,
      markdown: orig.markdown,
      prompt: orig.prompt ?? "",
    });
    toast({ tone: "success", title: `已恢复「${orig.name}」默认内容` });
  };

  const removeSelected = () => {
    if (!selected || selected.builtin) return;
    const remaining = templates.filter((item) => item.id !== selected.id);
    setTemplates(remaining);
    if (defaultTemplateId === selected.id) {
      setDefaultTemplateId(remaining[0]?.id || "builtin-standard");
    }
    setSelectedId(remaining[0]?.id || "");
  };

  const insertToken = (token: string) => {
    if (!selected) return;
    const el = mdRef.current;
    if (!el || previewMode) {
      updateSelected({ markdown: `${selected.markdown}${selected.markdown.endsWith("\n") ? "" : "\n"}${token}` });
      return;
    }
    const start = el.selectionStart ?? selected.markdown.length;
    const end = el.selectionEnd ?? selected.markdown.length;
    const next = `${selected.markdown.slice(0, start)}${token}${selected.markdown.slice(end)}`;
    updateSelected({ markdown: next });
    window.requestAnimationFrame(() => {
      el.focus();
      const caret = start + token.length;
      el.setSelectionRange(caret, caret);
    });
  };

  const handleSave = async () => {
    if (saving || loading) return;
    for (const tpl of templates) {
      if (!tpl.name.trim()) {
        setError("模板名称不能为空");
        setTab("templates");
        setSelectedId(tpl.id);
        return;
      }
    }
    setSaving(true);
    setError("");
    try {
      const updated = await putGenerationSettings({
        max_concurrent: maxConcurrent,
        writeup_prompt: writeupPrompt.trim() || null,
        default_template_id: defaultTemplateId,
        templates,
      });
      setSettings(updated);
      onSaved?.(updated);
      toast({ tone: "success", title: "Writeup 模板与 Prompt 设置已保存" });
      onClose();
    } catch (err) {
      setError(reportErrorText(t, err));
    } finally {
      setSaving(false);
    }
  };

  const previewMarkdown = useMemo(() => {
    if (!selected) return "";
    return renderWriteupTemplate(selected.markdown, {
      title: "Example Web Challenge",
      category: "web",
      target: "http://127.0.0.1:8080",
      flag: "flag{example_preview_flag}",
    });
  }, [selected]);

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => !o && !saving && onClose()}
      title="Writeup 模板与生成 Prompt 设置"
      description="自定义多套 CTF Writeup 模板与全局 AI 写作指令；生成和新建 Writeup 时均可直接选用。"
      icon="sliders"
      size="xl"
      dismissable={!saving}
      footer={
        <>
          <Button variant="ghost" onClick={onClose} disabled={saving}>取消</Button>
          <Button variant="primary" icon="check" loading={saving} disabled={loading} onClick={() => void handleSave()}>
            保存设置
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-4">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <SegmentedControl<TabKey>
            value={tab}
            onChange={setTab}
            ariaLabel="Writeup 配置分类"
            options={[
              { value: "templates", icon: "file", label: `模板管理 (${templates.length})` },
              { value: "prompt", icon: "sparkles", label: "全局生成 Prompt 与并发" },
            ]}
          />
          {tab === "templates" && (
            <Button size="sm" variant="outline" icon="plus" onClick={addCustomTemplate} disabled={loading}>
              新建自定义模板
            </Button>
          )}
        </div>

        {error && <Callout tone="danger" role="alert">{error}</Callout>}

        {loading ? (
          <div className="flex flex-col gap-3 py-6">
            <Skeleton className="h-5 w-1/3" />
            <Skeleton className="h-32 w-full" />
            <Skeleton className="h-24 w-2/3" />
          </div>
        ) : tab === "templates" ? (
          <div className="grid min-h-[420px] grid-cols-1 gap-4 md:grid-cols-[230px_minmax(0,1fr)]">
            <div className="cx-scroll flex max-h-[54vh] flex-col gap-1 overflow-y-auto rounded-xl border border-cx-border-subtle bg-cx-bg-subtle p-2">
              {templates.map((tpl) => {
                const active = selected?.id === tpl.id;
                const isDefault = defaultTemplateId === tpl.id;
                return (
                  <button
                    key={tpl.id}
                    type="button"
                    onClick={() => { setSelectedId(tpl.id); setPreviewMode(false); }}
                    className={cn(
                      "flex flex-col gap-1 rounded-lg border px-2.5 py-2 text-left transition-colors",
                      active
                        ? "border-cx-accent-line bg-cx-accent-soft text-cx-fg"
                        : "border-transparent hover:bg-cx-hover text-cx-fg-2",
                    )}
                  >
                    <div className="flex items-center gap-1.5">
                      <span className="min-w-0 flex-1 truncate text-cx-sm font-medium">{tpl.name || "未命名模板"}</span>
                      {isDefault && <Badge tone="accent">默认</Badge>}
                    </div>
                    <div className="flex min-w-0 items-center gap-1.5 text-cx-2xs text-cx-fg-4">
                      <span className="shrink-0 whitespace-nowrap">{tpl.builtin ? "内置模板" : "自定义"}</span>
                      {tpl.description && <span className="min-w-0 truncate">· {tpl.description}</span>}
                    </div>
                  </button>
                );
              })}
            </div>

            {selected ? (
              <div className="flex min-w-0 flex-col gap-3">
                <div className="flex flex-wrap items-center gap-2">
                  <Button
                    size="xs"
                    variant={defaultTemplateId === selected.id ? "soft" : "outline"}
                    icon="check"
                    disabled={defaultTemplateId === selected.id}
                    onClick={() => setDefaultTemplateId(selected.id)}
                  >
                    {defaultTemplateId === selected.id ? "已设为默认模板" : "设为默认模板"}
                  </Button>
                  <Button size="xs" variant="outline" icon="copy" onClick={duplicateSelected}>
                    复制为新模板
                  </Button>
                  {selected.builtin ? (
                    <Button size="xs" variant="ghost" icon="retry" onClick={restoreSelectedBuiltin}>
                      恢复此模板默认
                    </Button>
                  ) : (
                    <Button size="xs" variant="ghost" icon="trash" onClick={removeSelected}>
                      删除模板
                    </Button>
                  )}
                </div>

                <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                  <TextField
                    label="模板名称"
                    value={selected.name}
                    onChange={(e) => updateSelected({ name: e.target.value })}
                    placeholder="例如：比赛交档模板"
                  />
                  <TextField
                    label="简介说明"
                    value={selected.description}
                    onChange={(e) => updateSelected({ description: e.target.value })}
                    placeholder="简述该模板的适用场景"
                  />
                </div>

                <TextArea
                  label="模板专属附加 Prompt（可选）"
                  value={selected.prompt ?? ""}
                  onChange={(e) => updateSelected({ prompt: e.target.value })}
                  rows={2}
                  placeholder="例如：重点保留反汇编关键伪代码，篇幅控制在精简范围内"
                />

                <div className="flex flex-col gap-1.5">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <span className="text-cx-xs font-medium text-cx-fg-2">Markdown 模板骨架</span>
                    <div className="flex flex-wrap items-center gap-1">
                      {TEMPLATE_VARS.map((v) => (
                        <button
                          key={v.token}
                          type="button"
                          onClick={() => insertToken(v.token)}
                          title={`插入变量 ${v.token}（${v.label}）`}
                          className="rounded border border-cx-border-subtle bg-cx-bg-subtle px-1.5 py-0.5 font-cx-mono text-cx-2xs text-cx-fg-3 hover:border-cx-border hover:text-cx-fg"
                        >
                          {v.token}
                        </button>
                      ))}
                      <SegmentedControl<"edit" | "preview">
                        size="xs"
                        value={previewMode ? "preview" : "edit"}
                        onChange={(v) => setPreviewMode(v === "preview")}
                        ariaLabel="模板编辑或预览"
                        options={[
                          { value: "edit", label: "编辑" },
                          { value: "preview", label: "预览" },
                        ]}
                      />
                    </div>
                  </div>
                  {previewMode ? (
                    <div className="cx-scroll h-[230px] overflow-y-auto rounded-lg border border-cx-border bg-cx-bg p-4">
                      <ChatMarkdown text={previewMarkdown} className="report-prose" />
                    </div>
                  ) : (
                    <TextArea
                      ref={mdRef}
                      value={selected.markdown}
                      onChange={(e) => updateSelected({ markdown: e.target.value })}
                      spellCheck={false}
                      aria-label="Markdown 模板骨架"
                      className="h-[230px] font-cx-mono text-cx-xs"
                    />
                  )}
                </div>
              </div>
            ) : null}
          </div>
        ) : (
          <div className="flex flex-col gap-4">
            <div className="flex flex-col gap-1.5">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <span className="text-cx-xs font-medium text-cx-fg-2">
                  全局 Writeup 生成 Prompt（作用于所有 AI 生成的 CTF Writeup）
                </span>
                <Button
                  size="xs"
                  variant="ghost"
                  icon="retry"
                  disabled={!settings || writeupPrompt.trim() === settings.default_writeup_prompt.trim()}
                  onClick={() => settings && setWriteupPrompt(settings.default_writeup_prompt)}
                >
                  恢复系统默认 Prompt
                </Button>
              </div>
              <TextArea
                value={writeupPrompt}
                onChange={(e) => setWriteupPrompt(e.target.value)}
                rows={12}
                spellCheck={false}
                placeholder="例如：口吻专业克制，每个章节先给结论再给证据"
                aria-label="全局 Writeup 生成 Prompt"
                className="font-cx-mono text-cx-xs"
              />
              <p className="text-cx-xs text-cx-fg-4">
                生成时会将「全局生成 Prompt」+「所选模板的附加 Prompt」+「模板 Markdown 骨架」一并提供给模型。清空后保存将自动恢复系统内置默认 Prompt。
              </p>
            </div>

            <div className="flex flex-wrap items-center justify-between gap-3 rounded-xl border border-cx-border-subtle bg-cx-bg-subtle px-4 py-3">
              <div>
                <div className="text-cx-sm font-medium text-cx-fg">批量生成同时并发数</div>
                <div className="text-cx-xs text-cx-fg-3">超出并发上限的 Writeup 任务会自动排队依次生成（1 ~ 8）</div>
              </div>
              <div className="flex items-center gap-2">
                <Button
                  size="xs"
                  variant="outline"
                  disabled={maxConcurrent <= 1}
                  aria-label="减少并发数"
                  onClick={() => setMaxConcurrent((n) => Math.max(1, n - 1))}
                >
                  −
                </Button>
                <span className="w-6 text-center font-medium tabular-nums text-cx-sm">{maxConcurrent}</span>
                <Button
                  size="xs"
                  variant="outline"
                  disabled={maxConcurrent >= 8}
                  aria-label="增加并发数"
                  onClick={() => setMaxConcurrent((n) => Math.min(8, n + 1))}
                >
                  +
                </Button>
              </div>
            </div>
          </div>
        )}
      </div>
    </Dialog>
  );
}
