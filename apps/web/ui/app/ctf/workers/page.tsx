import type { Metadata } from "next";
import { Suspense } from "react";
import { WorkerOrchestration } from "@/components/WorkerOrchestration";

export const metadata: Metadata = {
  title: "CTF Worker 设置",
  description: "配置 CTF 工作台的 Worker 出战池、运行环境、调度预算与推理模型。",
};

export default function CtfWorkerSettingsPage() {
  return (
    <section className="single-task-workers" aria-label="CTF Worker 设置">
      <div className="single-task-workers-body">
        <Suspense fallback={null}>
          <WorkerOrchestration defaultReturnTo="/ctf" />
        </Suspense>
      </div>
    </section>
  );
}
