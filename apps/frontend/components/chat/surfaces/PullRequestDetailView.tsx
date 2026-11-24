"use client";

import { useCallback, useEffect, useState, type ChangeEvent, type FormEvent } from "react";
import { Icon } from "@muteki/ui";
import {
  Badge,
  Button,
  Callout,
  Checkbox,
  DiffStat,
  IconButton,
  Input,
  ScrollArea,
  Select,
  Spinner,
  TextArea,
  type Tone,
} from "@muteki/ui";
import {
  controlErrorText,
  fetchPullRequestStack,
  pullRequestLabel,
  readPullRequest,
  runPullRequestHostAction,
  setPullRequestFilesViewed,
  submitPullRequestReview,
  type ThreadPullRequestLink,
} from "@/lib/controlGrants";

type DetailTab = "conversation" | "files" | "stack";
type MergeMethod = "Merge" | "Squash" | "Rebase";

interface PrFileEntry {
  path?: string;
  status?: string;
  additions?: number;
  deletions?: number;
  patch?: string | null;
  viewed?: boolean;
}

interface PrCommentEntry {
  id?: string;
  author?: string;
  body?: string;
  created_at?: string;
}

interface PrReviewThread {
  id?: string;
  path?: string;
  line?: number;
  resolved?: boolean;
  comments?: PrCommentEntry[];
}

interface PrStackLayer {
  number?: number;
  title?: string;
  url?: string;
  state?: string;
  head_sha?: string;
  is_current?: boolean;
}

function outcomeErrorText(outcome: { type: string; content?: unknown } | undefined): string {
  if (!outcome || outcome.type === "applied") return "";
  if (outcome.type === "rejected" && outcome.content && typeof outcome.content === "object") {
    const c = outcome.content as Record<string, unknown>;
    return String(c.message || c.reason || "操作被代码平台拒绝");
  }
  if (outcome.type === "uncertain") return "请求已发送，但代码平台未确认是否生效，请刷新核对。";
  return "操作未完成。";
}

