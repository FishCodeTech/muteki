"use client";

import Link from "next/link";
import { Button } from "@heroui/react";

export default function WorkspaceError({ reset }: { error: Error & { digest?: string }; reset: () => void }) {
  return (
    <main className="workspace-route-state">
      <section className="workspace-route-card" aria-labelledby="workspace-error-title">
        <h1 id="workspace-error-title">工作区暂时无法显示</h1>
        <p role="alert">页面加载出现问题。可以重新尝试，或返回首页选择其他工作区。</p>
        <div className="workspace-route-actions"><Button variant="primary" onPress={reset}>重新尝试</Button><Link href="/">返回首页</Link></div>
      </section>
    </main>
  );
}
