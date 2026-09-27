import Link from "next/link";

export default function NotFound() {
  return (
    <main className="workspace-route-state">
      <section className="workspace-route-card" aria-labelledby="not-found-title">
        <h1 id="not-found-title">找不到这个页面</h1>
        <p>链接可能已失效。返回首页查找最近的工作区，或打开设置。</p>
        <div className="workspace-route-actions"><Link href="/">返回首页</Link><Link href="/settings/agents">打开设置</Link></div>
      </section>
    </main>
  );
}