export function PullRequestDetailView({
  threadId,
  link,
  onBack,
  onUpdated,
}: {
  threadId: string;
  link: ThreadPullRequestLink;
  onBack: () => void;
  onUpdated: () => void;
}) {
  const keyStr = link.url || pullRequestLabel(link.key);
  const [tab, setTab] = useState<DetailTab>("conversation");
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [caps, setCaps] = useState<Record<string, unknown>>({});
  const [conversation, setConversation] = useState<Record<string, unknown> | null>(null);
  const [files, setFiles] = useState<PrFileEntry[]>([]);
  const [viewedSet, setViewedSet] = useState<Set<string>>(() => new Set());
  const [stack, setStack] = useState<Record<string, unknown> | null>(null);

  const [commentBody, setCommentBody] = useState("");
  const [reviewVerdict, setReviewVerdict] = useState<"Comment" | "Approve" | "RequestChanges">("Comment");
  const [reviewBody, setReviewBody] = useState("");
  const [mergeMethod, setMergeMethod] = useState<MergeMethod>("Squash");
  const [removeCredits, setRemoveCredits] = useState(false);
  const [replyTarget, setReplyTarget] = useState<string | null>(null);
  const [replyText, setReplyText] = useState("");

  const loadTab = useCallback(async (targetTab: DetailTab) => {
    setLoading(true);
    setError("");
    try {
      if (targetTab === "conversation") {
        const res = await readPullRequest(threadId, keyStr, { type: "conversation" });
        setCaps(res.capabilities || {});
        const content = (res.response?.content && typeof res.response.content === "object")
          ? res.response.content as Record<string, unknown>
          : {};
        setConversation(content);
      } else if (targetTab === "files") {
        const [filesRes, viewedRes] = await Promise.all([
          readPullRequest(threadId, keyStr, { type: "files", content: { cursor: null } }),
          readPullRequest(threadId, keyStr, { type: "viewed_files" }).catch(() => null),
        ]);
        setCaps(filesRes.capabilities || {});
        const fContent = (filesRes.response?.content && typeof filesRes.response.content === "object")
          ? filesRes.response.content as Record<string, unknown>
          : {};
        const list = Array.isArray(fContent.files) ? fContent.files as PrFileEntry[] : [];
        setFiles(list);
        const vContent = (viewedRes?.response?.content && typeof viewedRes.response.content === "object")
          ? viewedRes.response.content as Record<string, unknown>
          : {};
        const vPaths = Array.isArray(vContent.paths) ? vContent.paths.map(String) : [];
        setViewedSet(new Set(vPaths));
      } else {
        const res = await fetchPullRequestStack(threadId, keyStr);
        setStack(res.stack || {});
      }
    } catch (cause) {
      setError(controlErrorText(cause, "读取拉取请求详情失败"));
    } finally {
      setLoading(false);
    }
  }, [threadId, keyStr]);

  useEffect(() => {
    void loadTab(tab);
  }, [tab, loadTab]);

  const headSha = String(
    (conversation?.head_sha as string | undefined)
    || (conversation?.head as Record<string, unknown> | undefined)?.sha
    || "",
  );

  const runAction = async (busyKey: string, action: { type: string; content?: unknown }, okMsg: string) => {
    setBusy(busyKey);
    setError("");
    setNotice("");
    try {
      const res = await runPullRequestHostAction(threadId, keyStr, action);
      const err = outcomeErrorText(res.outcome);
      if (err) {
        setError(err);
        return false;
      }
      setNotice(okMsg);
      onUpdated();
      await loadTab(tab);
      return true;
    } catch (cause) {
      setError(controlErrorText(cause, "执行操作失败"));
      return false;
    } finally {
      setBusy("");
    }
  };

  const handleComment = async (event: FormEvent) => {
    event.preventDefault();
    const text = commentBody.trim();
    if (!text) return;
    const ok = await runAction("comment", { type: "comment", content: { body: text } }, "评论已发布");
    if (ok) setCommentBody("");
  };

  const handleReview = async (event: FormEvent) => {
    event.preventDefault();
    setBusy("review");
    setError("");
    setNotice("");
    try {
      const res = await submitPullRequestReview(threadId, keyStr, reviewVerdict, headSha, reviewBody.trim(), []);
      const err = outcomeErrorText(res.outcome);
      if (err) {
        setError(err);
        return;
      }
      setNotice("审阅已提交");
      setReviewBody("");
      onUpdated();
      await loadTab("conversation");
    } catch (cause) {
      setError(controlErrorText(cause, "提交审阅失败"));
    } finally {
      setBusy("");
    }
  };

  const toggleViewed = async (path: string, nextViewed: boolean) => {
    setBusy(`viewed:${path}`);
    try {
      await setPullRequestFilesViewed(threadId, keyStr, [path], nextViewed);
      setViewedSet((prev) => {
        const next = new Set(prev);
        if (nextViewed) next.add(path);
        else next.delete(path);
        return next;
      });
    } catch (cause) {
      setError(controlErrorText(cause, "更新已读状态失败"));
    } finally {
      setBusy("");
    }
  };

  const comments = Array.isArray(conversation?.comments) ? conversation.comments as PrCommentEntry[] : [];
  const reviewThreads = Array.isArray(conversation?.review_threads) ? conversation.review_threads as PrReviewThread[] : [];
  const stackLayers = Array.isArray(stack?.layers) ? stack.layers as PrStackLayer[] : [];
  const stackId = typeof stack?.id === "number" ? stack.id : null;
  const snapshot = link.snapshot;

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="pull-request-detail">
      <div className="flex items-center gap-2 border-b border-cx-border-subtle px-3 py-2">
        <IconButton icon="arrowLeft" label="返回列表" onClick={onBack} />
        <div className="min-w-0 flex-1">
          <div className="truncate text-cx-sm font-medium text-cx-fg">
            {snapshot?.title || `#${link.key.number}`}
          </div>
          <div className="truncate text-cx-xs text-cx-fg-4">{pullRequestLabel(link.key)}</div>
        </div>
        <IconButton icon="refresh" label="刷新详情" loading={loading} onClick={() => void loadTab(tab)} />
      </div>

      <div className="flex gap-1 border-b border-cx-border-subtle px-3 py-1.5" role="tablist">
        {(["conversation", "files", "stack"] as const).map((item) => (
          <Button
            key={item}
            size="xs"
            variant={tab === item ? "secondary" : "ghost"}
            role="tab"
            aria-selected={tab === item}
            onClick={() => setTab(item)}
          >
            {item === "conversation" ? "对话与操作" : item === "files" ? `文件${files.length ? ` (${files.length})` : ""}` : "堆叠"}
          </Button>
        ))}
      </div>

      <ScrollArea className="flex-1">
        <div className="flex flex-col gap-3 p-3">
          {error ? (
            <Callout tone="danger" role="alert" title="操作失败" onDismiss={() => setError("")}>
              {error}
            </Callout>
          ) : null}
          {notice ? (
            <Callout tone="success" title={notice} onDismiss={() => setNotice("")} />
          ) : null}

          {loading ? (
            <div className="flex items-center justify-center py-8 text-cx-fg-4">
              <Spinner size={20} />
            </div>
          ) : tab === "conversation" ? (
            <>
              {typeof conversation?.body === "string" && conversation.body.trim() ? (
                <div className="rounded-xl border border-cx-border-subtle bg-cx-elevated p-3 text-cx-xs whitespace-pre-wrap break-words text-cx-fg-2">
                  {conversation.body}
                </div>
              ) : null}

              {snapshot && (snapshot.state === "open" || snapshot.state === "closed") ? (
                <section className="flex flex-col gap-2 rounded-xl border border-cx-border-subtle bg-cx-elevated p-3" aria-label="拉取请求生命周期操作">
                  <div className="flex flex-wrap items-center gap-1.5">
                    {snapshot.state === "open" ? (
                      <>
                        <Select<MergeMethod>
                          size="sm"
                          ariaLabel="合并方式"
                          value={mergeMethod}
                          onChange={setMergeMethod}
                          options={[
                            { value: "Squash", label: "Squash 合并" },
                            { value: "Merge", label: "Merge 提交" },
                            { value: "Rebase", label: "Rebase 合并" },
                          ]}
                        />
                        <Checkbox
                          checked={removeCredits}
                          onCheckedChange={setRemoveCredits}
                          className="items-center gap-1.5 px-1"
                          label={<span className="text-cx-xs text-cx-fg-3">移除 Co-authored-by</span>}
                        />
                        <Button
                          size="sm"
                          variant="primary"
                          loading={busy === "merge"}
                          disabled={busy !== "" || !headSha}
                          onClick={() => void runAction(
                            "merge",
                            { type: "merge", content: { head: headSha, method: mergeMethod, auto: false, remove_credits: removeCredits } },
                            "已执行合并",
                          )}
                        >
                          合并 PR
                        </Button>
                        {caps.update_branch !== false ? (
                          <Button
                            size="sm"
                            variant="secondary"
                            loading={busy === "update_branch"}
                            disabled={busy !== "" || !headSha}
                            onClick={() => void runAction("update_branch", { type: "update_branch", content: { head: headSha, rebase: false } }, "已更新分支")}
                          >
                            更新分支
                          </Button>
                        ) : null}
                        {snapshot.is_draft ? (
                          <Button
                            size="sm"
                            variant="secondary"
                            loading={busy === "ready"}
                            disabled={busy !== ""}
                            onClick={() => void runAction("ready", { type: "ready_for_review" }, "已标记为待审阅")}
                          >
                            标记为 Ready
                          </Button>
                        ) : (
                          <Button
                            size="sm"
                            variant="ghost"
                            loading={busy === "draft"}
                            disabled={busy !== ""}
                            onClick={() => void runAction("draft", { type: "convert_to_draft" }, "已转为草稿")}
                          >
                            转为草稿
                          </Button>
                        )}
                        <Button
                          size="sm"
                          variant="ghost"
                          loading={busy === "close"}
                          disabled={busy !== ""}
                          onClick={() => void runAction("close", { type: "close" }, "已关闭 PR")}
                        >
                          关闭 PR
                        </Button>
                      </>
                    ) : (
                      <Button
                        size="sm"
                        variant="secondary"
                        loading={busy === "reopen"}
                        disabled={busy !== ""}
                        onClick={() => void runAction("reopen", { type: "reopen" }, "已重新打开 PR")}
                      >
                        重新打开
                      </Button>
                    )}
                  </div>
                </section>
              ) : null}

              <form onSubmit={(e) => void handleReview(e)} className="flex flex-col gap-2 rounded-xl border border-cx-border-subtle bg-cx-elevated p-3">
                <div className="flex items-center justify-between gap-2">
                  <span className="text-cx-xs font-medium text-cx-fg-2">提交审阅</span>
                  <Select<"Comment" | "Approve" | "RequestChanges">
                    size="sm"
                    ariaLabel="审阅结论"
                    value={reviewVerdict}
                    onChange={setReviewVerdict}
                    options={[
                      { value: "Comment", label: "仅评论" },
                      { value: "Approve", label: "批准 (Approve)" },
                      { value: "RequestChanges", label: "请求修改" },
                    ]}
                  />
                </div>
                <TextArea
                  rows={2}
                  placeholder="填写审阅意见（批准时可留空）"
                  value={reviewBody}
                  onChange={(e: ChangeEvent<HTMLTextAreaElement>) => setReviewBody(e.target.value)}
                />
                <div className="flex justify-end">
                  <Button type="submit" size="xs" variant="secondary" loading={busy === "review"} disabled={busy !== "" || !headSha}>
                    提交审阅
                  </Button>
                </div>
              </form>

              {reviewThreads.length > 0 ? (
                <section className="flex flex-col gap-2">
                  <div className="text-cx-xs font-medium text-cx-fg-3">行内审阅讨论 ({reviewThreads.length})</div>
                  {reviewThreads.map((rt, idx) => {
                    const rtId = rt.id || String(idx);
                    return (
                      <div key={rtId} className="flex flex-col gap-1.5 rounded-xl border border-cx-border-subtle bg-cx-elevated p-2.5 text-cx-xs">
                        <div className="flex items-center justify-between gap-2">
                          <span className="truncate font-mono text-cx-2xs text-cx-fg-3">
                            {rt.path || "文件"}{rt.line ? `:${rt.line}` : ""}
                          </span>
                          <div className="flex items-center gap-1">
                            <Badge tone={(rt.resolved ? "success" : "warning") as Tone}>
                              {rt.resolved ? "已解决" : "未解决"}
                            </Badge>
                            {rt.id ? (
                              <Button
                                size="xs"
                                variant="ghost"
                                loading={busy === `resolve:${rt.id}`}
                                onClick={() => void runAction(`resolve:${rt.id}`, { type: "resolve_thread", content: { thread_id: rt.id, resolved: !rt.resolved } }, rt.resolved ? "已重新打开讨论" : "已标记为已解决")}
                              >
                                {rt.resolved ? "取消解决" : "解决"}
                              </Button>
                            ) : null}
                          </div>
                        </div>
                        {(rt.comments || []).map((c, cIdx) => (
                          <div key={c.id || cIdx} className="rounded-lg bg-cx-bg-subtle px-2 py-1.5">
                            <span className="font-medium text-cx-fg-2">{c.author || "审阅者"}: </span>
                            <span className="whitespace-pre-wrap break-words text-cx-fg-3">{c.body}</span>
                          </div>
                        ))}
                        {rt.id ? (
                          replyTarget === rt.id ? (
                            <div className="flex items-center gap-1.5 pt-1">
                              <Input
                                size="sm"
                                className="min-w-0 flex-1"
                                placeholder="回复该讨论…"
                                value={replyText}
                                onChange={(e) => setReplyText(e.target.value)}
                              />
                              <Button
                                size="md"
                                variant="primary"
                                loading={busy === `reply:${rt.id}`}
                                disabled={!replyText.trim()}
                                onClick={() => {
                                  const text = replyText.trim();
                                  if (!text) return;
                                  void runAction(`reply:${rt.id}`, { type: "reply_to_thread", content: { thread_id: rt.id, body: text } }, "已回复").then((ok) => {
                                    if (ok) {
                                      setReplyText("");
                                      setReplyTarget(null);
                                    }
                                  });
                                }}
                              >
                                发送
                              </Button>
                              <Button size="md" variant="ghost" onClick={() => setReplyTarget(null)}>取消</Button>
                            </div>
                          ) : (
                            <div className="flex justify-end">
                              <Button size="xs" variant="ghost" onClick={() => { setReplyTarget(rt.id || null); setReplyText(""); }}>
                                回复
                              </Button>
                            </div>
                          )
                        ) : null}
                      </div>
                    );
                  })}
                </section>
              ) : null}

              <section className="flex flex-col gap-2">
                <div className="text-cx-xs font-medium text-cx-fg-3">评论 ({comments.length})</div>
                {comments.map((c, idx) => (
                  <div key={c.id || idx} className="rounded-xl border border-cx-border-subtle bg-cx-elevated p-2.5 text-cx-xs">
                    <div className="mb-1 font-medium text-cx-fg-2">{c.author || "用户"}</div>
                    <div className="whitespace-pre-wrap break-words text-cx-fg-3">{c.body}</div>
                  </div>
                ))}
                <form onSubmit={(e) => void handleComment(e)} className="flex flex-col gap-2">
                  <TextArea
                    rows={2}
                    placeholder="添加 PR 评论…"
                    value={commentBody}
                    onChange={(e: ChangeEvent<HTMLTextAreaElement>) => setCommentBody(e.target.value)}
                  />
                  <div className="flex justify-end">
                    <Button type="submit" size="xs" variant="secondary" loading={busy === "comment"} disabled={busy !== "" || !commentBody.trim()}>
                      发表评论
                    </Button>
                  </div>
                </form>
              </section>
            </>
          ) : tab === "files" ? (
            <section className="flex flex-col gap-2">
              {files.length === 0 ? (
                <div className="py-6 text-center text-cx-xs text-cx-fg-4">没有可显示的文件变更。</div>
              ) : files.map((file, idx) => {
                const path = file.path || `file-${idx}`;
                const isViewed = viewedSet.has(path) || Boolean(file.viewed);
                return (
                  <div key={path} className="flex flex-col gap-1.5 rounded-xl border border-cx-border-subtle bg-cx-elevated p-2.5">
                    <div className="flex items-center gap-2">
                      <Icon name="file" size={13} className="shrink-0 text-cx-fg-3" />
                      <span className="min-w-0 flex-1 truncate font-mono text-cx-xs text-cx-fg">{path}</span>
                      {(file.additions != null || file.deletions != null) ? (
                        <DiffStat additions={file.additions ?? 0} deletions={file.deletions ?? 0} />
                      ) : null}
                      <Button
                        size="xs"
                        variant={isViewed ? "soft" : "ghost"}
                        loading={busy === `viewed:${path}`}
                        onClick={() => void toggleViewed(path, !isViewed)}
                      >
                        {isViewed ? "已读" : "标为已读"}
                      </Button>
                    </div>
                    {file.patch ? (
                      <pre className="max-h-48 overflow-auto rounded-lg bg-cx-bg-subtle p-2 font-mono text-cx-2xs leading-4 text-cx-fg-3">
                        {file.patch}
                      </pre>
                    ) : null}
                  </div>
                );
              })}
            </section>
          ) : (
            <section className="flex flex-col gap-2">
              {stackLayers.length === 0 ? (
                <div className="py-6 text-center text-cx-xs text-cx-fg-4">当前拉取请求不在堆叠 (Stack) 中。</div>
              ) : (
                <>
                  {stackLayers.map((layer, idx) => (
                    <div key={layer.number || idx} className="flex items-center gap-2 rounded-xl border border-cx-border-subtle bg-cx-elevated p-2.5 text-cx-xs">
                      <Badge tone={layer.is_current ? "accent" : "neutral"}>#{layer.number}</Badge>
                      <span className="min-w-0 flex-1 truncate text-cx-fg">{layer.title || "拉取请求"}</span>
                      <Badge tone={layer.state === "merged" ? "accent" : layer.state === "open" ? "success" : "neutral"}>
                        {layer.state || "open"}
                      </Badge>
                    </div>
                  ))}
                  {stackId != null ? (
                    <div className="flex justify-end gap-2 pt-1">
                      <Button
                        size="xs"
                        variant="secondary"
                        loading={busy === "rebase_stack"}
                        onClick={() => {
                          const heads = stackLayers
                            .filter((l) => l.state === "open" && l.number && l.head_sha)
                            .map((l) => ({ number: l.number, head: l.head_sha }));
                          void runAction("rebase_stack", { type: "rebase_stack", content: { stack: stackId, heads } }, "已变基堆叠");
                        }}
                      >
                        变基整个堆叠
                      </Button>
                      <Button
                        size="xs"
                        variant="primary"
                        loading={busy === "merge_stack"}
                        onClick={() => {
                          const heads = stackLayers
                            .filter((l) => l.state === "open" && l.number && l.head_sha)
                            .map((l) => ({ number: l.number, head: l.head_sha }));
                          void runAction("merge_stack", { type: "merge_stack", content: { stack: stackId, heads, method: mergeMethod } }, "已触发堆叠合并");
                        }}
                      >
                        合并堆叠
                      </Button>
                    </div>
                  ) : null}
                </>
              )}
            </section>
          )}
        </div>
      </ScrollArea>
    </div>
  );
}
