"use client";

import { useCallback, useDeferredValue, useId, useLayoutEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from "react";
import dynamic from "next/dynamic";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { Icon } from "@muteki/ui";
import { Button, IconButton, Menu, MenuItem, MenuSeparator, toast } from "@muteki/ui";
import { desktopChatBridge } from "@/lib/desktopChatBridge";
import { ReportPreviewContent } from "./ReportPreviewContent";
import { ReportNativePreview } from "./ReportNativePreview";
import type { ReportEditorInput } from "./ReportTextEditor";
import type { WriteupTemplate } from "@/lib/reports";
import {
  continueList,
  imageFileToDataUrl,
  inlineReportAssets,
  insertBlock,
  insertCodeBlock,
  insertLink,
  insertRule,
  insertTable,
  newAssetId,
  shiftIndent,
  toggleLinePrefix,
  wrapInline,
  type LineKind,
  type ReportAssets,
} from "@/lib/reportMarkdown";

export type EditorView = "edit" | "split" | "preview";
const ReportTextEditor = dynamic(() => import("./ReportTextEditor").then((module) => module.ReportTextEditor), { ssr: false });
const ISOLATED_PREVIEW_MIN_CHARS = 16 * 1024;

const hasFiles = (event: DragEvent) => event.dataTransfer?.types.includes("Files");
const hasImageFiles = (event: DragEvent) => {
  const items = Array.from(event.dataTransfer?.items ?? []);
  if (items.length) return items.some((item) => item.kind === "file" && item.type.startsWith("image/"));
  return Boolean(hasFiles(event));
};

function ToolbarDivider() {
  return <span aria-hidden className="mx-1 h-4 w-px shrink-0 bg-cx-border-subtle" />;
}

export function MarkdownEditor({
  value,
  onChange,
  assets,
  onAssetsChange,
  onSave,
  view,
  centered = false,
  editorRef,
  extraActions,
  templates,
  onApplyTemplate,
  disabled,
}: {
  value: string;
  onChange: (value: string) => void;
  assets: ReportAssets;
  onAssetsChange: (update: (previous: ReportAssets) => ReportAssets) => void;
  onSave?: () => void;
  view: EditorView;
  centered?: boolean;
  editorRef?: RefObject<ReportEditorInput | null>;
  extraActions?: ReactNode;
  templates?: WriteupTemplate[];
  onApplyTemplate?: (template: WriteupTemplate) => void;
  disabled?: boolean;
}) {
  const t = useT();
  const internalRef = useRef<ReportEditorInput | null>(null);
  const areaRef = editorRef ?? internalRef;
  const previewRef = useRef<HTMLDivElement | null>(null);
  const fileRef = useRef<HTMLInputElement | null>(null);
  const syncing = useRef(false);
  const nativeSurface = useId();
  const sendNativeScroll = useRef<(ratio: number) => void>(() => {});
  const nativeEditorScroll = useRef<number | null>(null);
  const [nativeError, setNativeError] = useState(false);
  const [fileDrag, setFileDrag] = useState(false);
  const [headingOpen, setHeadingOpen] = useState(false);
  const [templateMenuOpen, setTemplateMenuOpen] = useState(false);

  const showEditor = view !== "preview";
  const showPreview = view !== "edit";
  const deferred = useDeferredValue(value);
  const lastSplitPreview = useRef<{ text: string; assets: ReportAssets } | null>(null);
  const renderPreview = showPreview || (
    value === deferred && lastSplitPreview.current?.text === value && lastSplitPreview.current.assets === assets
  );
  useLayoutEffect(() => {
    if (view === "split") {
      lastSplitPreview.current = { text: deferred, assets };
    } else if (view === "preview" || !renderPreview) {
      lastSplitPreview.current = null;
    } else if (previewRef.current) {
      previewRef.current.scrollTop = 0;
    }
  }, [assets, deferred, renderPreview, view]);
  const previewText = useMemo(
    () => renderPreview ? inlineReportAssets(deferred, assets) : "",
    [renderPreview, deferred, assets],
  );
  const nativePreview = renderPreview && previewText.length >= ISOLATED_PREVIEW_MIN_CHARS && Boolean(previewText.trim())
    && !nativeError && Boolean(desktopChatBridge()?.presentReportPreview);

  const withArea = useCallback((action: (el: ReportEditorInput) => void) => {
    const el = areaRef.current;
    if (!el || disabled) return;
    action(el);
  }, [areaRef, disabled]);

  const insertImages = useCallback(async (files: File[]) => {
    const el = areaRef.current;
    if (!el) return;
    for (const file of files) {
      try {
        const url = await imageFileToDataUrl(file);
        if (areaRef.current !== el) return;
        const id = newAssetId();
        const alt = /^image\.(png|jpe?g|webp|gif)$/i.test(file.name) ? t("reports.editor.imageAlt") : file.name.replace(/\.[^.]+$/, "");
        onAssetsChange((previous) => ({ ...previous, [id]: url }));
        insertBlock(el, `![${alt.replace(/[\[\]]/g, "")}][${id}]`);
      } catch (error) {
        toast({ tone: "danger", title: t("reports.imageFailed", { reason: error instanceof Error ? error.message : String(error) }) });
      }
    }
  }, [areaRef, onAssetsChange, t]);

  const onKeyDown = (event: KeyboardEvent) => {
    const el = areaRef.current;
    if (!el) return;
    const mod = event.metaKey || event.ctrlKey;
    const key = event.key.toLowerCase();
    if (event.isComposing || event.keyCode === 229) return;
    if (mod && !event.altKey) {
      const handled = (run: () => void) => { event.preventDefault(); run(); };
      if (key === "s") return handled(() => onSave?.());
      if (key === "b") return handled(() => wrapInline(el, "**", "**", "加粗文本"));
      if (key === "i") return handled(() => wrapInline(el, "*", "*", "斜体文本"));
      if (key === "e") return handled(() => wrapInline(el, "`", "`", "code"));
      if (key === "k") return handled(() => insertLink(el));
      if (key === "x" && event.shiftKey) return handled(() => wrapInline(el, "~~", "~~", "删除线文本"));
      return;
    }
    if (event.key === "Enter" && !event.shiftKey && !event.altKey && continueList(el)) event.preventDefault();
    else if (event.key === "Tab" && !event.altKey && shiftIndent(el, event.shiftKey)) event.preventDefault();
  };

  const onPaste = (event: ClipboardEvent) => {
    const files = Array.from(event.clipboardData?.files ?? []).filter((file) => file.type.startsWith("image/"));
    if (!files.length) return;
    event.preventDefault();
    void insertImages(files);
  };

  const onDropFiles = (event: DragEvent) => {
    setFileDrag(false);
    const files = Array.from(event.dataTransfer?.files ?? []).filter((file) => file.type.startsWith("image/"));
    if (!files.length) return;
    event.preventDefault();
    event.stopPropagation();
    void insertImages(files);
  };

  const syncScroll = (from: HTMLElement, to: HTMLElement | null) => {
    if (!to || view !== "split" || syncing.current) return;
    const max = from.scrollHeight - from.clientHeight;
    const ratio = max > 0 ? from.scrollTop / max : 0;
    syncing.current = true;
    to.scrollTop = ratio * (to.scrollHeight - to.clientHeight);
    window.requestAnimationFrame(() => { syncing.current = false; });
  };

  const scrollEditor = (element: HTMLElement) => {
    if (!nativePreview) {
      syncScroll(element, previewRef.current);
      return;
    }
    if (view !== "split") return;
    if (element.scrollTop === nativeEditorScroll.current) {
      nativeEditorScroll.current = null;
      return;
    }
    nativeEditorScroll.current = null;
    const max = element.scrollHeight - element.clientHeight;
    sendNativeScroll.current(max > 0 ? Math.max(0, Math.min(1, element.scrollTop / max)) : 0);
  };

  const scrollFromNative = (ratio: number) => {
    const element = areaRef.current?.scrollElement;
    if (view !== "split" || !element) return;
    element.scrollTop = ratio * (element.scrollHeight - element.clientHeight);
    nativeEditorScroll.current = element.scrollTop;
  };

  const tools = (
    <>
      <Menu
        open={headingOpen}
        onOpenChange={setHeadingOpen}
        trigger={<IconButton icon="heading" label={t("reports.editor.heading")} size="sm" disabled={disabled} />}
      >
        {(["h1", "h2", "h3"] as const).map((kind) => (
          <MenuItem key={kind} onSelect={() => withArea((el) => toggleLinePrefix(el, kind))}>{t(`reports.editor.${kind}`)}</MenuItem>
        ))}
      </Menu>
      <IconButton icon="bold" label={t("reports.editor.bold")} shortcut="mod+B" size="sm" disabled={disabled} onClick={() => withArea((el) => wrapInline(el, "**", "**", "加粗文本"))} />
      <IconButton icon="italic" label={t("reports.editor.italic")} shortcut="mod+I" size="sm" disabled={disabled} onClick={() => withArea((el) => wrapInline(el, "*", "*", "斜体文本"))} />
      <IconButton icon="code" label={t("reports.editor.inlineCode")} shortcut="mod+E" size="sm" disabled={disabled} onClick={() => withArea((el) => wrapInline(el, "`", "`", "code"))} />
      <ToolbarDivider />
      {([
        ["quote", "quote", "quote"],
        ["bullet", "list", "bullet"],
        ["ordered", "listOrdered", "ordered"],
        ["task", "listChecks", "task"],
      ] as const).map(([kind, icon, label]) => (
        <IconButton key={kind} icon={icon} label={t(`reports.editor.${label}`)} size="sm" disabled={disabled} onClick={() => withArea((el) => toggleLinePrefix(el, kind as LineKind))} />
      ))}
      <ToolbarDivider />
      <IconButton icon="link" label={t("reports.editor.link")} shortcut="mod+K" size="sm" disabled={disabled} onClick={() => withArea(insertLink)} />
      <IconButton icon="image" label={t("reports.editor.image")} size="sm" disabled={disabled} onClick={() => fileRef.current?.click()} />
      <Menu trigger={<IconButton icon="plus" label={t("reports.editor.insert")} size="sm" disabled={disabled} />}>
        <MenuItem icon="table" onSelect={() => withArea(insertTable)}>{t("reports.editor.table")}</MenuItem>
        <MenuItem icon="codeBlock" onSelect={() => withArea(insertCodeBlock)}>{t("reports.editor.codeBlock")}</MenuItem>
        <MenuItem icon="minus" onSelect={() => withArea(insertRule)}>{t("reports.editor.rule")}</MenuItem>
        <MenuSeparator />
        <MenuItem icon="strikethrough" onSelect={() => withArea((el) => wrapInline(el, "~~", "~~", "删除线文本"))}>{t("reports.editor.strike")}</MenuItem>
      </Menu>
      <ToolbarDivider />
      <IconButton icon="undo" label={t("reports.editor.undo")} shortcut="mod+Z" size="sm" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => withArea((el) => el.undo())} />
      <IconButton icon="redo" label={t("reports.editor.redo")} shortcut="mod+shift+Z" size="sm" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => withArea((el) => el.redo())} />
    </>
  );

  return (
    <div className="report-editor @container flex min-h-0 flex-1 flex-col">
      {showEditor && (
        <div className={cn("shrink-0", centered ? "mx-auto w-full max-w-[820px] px-6 sm:px-10" : "px-6")}>
          <div className="cx-no-scrollbar flex items-center gap-1 overflow-x-auto border-b border-cx-border-subtle py-1.5">
            <div className="flex shrink-0 items-center gap-0.5" role="toolbar" aria-label="Markdown">
              {tools}
            </div>
            <div className="ml-auto flex shrink-0 items-center gap-1 pl-2">
              {templates && templates.length > 0 && onApplyTemplate && (
                <Menu
                  open={templateMenuOpen}
                  onOpenChange={setTemplateMenuOpen}
                  placement="bottom-end"
                  trigger={
                    <Button size="xs" variant="ghost" icon="file" iconRight="chevronDown" disabled={disabled}>
                      {t("reports.editor.template")}
                    </Button>
                  }
                >
                  {templates.map((tpl) => (
                    <MenuItem
                      key={tpl.id}
                      icon="file"
                      description={tpl.description}
                      onSelect={() => onApplyTemplate(tpl)}
                    >
                      {tpl.name}
                    </MenuItem>
                  ))}
                </Menu>
              )}
              {extraActions}
            </div>
            <input
              ref={fileRef}
              type="file"
              accept="image/png,image/jpeg,image/webp,image/gif"
              multiple
              hidden
              onChange={(event) => {
                const files = Array.from(event.target.files ?? []);
                event.target.value = "";
                if (files.length) void insertImages(files);
              }}
            />
          </div>
        </div>
      )}

      <div className={cn("relative grid min-h-0 flex-1 grid-cols-1 grid-rows-1", view === "split" && "@[640px]:grid-cols-2 @max-[640px]:grid-rows-2")}>
        {showEditor && (
          <div className={cn("relative min-h-0 min-w-0", view === "split" && "border-cx-border-subtle @[640px]:border-r @max-[640px]:border-b")}>
            <ReportTextEditor
              inputRef={areaRef}
              value={value}
              centered={centered}
              disabled={Boolean(disabled)}
              onChange={onChange}
              onKeyDown={onKeyDown}
              onPaste={onPaste}
              onDragOver={(event) => { if (hasImageFiles(event)) { event.preventDefault(); event.stopPropagation(); setFileDrag(true); } }}
              onDragLeave={() => setFileDrag(false)}
              onDrop={onDropFiles}
              onScroll={scrollEditor}
              placeholder={t("reports.editor.placeholder")}
              label={t("reports.title")}
            />
            {fileDrag && (
              <div className="pointer-events-none absolute inset-2 grid place-items-center rounded-xl border-2 border-dashed border-cx-accent bg-cx-accent-soft text-cx-sm font-medium text-cx-accent">
                <span className="inline-flex items-center gap-2"><Icon name="image" size={16} />{t("reports.editor.image")}</span>
              </div>
            )}
          </div>
        )}
        {renderPreview && (
          <div
            ref={previewRef}
            tabIndex={nativePreview && showPreview ? 0 : undefined}
            aria-label={nativePreview ? t("reports.editor.preview") : undefined}
            aria-hidden={!showPreview || undefined}
            inert={!showPreview || undefined}
            onScroll={(event) => syncScroll(event.currentTarget, areaRef.current?.scrollElement ?? null)}
            className={cn(
              "cx-scroll absolute inset-0 min-h-0 min-w-0 overflow-y-auto",
              view !== "preview" && "@[640px]:left-1/2 @max-[640px]:top-1/2",
              !showPreview && "invisible pointer-events-none",
            )}
          >
            {nativePreview ? (
              <ReportNativePreview
                surfaceId={nativeSurface}
                target={previewRef}
                active={showPreview}
                text={previewText}
                wide={view === "preview"}
                sendScroll={sendNativeScroll}
                onScroll={scrollFromNative}
                onError={(error) => {
                  console.warn("desktop.report_preview.failed", error);
                  setNativeError(true);
                }}
              />
            ) : <ReportPreviewContent text={previewText} wide={view === "preview"} />}
          </div>
        )}
      </div>
    </div>
  );
}
