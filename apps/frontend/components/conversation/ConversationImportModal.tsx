"use client";

import { useLang } from "@/lib/i18n";

import { useCallback, useRef, useState } from "react";
import {
  fetchImportScan,
  applyImport,
  type ProviderSessionScan,
  type ImportApplyResult,
} from "@/lib/useConversation";
import type { ConversationProject } from "@/lib/useConversation";
import { NativePathPicker } from "@/components/NativePathPicker";
import { Badge, Button, Callout, Checkbox, Dialog, Label, SegmentedControl, Select, type Tone } from "@muteki/ui";

type AdapterId = "claude" | "codex";
type Step = "config" | "preview" | "result";

const STATUS_LABEL: Record<string, string> = {
  continuable: "可续聊",
  read_only: "历史副本",
  missing_tools: "缺少工具记录",
};

const STATUS_TONE: Record<string, Tone> = {
  continuable: "success",
  read_only: "neutral",
  missing_tools: "warning",
};

const ADAPTER_OPTIONS: { value: AdapterId; label: string }[] = [
  { value: "claude", label: "Claude (claude-code)" },
  { value: "codex", label: "Codex (openai-codex)" },
];

export interface ConversationImportModalProps {
  projects: ConversationProject[];
  onClose: () => void;
  onImportDone: () => void;
}

