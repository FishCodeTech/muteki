"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { Button, Checkbox, IconButton, Spinner, TextField } from "@/components/chat/ui";
import { checkAuth, login, onAuthRequired, currentAuthGeneration, currentAuthPersistenceWarning, currentAuthNotice, currentServiceOrigin, isNativeAuth } from "@/lib/serviceAuth";
import { useT, useLang } from "@/lib/i18n";
import { MutekiLogo } from "@/components/MutekiLogo";

import { NotificationInbox } from "./NotificationInbox";

type AuthPhase = "checking" | "locked" | "open" | "unavailable";
type AuthSnapshot = { authenticated: boolean; authRequired: boolean; at: number; generation: number; expiresAt: number | null };

let authSnapshot: AuthSnapshot | null = null;
const AUTH_SNAPSHOT_MS = 30_000;

function snapshotIsOpen(): boolean {
  if (!authSnapshot || authSnapshot.generation !== currentAuthGeneration()) return false;
  if (Date.now() - authSnapshot.at > AUTH_SNAPSHOT_MS || (authSnapshot.expiresAt !== null && authSnapshot.expiresAt * 1000 <= Date.now())) return false;
  return !authSnapshot.authRequired || authSnapshot.authenticated;
}

function rememberAuth(authenticated: boolean, authRequired: boolean, expiresAt: number | null): void {
  authSnapshot = { authenticated, authRequired, expiresAt, at: Date.now(), generation: currentAuthGeneration() };
}

