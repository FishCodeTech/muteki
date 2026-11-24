"use client";

import { useEffect, useState } from "react";

function completionAge(completed: number, now: number): string {
  const seconds = Math.max(0, Math.floor((now - completed) / 1000));
  if (seconds < 5) return "刚刚";
  if (seconds < 60) return `${seconds} 秒前`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)} 分钟前`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} 小时前`;
  return `${Math.floor(seconds / 86400)} 天前`;
}

export function MessageCompletionTime({ completedAt }: { completedAt: string }) {
  const completed = Date.parse(completedAt);
  const [absolute, setAbsolute] = useState(false);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (!Number.isFinite(completed) || absolute) return;
    let timer: ReturnType<typeof setTimeout>;
    const tick = () => {
      clearTimeout(timer);
      if (document.hidden) return;
      const current = Date.now();
      setNow(current);
      const age = Math.max(0, current - completed);
      const step = age < 60_000 ? 1000 : age < 3_600_000 ? 60_000 : age < 86_400_000 ? 3_600_000 : 86_400_000;
      timer = setTimeout(tick, step - age % step + 10);
    };
    tick();
    document.addEventListener("visibilitychange", tick);
    return () => { clearTimeout(timer); document.removeEventListener("visibilitychange", tick); };
  }, [absolute, completed]);

  if (!Number.isFinite(completed)) return null;
  const date = new Date(completed);
  const pad = (value: number) => String(value).padStart(2, "0");
  const fullTime = `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
  const label = absolute ? fullTime : completionAge(completed, now);
  const hint = absolute ? "点击显示距完成的时间" : "点击显示完整完成时间";
  return (
    <button
      type="button"
      aria-pressed={absolute}
      aria-label={`完成于 ${fullTime}（本地时间），${hint}`}
      title={`${fullTime}（本地时间） · ${hint}`}
      onClick={() => setAbsolute(value => !value)}
      className="inline-flex min-h-7 items-center whitespace-nowrap rounded-md px-1 text-cx-fg-4 transition-colors hover:bg-cx-hover hover:text-cx-fg-2 focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
    >
      <time dateTime={date.toISOString()}>{label}</time>
    </button>
  );
}
