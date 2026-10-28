"use client";

import { useEffect, useState } from "react";
import { Button } from "@heroui/react";
import { Icon } from "@/components/Icon";
import { controlSrcRun, resolveSrcRun, startSrcRun } from "@/lib/srcApi";
import type { SrcRun } from "@/lib/srcTypes";
import { SrcDefinitions, SrcEmpty, SrcError, SrcJson, SrcSection, SrcStatus, stateTone } from "./ui";

export function RunInspector({ taskId, run, onChanged }: { taskId: string; run?: SrcRun; onChanged: () => Promise<void> | void }) {
  const [target, setTarget] = useState("");
  const [goal, setGoal] = useState("");
  const [directive, setDirective] = useState("");
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const boundTarget = run?.binding.target_url || run?.binding.contract?.target || "";
  useEffect(() => {
    setTarget(boundTarget);
    setGoal(""); setDirective(""); setError(""); setNotice("");
  }, [run?.binding.run_id, boundTarget]);
  if (!run) return <SrcEmpty>此 Run 不属于当前 SRC Task，或绑定不可读。</SrcEmpty>;
  const { binding, run: snapshot } = run;
  const detail = snapshot.detail as { summary?: { name?: string; target?: string }; binding_key?: string; task_revision?: number };
  const summary = detail.summary || {};
  const targetHint = boundTarget || summary.target || "";
  const act = async (name: string, action: () => Promise<unknown>, message: string) => {
    if (busy) return;
    setBusy(name); setError(""); setNotice("");
    try { await action(); setNotice(message); await onChanged(); }
    catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setBusy(""); }
  };
  const start = () => act("start", () => startSrcRun(taskId, binding.run_id, { target_url: target.trim(), goal: goal.trim(), prompt: goal.trim() }), "已在原 Run 启动新一代执行；图和 workspace 保持独立。");
  const resolve = () => act("resolve", () => resolveSrcRun(taskId, binding.run_id, directive), "已向原 Run 追加续跑指令，没有创建新图。");
  const control = (action: "pause" | "resume" | "stop") => act(action, () => controlSrcRun(taskId, binding.run_id, action), `已提交 ${action} 控制命令。`);
  return <div>
    <SrcError error={error} />
    {notice ? <p className="src-notice src-notice--success" role="status"><Icon name="check" size={14} />{notice}</p> : null}
    <SrcSection title="可信 Run 绑定" actions={<SrcStatus tone={stateTone(snapshot.state)}>{snapshot.state}</SrcStatus>}>
      <SrcDefinitions rows={[["Run", <code key="run">{binding.run_id}</code>], ["执行器 / 产品", `${binding.execution_mode} / ${binding.product_mode}`], ["执行代", snapshot.generation], ["binding revision", binding.binding_revision], ["binding key", <code key="binding">{binding.binding_key}</code>], ["目标契约", <code key="target">{binding.target_contract_ref}</code>], ["计划 / 授权", `${binding.plan_ref || "未固定"} / ${binding.authorization_ref || "未固定"}`], ["来源消息", binding.source_message_id || "未记录"], ["当前名称", summary.name || binding.run_id]]} />
    </SrcSection>
    <SrcSection title="当前快照">
      <SrcJson value={snapshot.detail} />
      <p className="src-hint">图、workspace 和报告均按此 Run 独立保存；Task 只读投影不会把其他 Run 的证据写成本 Run Fact。</p>
    </SrcSection>
    {snapshot.state === "draft" ? <SrcSection title="启动现有 Run">
      <label className="src-field">完整目标 URL<input type="url" required value={target} placeholder={targetHint || "https://example.com/"} onChange={event => setTarget(event.target.value)} /></label>
      <label className="src-field">本次要回答的问题<textarea rows={3} value={goal} onChange={event => setGoal(event.target.value)} /></label>
      <Button variant="primary" isPending={busy === "start"} isDisabled={!target.trim() || Boolean(busy)} onPress={() => void start()}><Icon name="play" size={14} />在原 Run 启动</Button>
    </SrcSection> : <SrcSection title="在原 Run 续跑 / 补证">
      <label className="src-field">追加指令（可选）<textarea rows={3} value={directive} onChange={event => setDirective(event.target.value)} placeholder="补充证据、平台反馈或修复复测要求" /></label>
      <div className="src-button-row"><Button variant="primary" isPending={busy === "resolve"} isDisabled={Boolean(busy)} onPress={() => void resolve()}><Icon name="refresh" size={14} />续跑原 Run</Button>{snapshot.state === "paused" ? <Button variant="secondary" isPending={busy === "resume"} onPress={() => void control("resume")}>恢复</Button> : <Button variant="secondary" isPending={busy === "pause"} onPress={() => void control("pause")}>暂停</Button>}<Button variant="ghost" isPending={busy === "stop"} onPress={() => void control("stop")}>停止</Button></div>
    </SrcSection>}
  </div>;
}