export function LoginGate({ children, onAuthStateChange }: { children: React.ReactNode; onAuthStateChange?: (open: boolean) => void }) {
  const t = useT();
  const { lang } = useLang(); const en = lang === "en";
  const [remember, setRemember] = useState(true);
  const [connectionOrigin, setConnectionOrigin] = useState("");
  const [phase, setPhase] = useState<AuthPhase>(() => (snapshotIsOpen() ? "open" : "checking"));
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [expiresAt, setExpiresAt] = useState<number | null>(() => authSnapshot?.expiresAt ?? null);
  useEffect(() => { onAuthStateChange?.(phase === "open"); }, [phase, onAuthStateChange]);

  const verifySequence = useRef(0);
  const verify = useCallback(async () => {
    const sequence = ++verifySequence.current;
    try {
      const { authenticated, authRequired, expiresAt: expiry } = await checkAuth();
      if (sequence !== verifySequence.current) return;
      // Auth metadata was verified against the current service.
      const open = !authRequired || authenticated;
      rememberAuth(authenticated, authRequired, expiry);
      setExpiresAt(expiry);
      setPhase(open ? "open" : "locked");
    } catch (exc) {
      if (sequence !== verifySequence.current) return;
      setError(exc instanceof Error ? exc.message : "认证校验失败，请重试。");
      authSnapshot = null;
      setPhase("unavailable");
    }
  }, []);

  useEffect(() => {
    setConnectionOrigin(currentServiceOrigin());
    verify();
    // A later 401 invalidates verified session metadata and re-locks the gate.
    return onAuthRequired((reason) => {
      verifySequence.current += 1;
      setConnectionOrigin(currentServiceOrigin());
      authSnapshot = null;
      setPassword("");
      setError("");
      setPhase(reason === "expired" ? "locked" : "checking");
      if (reason !== "expired") void verify();
    });
  }, [verify]);

  useEffect(() => {
    if (phase !== "open") return;
    const visible = () => { if (document.visibilityState === "visible") void verify(); };
    window.addEventListener("focus", visible);
    document.addEventListener("visibilitychange", visible);
    const timer = expiresAt === null ? null : window.setTimeout(() => void verify(),
      Math.max(0, Math.min(2_147_483_647, expiresAt * 1000 - Date.now() + 100)));
    return () => {
      window.removeEventListener("focus", visible);
      document.removeEventListener("visibilitychange", visible);
      if (timer !== null) window.clearTimeout(timer);
    };
  }, [phase, expiresAt, verify]);

  const submit = useCallback(
    async (e: React.FormEvent) => {
      e.preventDefault();
      if (!password) {
        setError(t("login.empty"));
        return;
      }
      verifySequence.current += 1;
      setBusy(true);
      setError("");
      try {
        const { ok, authRequired } = await login(password, remember);
        if (ok) {
          setPassword("");
          const verified = await checkAuth();
          rememberAuth(verified.authenticated, authRequired, verified.expiresAt);
          setExpiresAt(verified.expiresAt);
          setPhase(verified.authenticated ? "open" : "locked");
          if (!verified.authenticated) setError(isNativeAuth() ? "桌面会话未能建立，请重新连接后重试。" : "浏览器未能保存登录，请允许本站使用 Cookie 后重试。");
        } else {
          setError(t("login.error"));
        }
      } catch (exc) {
        setError(exc instanceof Error ? exc.message : t("login.error"));
      } finally {
        setBusy(false);
      }
    },
    [password, remember, t]
  );

  if (phase === "open") return <>{currentAuthPersistenceWarning() ? <div role="status" className="cx-root border-b border-cx-border-subtle px-4 py-2 text-[13px] text-cx-fg-2">{currentAuthPersistenceWarning()}</div> : null}<NotificationInbox />{children}</>;

  return (
    <div className="cx-root login-gate">
      <section className="login-gate-card" aria-labelledby="login-heading">
        <header className="login-gate-copy">
          <MutekiLogo size={28} wordmark />
          <h1 id="login-heading">{en ? "Sign in to your workspace" : "登录工作台"}</h1>
          <p>{phase === "unavailable"
            ? (en ? "Check that the service is running and try again." : "请检查服务是否运行，然后重新连接。")
            : (en ? "Enter this service’s access password to continue." : "使用当前服务的访问密码继续。")}</p>
        </header>
        {phase === "checking" ? <div className="login-gate-checking" role="status"><Spinner /><span>{t("login.checking")}</span></div>
          : phase === "unavailable" ? <div className="login-gate-unavailable">
            <p role="alert" className="login-gate-error">{error}</p>
            <Button variant="secondary" size="lg" onClick={() => { setError(""); setPhase("checking"); void verify(); }}>{en ? "Retry connection" : "重新校验连接"}</Button>
          </div>
            : <form className="login-gate-form" aria-busy={busy} noValidate onSubmit={submit}>
              {currentAuthNotice() ? <p role="status" className="login-gate-notice">{currentAuthNotice()}</p> : null}
              <TextField label={t("login.placeholder")} type="password" autoFocus autoComplete="current-password" size="lg"
                placeholder={en ? "Enter your access password" : "输入访问密码"} value={password}
                onChange={event => { setPassword(event.target.value); if (error) setError(""); }} disabled={busy}
                aria-describedby={error ? "login-error" : undefined} invalid={Boolean(error)} />
              {error ? <p id="login-error" role="alert" className="login-gate-error">{error}</p> : null}
              <Checkbox checked={remember} onCheckedChange={setRemember} disabled={busy} label={en ? "Remember sign-in on this device" : "在这台设备上记住登录"} />
              <Button type="submit" variant="primary" size="lg" className="w-full" loading={busy} disabled={busy}>{busy ? t("login.checking") : (en ? "Sign in" : "登录")}</Button>
            </form>}
        <footer className="login-gate-service">
          <div><span>{en ? "Connected service" : "当前服务"}</span><span title={connectionOrigin}>{connectionOrigin}</span></div>
          {phase !== "unavailable" ? <IconButton icon="refresh" size="sm" label={en ? "Retry connection" : "重新校验连接"}
            disabled={busy || phase === "checking"} onClick={() => { setError(""); setPhase("checking"); void verify(); }} /> : null}
        </footer>
      </section>
    </div>
  );
}
