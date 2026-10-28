"use client";

import { useMemo, useState } from "react";
import { Button } from "@heroui/react";
import { Icon } from "@/components/Icon";
import { querySrc } from "./reviewPrimitives";
import { indexedArtifacts, type ArtifactRecordPage } from "./artifactRefs";
import { SrcEmpty, SrcError, SrcLoading, SrcSection, SrcStatus, stateTone, useSrcResource } from "./ui";

export function ArtifactCatalog({ taskId, selectedArtifactId, onSelectArtifact }: {
  taskId: string; selectedArtifactId?: string; onSelectArtifact: (artifactId: string) => void;
}) {
  const [cursor, setCursor] = useState("");
  const [history, setHistory] = useState<string[]>([]);
  const [needle, setNeedle] = useState("");
  const [media, setMedia] = useState("");
  const resource = useSrcResource<ArtifactRecordPage>(() => querySrc(taskId, "src.record.list", { kind: "", limit: 500, cursor }), [taskId, cursor]);
  const entries = useMemo(() => indexedArtifacts(resource.data?.records || []).map(entry => ({ ...entry.ref, source_kind: entry.source_kind, source_record_id: entry.source_record_id })), [resource.data]);
  const visible = entries.filter(entry => {
    const matchesText = !needle || `${entry.sha256} ${entry.name || ""} ${entry.source_kind} ${entry.source_record_id}`.toLowerCase().includes(needle.toLowerCase());
    const matchesMedia = !media || entry.media_type === media || (media === "image" && entry.media_type.startsWith("image/")) || (media === "http" && entry.media_type.includes("http"));
    return matchesText && matchesMedia;
  });
  return <div>
    <SrcSection title="Artifact 目录" count={entries.length} actions={<Button size="sm" variant="ghost" onPress={() => void resource.reload()}><Icon name="refresh" size={14} />刷新</Button>}>
      <p className="src-hint">从当前 Task 的持久来源记录中解析 Artifact 引用。这里只列可定位原件，不把摘要或索引条目当作完整证据。</p>
      <div className="src-form-grid">
        <label className="src-field">搜索 hash / 名称 / 来源<input value={needle} onChange={event => setNeedle(event.target.value)} placeholder="筛选当前页" /></label>
        <label className="src-field">媒体类型<select value={media} onChange={event => setMedia(event.target.value)}><option value="">全部</option><option value="image">图片</option><option value="http">HTTP</option><option value="application/json">JSON</option><option value="text/plain">纯文本</option></select></label>
      </div>
      {resource.data?.partial_reason ? <p className="src-notice src-notice--warning" role="status">{resource.data.partial_reason}</p> : null}
      {resource.data?.watermarks?.length ? <details><summary>来源水位与完整性</summary><pre className="src-json">{JSON.stringify(resource.data.watermarks, null, 2)}</pre></details> : null}
    </SrcSection>
    {resource.loading && !resource.data ? <SrcLoading label="正在恢复 Artifact 引用与来源…" /> : <SrcError error={resource.error} onRetry={() => void resource.reload()} />}
    {resource.data && !visible.length ? <SrcEmpty>当前筛选没有 Artifact 引用；空结果与来源失败不会混为一谈。</SrcEmpty> : null}
    <div className="src-record-list">{visible.map(entry => <button key={entry.sha256} aria-current={selectedArtifactId === entry.sha256 ? "true" : undefined} onClick={() => onSelectArtifact(entry.sha256)}><span><strong>{entry.name || entry.artifact_id}</strong><small>{entry.media_type} · {entry.size} bytes · {entry.source_kind}/{entry.source_record_id}</small><code>{entry.sha256}</code></span><SrcStatus tone={stateTone(entry.complete || "unavailable")}>{entry.complete || "unavailable"}</SrcStatus></button>)}</div>
    <div className="src-button-row"><Button size="sm" variant="ghost" isDisabled={!history.length} onPress={() => { const previous = history.at(-1) || ""; setHistory(rows => rows.slice(0, -1)); setCursor(previous); }}>上一页</Button><Button size="sm" variant="ghost" isDisabled={!resource.data?.next_cursor} onPress={() => { const next = resource.data?.next_cursor || ""; setHistory(rows => [...rows, cursor]); setCursor(next); }}>下一页</Button><span className="src-hint">总数：{resource.data?.total ?? "未知"} · 当前页：{entries.length}</span></div>
  </div>;
}
