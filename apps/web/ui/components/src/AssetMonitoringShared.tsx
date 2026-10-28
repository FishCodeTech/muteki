"use client";

import type { ReactNode } from "react";
import { Button } from "@heroui/react";
import { Icon } from "@/components/Icon";
import type { PageInfo } from "./assetMonitoringTypes";
import styles from "./AssetMonitoring.module.css";

export function Loading({ children = "正在读取…" }: { children?: ReactNode }) {
  return <p className={styles.loading} role="status"><Icon name="loader" size={15} />{children}</p>;
}

export function ErrorMessage({ message, onRetry }: { message: string; onRetry?: () => void }) {
  return <div className={styles.error} role="alert"><Icon name="circleAlert" size={16} /><span>{message}</span>{onRetry ? <Button size="sm" variant="ghost" onPress={onRetry}>重试</Button> : null}</div>;
}

export function Empty({ children }: { children: ReactNode }) {
  return <p className={styles.empty}>{children}</p>;
}

export function Notice({ children, tone = "info" }: { children: ReactNode; tone?: "info" | "warning" | "danger" }) {
  return <p className={`${styles.notice} ${styles[tone]}`} role="status"><Icon name={tone === "info" ? "info" : "alert"} size={14} />{children}</p>;
}

export function Pagination({ page, onPrevious, onNext, loading }: {
  page: PageInfo; onPrevious?: () => void; onNext?: () => void; loading?: boolean;
}) {
  return <div className={styles.pagination}>
    <span>{page.total === null || page.total === undefined ? "总量未知" : `共 ${page.total} 项`}{page.complete === false ? ` · 结果不完整${page.partial_reason ? `：${page.partial_reason}` : ""}` : ""}</span>
    <div><Button size="sm" variant="ghost" isDisabled={!onPrevious || loading} onPress={onPrevious}>上一页</Button><Button size="sm" variant="ghost" isDisabled={!onNext || loading} onPress={onNext}>下一页</Button></div>
  </div>;
}

export function Timestamp({ value }: { value?: string | null }) {
  if (!value) return <>未知</>;
  const date = new Date(value);
  return <time dateTime={value} title={value}>{Number.isNaN(date.getTime()) ? value : date.toLocaleString()}</time>;
}

export function JsonDetails({ title, value, open }: { title: string; value: unknown; open?: boolean }) {
  return <details className={styles.jsonDetails} open={open}><summary>{title}</summary><pre>{JSON.stringify(value ?? null, null, 2)}</pre></details>;
}

export function Field({ label, children }: { label: string; children: ReactNode }) {
  return <div className={styles.field}><dt>{label}</dt><dd>{children}</dd></div>;
}

export function CursorHistory() {
  return {
    push(stack: string[], cursor: string | null | undefined) {
      return cursor ? [...stack, cursor] : stack;
    },
  };
}