export function ConversationImportModal({
  projects,
  onClose,
  onImportDone,
}: ConversationImportModalProps) {
  const { lang } = useLang();
  const [step, setStep] = useState<Step>("config");
  const [adapter, setAdapter] = useState<AdapterId>("claude");
  const [customPath, setCustomPath] = useState("");
  const [projectId, setProjectId] = useState("");
  const [scanning, setScanning] = useState(false);
  const [pickingPath, setPickingPath] = useState(false);
  const [scanError, setScanError] = useState("");
  const [scans, setScans] = useState<ProviderSessionScan[]>([]);
  const [basePath, setBasePath] = useState("");
  const [scanAdapter, setScanAdapter] = useState<AdapterId>("claude");
  const scanGeneration = useRef(0);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [applying, setApplying] = useState(false);
  const [result, setResult] = useState<ImportApplyResult | null>(null);
  const [applyError, setApplyError] = useState("");

  const doScan = useCallback(async () => {
    const generation = ++scanGeneration.current;
    setScanning(true);
    setScanError("");
    try {
      const res = await fetchImportScan({ adapter, path: customPath || undefined, limit: 100 });
      if (generation !== scanGeneration.current) return;
      setScanAdapter(adapter);
      setBasePath(res.base_path);
      if (!res.exists) {
        setScanError(`路径不存在：${res.base_path}`);
        setScans([]);
        return;
      }
      setScans(res.scans);
      setSelected(new Set(res.scans.map((s) => s.session_id)));
      setStep("preview");
    } catch (err) {
      setScanError(err instanceof Error ? err.message : "扫描失败");
    } finally {
      setScanning(false);
    }
  }, [adapter, customPath]);

  const toggleAll = () => {
    if (selected.size === scans.length) {
      setSelected(new Set());
    } else {
      setSelected(new Set(scans.map((s) => s.session_id)));
    }
  };

  const doApply = useCallback(async () => {
    setApplying(true);
    setApplyError("");
    try {
      const res = await applyImport({
        adapter_id: scanAdapter,
        source_path: basePath,
        sessions: [...selected],
        project_id: projectId || undefined,
      });
      setResult(res);
      setStep("result");
      onImportDone();
    } catch (err) {
      setApplyError(err instanceof Error ? err.message : "导入失败");
    } finally {
      setApplying(false);
    }
  }, [scanAdapter, scans, basePath, selected, projectId, onImportDone]);

  const projectOptions = [
    { value: "", label: "不归入项目" },
    ...projects.map((p) => ({ value: p.project_id, label: p.name })),
  ];

  return (
    <Dialog
      open
      onOpenChange={(next) => { if (!next) onClose(); }}
      title="导入 Provider 历史会话"
      description="从当前服务宿主的 Claude / Codex 目录读取并导入"
      icon="download"
      size="lg"
      footer={(
        <>
          {step === "config" && (
            <>
              <Button variant="ghost" onClick={onClose}>取消</Button>
              <Button variant="primary" onClick={doScan} loading={scanning} disabled={pickingPath}>
                {scanning ? "扫描中…" : "扫描历史"}
              </Button>
            </>
          )}
          {step === "preview" && (
            <>
              <Button variant="ghost" onClick={() => { setStep("config"); setScanError(""); }}>返回</Button>
              <Button variant="primary" onClick={doApply} loading={applying} disabled={selected.size === 0}>
                {applying ? "导入中…" : `导入 ${selected.size} 个会话`}
              </Button>
            </>
          )}
          {step === "result" && <Button variant="primary" onClick={onClose}>完成</Button>}
        </>
      )}
    >
      {step === "config" && (
        <div className="flex flex-col gap-4">
          <div>
            <Label>Provider</Label>
            <SegmentedControl<AdapterId>
              size="md"
              ariaLabel="Provider"
              value={adapter}
              options={ADAPTER_OPTIONS.map((option) => ({ ...option, disabled: scanning || applying || pickingPath }))}
              onChange={(next) => { setAdapter(next); setScans([]); setSelected(new Set()); }}
            />
          </div>

          <NativePathPicker
            id="import-path"
            label={lang === "en" ? "History directory path" : "历史目录路径"}
            kind="directory"
            disabled={scanning || applying}
            value={customPath}
            onChange={(path) => { setCustomPath(path); setScans([]); setSelected(new Set()); }}
            onBusyChange={setPickingPath}
            placeholder={lang === "en" ? "Service host history directory; default requires host discovery" : "服务宿主历史目录；留空需服务允许宿主发现"}
          />

          {projects.length > 0 && (
            <div>
              <Label hint="可选">导入到项目</Label>
              <Select<string>
                size="md"
                className="w-full"
                ariaLabel="导入到项目"
                value={projectId}
                options={projectOptions}
                onChange={setProjectId}
              />
            </div>
          )}

          {scanError && <Callout tone="danger" role="alert">{scanError}</Callout>}
        </div>
      )}

      {step === "preview" && (
        <div className="flex flex-col gap-3">
          <div className="flex items-center justify-between gap-3 rounded-xl bg-cx-bg-subtle px-3 py-2.5 text-cx-xs">
            <span className="min-w-0 truncate text-cx-fg-3">
              扫描路径 <code className="font-cx-mono text-cx-fg">{basePath}</code>
            </span>
            <span className="shrink-0 font-medium text-cx-fg">共 {scans.length} 个会话</span>
          </div>

          {scans.length === 0 ? (
            <p className="py-4 text-center text-cx-sm text-cx-fg-3">未找到可导入的会话</p>
          ) : (
            <>
              <div className="flex items-center gap-2 text-cx-xs">
                <Button variant="soft" size="xs" onClick={toggleAll}>
                  {selected.size === scans.length ? "取消全选" : "全选"}
                </Button>
                <span className="cx-tabular text-cx-fg-3">已选 {selected.size} / {scans.length}</span>
              </div>

              <div className="cx-scroll max-h-72 divide-y divide-cx-border-subtle overflow-y-auto rounded-xl border border-cx-border-subtle">
                {scans.map((scan) => (
                  <div key={scan.session_id} className="flex items-start gap-3 px-3 py-2.5 hover:bg-cx-hover">
                    <Checkbox
                      className="min-w-0 flex-1"
                      checked={selected.has(scan.session_id)}
                      onCheckedChange={(checked) => {
                        setSelected((prev) => {
                          const next = new Set(prev);
                          if (checked) next.add(scan.session_id);
                          else next.delete(scan.session_id);
                          return next;
                        });
                      }}
                      label={<span className="block truncate">{scan.title}</span>}
                      description={(
                        <span className="block truncate">
                          {scan.message_count} 条消息
                          {scan.created_at && ` · ${scan.created_at.slice(0, 10)}`}
                          {scan.missing_fields.length > 0 && (
                            <span className="text-cx-warning"> · 缺少：{scan.missing_fields.join("、")}</span>
                          )}
                        </span>
                      )}
                    />
                    <Badge tone={STATUS_TONE[scan.import_status] ?? "neutral"}>
                      {STATUS_LABEL[scan.import_status] || scan.import_status}
                    </Badge>
                  </div>
                ))}
              </div>
            </>
          )}

          {applyError && <Callout tone="danger" role="alert">{applyError}</Callout>}
        </div>
      )}

      {step === "result" && result && (
        <div className="flex flex-col gap-3">
          <div className="grid grid-cols-3 gap-3 text-center">
            {[
              { label: "已导入", count: result.imported.length, color: "text-cx-success" },
              { label: "已跳过（重复）", count: result.skipped.length, color: "text-cx-fg-3" },
              { label: "失败", count: result.failed.length, color: result.failed.length ? "text-cx-danger" : "text-cx-fg-3" },
            ].map(({ label, count, color }) => (
              <div key={label} className="rounded-xl border border-cx-border-subtle bg-cx-bg-subtle py-3">
                <div className={`cx-tabular text-cx-lg font-semibold leading-8 ${color}`}>{count}</div>
                <div className="text-cx-xs text-cx-fg-3">{label}</div>
              </div>
            ))}
          </div>
          {result.failed.length > 0 && (
            <details className="rounded-xl border border-cx-border-subtle bg-cx-danger-soft px-3 py-2.5">
              <summary className="cursor-pointer text-cx-xs text-cx-danger">查看失败详情</summary>
              <ul className="m-0 mt-2 flex list-none flex-col gap-1 p-0">
                {result.failed.map((f) => (
                  <li key={f.session_id} className="text-cx-xs text-cx-fg-2">
                    <code className="font-cx-mono">{f.session_id.slice(0, 12)}</code>：{f.error}
                  </li>
                ))}
              </ul>
            </details>
          )}
          <p className="text-cx-sm leading-5 text-cx-fg-2">
            原始历史文件未被修改。导入内容是历史副本，不会恢复原应用的运行会话，可在对话列表中查看。
          </p>
        </div>
      )}
    </Dialog>
  );
}
