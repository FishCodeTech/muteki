"use client";
import { shareDate, type ShareSnapshot } from "@/lib/conversationShares";

/** Plain text renders no executable HTML, remote images, or original file URLs. */
export function ShareSnapshotView({ snapshot, onDownload }: { snapshot: ShareSnapshot; onDownload?: (id: string) => void }) {
  return <section aria-label="分享内容完整预览" className="min-w-0 space-y-4">
    <header className="space-y-1">
      <h2 className="break-words text-lg font-semibold">{snapshot.title}</h2>
      <p className="text-xs text-cx-fg-3">创建快照时的有效分支 · 水位 #{snapshot.watermark} · {snapshot.messages.length} 条正文 · {snapshot.attachments.length} 个附件</p>
      <p className="text-xs text-cx-fg-3">固定于 {shareDate(snapshot.captured_at)}；之后的新消息不会加入。</p>
    </header>
    {snapshot.messages.map((message) => <article key={message.id} className="min-w-0 rounded-xl border border-cx-border p-3">
      <h3 className="mb-2 text-xs font-semibold text-cx-fg-3">{message.role === "user" ? "用户" : "助手"}</h3>
      <pre className="whitespace-pre-wrap break-words font-sans text-sm [overflow-wrap:anywhere]">{message.text || "（仅附件）"}</pre>
    </article>)}
    {snapshot.tool_summaries.length > 0 ? <section aria-label="不含参数的工具状态摘要"><h3 className="text-sm font-semibold">工具摘要（不含调用参数与输出）</h3><ul className="mt-2 space-y-1 text-sm">{snapshot.tool_summaries.map((tool, i) => <li key={i}>{tool.name} · {{ completed: "已完成", failed: "失败", declined: "已拒绝", cancelled: "已取消", ended: "已结束" }[tool.status] || "已结束"}</li>)}</ul></section> : null}
    {snapshot.attachments.map((item) => <section key={item.id} className="min-w-0 rounded-xl border border-cx-border p-3">
      <h3 className="break-words text-sm font-semibold">附件：{item.name}（{item.size} 字节）</h3>
      <pre className="mt-2 whitespace-pre-wrap break-words text-xs [overflow-wrap:anywhere]">{item.text}</pre>
      {onDownload ? <button type="button" className="mt-2 rounded border border-cx-border px-3 py-2 text-sm focus-visible:outline-2" onClick={() => onDownload(item.id)}>下载此快照中的附件</button> : null}
    </section>)}
  </section>;
}
