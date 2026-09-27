import type { Metadata } from "next";
import { WorkerOrchestration } from "@/components/WorkerOrchestration";

export const metadata: Metadata = {
  title: "单题设置",
  description: "配置单题任务使用的 Worker 出战池、运行环境、调度预算与推理模型。",
};

export default function TaskWorkerSettingsPage() {
  return (
    <section className="single-task-workers" aria-label="单题设置">
      <div className="single-task-workers-body">
        <WorkerOrchestration defaultReturnTo="/task" />
      </div>
    </section>
  );
}
