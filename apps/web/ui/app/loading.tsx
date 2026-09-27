export default function Loading() {
  return (
    <div className="workspace-route-state" role="status" aria-label="正在打开工作区">
      <div className="workspace-route-card" aria-hidden="true">
        <div className="t-skeleton ux-skeleton-line short" />
        <div className="t-skeleton ux-skeleton-line" />
        <div className="t-skeleton ux-skeleton-line" />
        <p>正在打开工作区…</p>
      </div>
    </div>
  );
}
