"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { Button } from "@muteki/ui";
import { Icon } from "@muteki/ui";
import { readCacheIdentity } from "@/lib/readCache";

function sameDependencies(a: readonly unknown[], b: readonly unknown[]): boolean {
  return a.length === b.length && a.every((item, index) => Object.is(item, b[index]));
}

export function SrcError({ error, onRetry }: { error: string; onRetry?: () => void }) {
  if (!error) return null;
  return <div className="src-notice src-notice--danger" role="alert"><Icon name="circleAlert" size={15} /><span>{error}</span>{onRetry ? <Button size="sm" variant="ghost" onClick={onRetry}>重试</Button> : null}</div>;
}

export function SrcLoading({ label = "正在读取…" }: { label?: string }) {
  return <div className="src-empty" role="status"><Icon name="loader" size={18} className="src-spin" /><p>{label}</p></div>;
}

export function useSrcResource<T>(loader: () => Promise<T>, dependencies: readonly unknown[], options?: { pollMs?: number; enabled?: boolean }) {
  const { pollMs = 0, enabled = true } = options || {};
  const effectiveDeps = [readCacheIdentity(), ...dependencies];
  const [state, setState] = useState<{ deps: readonly unknown[]; data: T | null; error: string; loading: boolean }>(() => ({
    deps: effectiveDeps,
    data: null,
    error: "",
    loading: enabled,
  }));
  const sameResource = sameDependencies(state.deps, effectiveDeps);
  const data = sameResource ? state.data : null;
  const error = sameResource ? state.error : "";
  const loading = sameResource ? state.loading : enabled;

  const seq = useRef(0);
  const inFlightSeq = useRef(-1);
  const loaderRef = useRef(loader);
  loaderRef.current = loader;
  const effectiveDepsRef = useRef(effectiveDeps);
  effectiveDepsRef.current = effectiveDeps;
  const dataRef = useRef(data);
  dataRef.current = data;

  const setData = useCallback((value: T | null) => {
    setState(prev => ({ ...prev, deps: effectiveDepsRef.current, data: value }));
  }, []);

  const reload = useCallback(async (reloadOptions?: { tick?: boolean }) => {
    if (reloadOptions?.tick && inFlightSeq.current === seq.current) return;
    const targetDeps = effectiveDepsRef.current;
    const current = ++seq.current;
    inFlightSeq.current = current;
    if (dataRef.current === null) {
      setState(prev => ({
        deps: targetDeps,
        data: sameDependencies(prev.deps, targetDeps) ? prev.data : null,
        error: sameDependencies(prev.deps, targetDeps) ? prev.error : "",
        loading: true,
      }));
    }
    try {
      const result = await loaderRef.current();
      if (current === seq.current) {
        setState({ deps: targetDeps, data: result, error: "", loading: false });
      }
    } catch (cause) {
      if (current === seq.current) {
        const message = cause instanceof Error ? cause.message : String(cause);
        setState(prev => ({
          deps: targetDeps,
          data: sameDependencies(prev.deps, targetDeps) ? prev.data : null,
          error: message,
          loading: false,
        }));
      }
    } finally {
      if (inFlightSeq.current === current) inFlightSeq.current = -1;
    }
  }, []);

  useEffect(() => {
    const targetDeps = effectiveDepsRef.current;
    if (!enabled) {
      setState(prev => ({
        deps: targetDeps,
        data: sameDependencies(prev.deps, targetDeps) ? prev.data : null,
        error: sameDependencies(prev.deps, targetDeps) ? prev.error : "",
        loading: false,
      }));
      return;
    }
    seq.current += 1;
    void reload();
    if (!pollMs) return;
    const timer = window.setInterval(() => {
      if (typeof document === "undefined" || !document.hidden) void reload({ tick: true });
    }, pollMs);
    const onVisibility = () => {
      if (!document.hidden) void reload({ tick: true });
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
      seq.current += 1;
    };
    // The caller controls resource identity through dependencies.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...effectiveDeps, enabled, pollMs, reload]);
  return { data, error, loading, reload, setData };
}
