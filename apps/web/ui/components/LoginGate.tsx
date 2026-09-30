"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { Button, FieldError, Input, Label, TextField } from "@heroui/react";
import { checkAuth, login, onAuthRequired, currentAuthGeneration, currentAuthPersistenceWarning } from "@/lib/useRun";
import { useT } from "@/lib/i18n";
import { MutekiLogo } from "@/components/MutekiLogo";

type AuthPhase = "checking" | "locked" | "open";
type AuthSnapshot = { authenticated: boolean; authRequired: boolean; at: number; generation: number };

let authSnapshot: AuthSnapshot | null = null;
const AUTH_SNAPSHOT_MS = 30_000;

function snapshotIsOpen(): boolean {
  if (!authSnapshot || authSnapshot.generation !== currentAuthGeneration()) return false;
  if (Date.now() - authSnapshot.at > AUTH_SNAPSHOT_MS) return false;
  return !authSnapshot.authRequired || authSnapshot.authenticated;
}

function rememberAuth(authenticated: boolean, authRequired: boolean): void {
  authSnapshot = { authenticated, authRequired, at: Date.now(), generation: currentAuthGeneration() };
}

/**
 * Auth gate (P3). Wraps the whole deck. On mount it asks the backend whether a
 * valid token is present (checkAuth → GET /api/auth/me). Three outcomes:
 *
 *   - auth disabled (no MUTEKI_WEB_PASSWORD on the server)  → render children.
 *   - token already valid                                   → render children.
 *   - otherwise                                             → show the password form.
 *
 * A mid-session 401 (token expired/cleared) fires onAuthRequired(), which bounces
 * back to the form without a reload. The password is POSTed to /api/auth/login
 * and exchanged for a signed session token (stored in localStorage by login());
 * the password itself never persists client-side.
 *
 * Settings and other routes remount this gate on every navigation. A short
 * in-memory snapshot keeps an already-authenticated session from flashing
 * "校验中…" while /api/auth/me revalidates in the background.
 */
export function LoginGate({ children, onAuthStateChange }: { children: React.ReactNode; onAuthStateChange?: (open: boolean) => void }) {
  const t = useT();
  const [phase, setPhase] = useState<AuthPhase>(() => (snapshotIsOpen() ? "open" : "checking"));
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => { onAuthStateChange?.(phase === "open"); }, [phase, onAuthStateChange]);

  const verifySequence = useRef(0);
  const verify = useCallback(async () => {
    const sequence = ++verifySequence.current;
    try {
      const { authenticated, authRequired } = await checkAuth();
      if (sequence !== verifySequence.current) return;
      // Auth metadata was verified against the current service.
      const open = !authRequired || authenticated;
      rememberAuth(authenticated, authRequired);
      setPhase(open ? "open" : "locked");
    } catch (exc) {
      if (sequence !== verifySequence.current) return;
      setError(exc instanceof Error ? exc.message : "认证校验失败，请重试。");
      authSnapshot = null;
      setPhase("locked");
    }
  }, []);

  useEffect(() => {
    verify();
    // A 401 on any later request clears the token and re-locks the gate.
    return onAuthRequired((reason) => {
      verifySequence.current += 1;
      authSnapshot = null;
      setPassword("");
      setError("");
      setPhase(reason === "expired" ? "locked" : "checking");
      if (reason !== "expired") void verify();
    });
  }, [verify]);

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
        const { ok, authRequired } = await login(password);
        if (ok) {
          setPassword("");
          const verified = await checkAuth();
          rememberAuth(verified.authenticated, authRequired);
          setPhase(verified.authenticated ? "open" : "locked");
        } else {
          setError(t("login.error"));
        }
      } catch (exc) {
        setError(exc instanceof Error ? exc.message : t("login.error"));
      } finally {
        setBusy(false);
      }
    },
    [password, t]
  );

  if (phase === "open") return <>{currentAuthPersistenceWarning() ? <div role="status" className="px-4 py-2 text-sm">{currentAuthPersistenceWarning()}</div> : null}{children}</>;

  return (
    <div className="login-gate">
      {phase === "checking" ? (
        <div className="login-gate-checking" role="status">{t("login.checking")}</div>
      ) : (
        <form className="login-gate-card" aria-busy={busy} noValidate onSubmit={submit}>
          <div className="login-gate-copy">
            <MutekiLogo size={52} wordmark className="login-gate-logo" />
            <div className="login-gate-sub">{t("login.subtitle")}</div>
          </div>
          <TextField value={password} onChange={setPassword} isInvalid={Boolean(error)} isDisabled={busy}>
            <Label className="sr-only">{t("login.placeholder")}</Label>
            <Input className={`login-gate-input ${error ? "error" : ""}`} type="password" autoFocus autoComplete="current-password" placeholder={t("login.placeholder")} />
            {error ? <FieldError className="login-gate-error"><span role="alert">{error}</span></FieldError> : null}
          </TextField>
          <Button className="login-gate-submit" type="submit" variant="primary" isPending={busy} isDisabled={busy}>
            {busy ? t("login.checking") : t("login.submit")}
          </Button>
        </form>
      )}
    </div>
  );
}
