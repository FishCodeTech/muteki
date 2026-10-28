"use client";

/* eslint-disable @next/next/no-img-element -- original evidence bytes are served directly, not optimized or rewritten. */

import { useEffect, useState } from "react";
import { Button } from "@heroui/react";
import { Icon } from "@/components/Icon";
import { querySrc } from "./reviewPrimitives";
import { resolveArtifactRef } from "./artifactRefs";
import { byteText } from "./RequestWorkspaceTypes";
import { SrcDefinitions, SrcError, SrcJson, SrcLoading, SrcSection, useSrcResource } from "./ui";

export function ArtifactInspector({ taskId, artifactId }: { taskId: string; artifactId: string }) {
  const resource = useSrcResource(async () => {
    const ref = await resolveArtifactRef(taskId, artifactId);
    const content = await querySrc<unknown>(taskId, "src.artifact.read", { ref_data: ref });
    return { content, metadata: ref };
  }, [taskId, artifactId]);
  const [imageUrl, setImageUrl] = useState("");
  const [binary, setBinary] = useState<Blob | null>(null);
  useEffect(() => {
    if (!resource.data) return;
    const encoded = resource.data.content && typeof resource.data.content === "object"
      ? (resource.data.content as { encoding?: string; data?: string }).data : undefined;
    if (!encoded) { setBinary(null); setImageUrl(""); return; }
    const bytes = Uint8Array.from(atob(encoded), char => char.charCodeAt(0));
    const blob = new Blob([bytes]);
    setBinary(blob);
    const mediaType = String((resource.data.metadata as { media_type?: unknown }).media_type || "application/octet-stream");
    if (!mediaType.startsWith("image/")) { setImageUrl(""); return; }
    const url = URL.createObjectURL(blob);
    setImageUrl(url);
    return () => URL.revokeObjectURL(url);
  }, [resource.data]);
  if (resource.loading && !resource.data) return <SrcLoading label="正在读取 Artifact 原始字节…" />;
  if (!resource.data) return <SrcError error={resource.error} onRetry={() => void resource.reload()} />;
  const metadata = resource.data.metadata as { media_type?: unknown; name?: unknown };
  const text = byteText(resource.data.content);
  const mediaType = String(metadata.media_type || "application/octet-stream");
  const download = () => {
    if (!binary) return;
    const url = URL.createObjectURL(binary);
    const anchor = document.createElement("a");
    anchor.href = url; anchor.download = String(metadata.name || `${artifactId}.bin`);
    anchor.click(); window.setTimeout(() => URL.revokeObjectURL(url), 1000);
  };
  return <div>
    <SrcSection title="Artifact 原始证据" actions={binary ? <Button size="sm" variant="secondary" onPress={download}><Icon name="download" size={14} />下载原字节</Button> : null}>
      <SrcDefinitions rows={[["Artifact / SHA-256", <code key="id">{artifactId}</code>], ["媒体类型", mediaType], ["大小", `${binary?.size ?? 0} bytes`], ["读取状态", "已从当前 Task 权限域读取并校验"]]} />
    </SrcSection>
    <SrcSection title={imageUrl ? "原图" : "原文与元数据"}>
      {imageUrl ? (
        <img className="src-artifact-image" src={imageUrl} alt={`Artifact ${artifactId} 原始截图`} />
      ) : text ? (
        <pre className="src-json src-artifact-text">{text}</pre>
      ) : (
        <SrcJson value={{ ...metadata, binary: true, note: "二进制原文不作为文本展示，可下载原字节" }} />
      )}
    </SrcSection>
  </div>;
}
