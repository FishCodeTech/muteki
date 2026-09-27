"use client";

import { useCallback, useEffect, useState } from "react";
import { Button, FieldError, Input, Label, TextField } from "@heroui/react";
import { checkAuth, login, onAuthRequired } from "@/lib/useRun";
import { clearAllComposerDrafts } from "@/lib/composerDraftStore";
import { clearAllComposerRecall } from "@/lib/composerRecallStore";
import { useT } from "@/lib/i18n";
import { MutekiLogo } from "@/components/MutekiLogo";

type AuthPhase = "checking" | "locked" | "open";
type AuthSnapshot = { authenticated: boolean; authRequired: boolean; at: number };

let authSnapshot: AuthSnapshot | null = null;
const AUTH_SNAPSHOT_MS = 30_000;

function snapshotIsOpen(): boolean {
  if (!authSnapshot) return false;
  if (Date.now() - authSnapshot.at > AUTH_SNAPSHOT_MS) return false;
  return !authSnapshot.authRequired || authSnapshot.authenticated;
}

function rememberAuth(authenticated: boolean, authRequired: boolean): void {
  authSnapshot = { authenticated, authRequired, at: Date.now() };
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
export function LoginGate({ children }: { children: React.ReactNode }) {
  const t = useT();
  const [phase, setPhase] = useState<AuthPhase>(() => (snapshotIsOpen() ? "open" : "checking"));
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const verify = useCallback(async () => {
    try {
      const { authenticated, authRequired } = await checkAuth();
      // authRequired=false → server has no password → always open.
      const open = !authRequired || authenticated;
      rememberAuth(authenticated, authRequired);
      setPhase(open ? "open" : "locked");
    } catch {
      // checkAuth() resolves (never throws) for any HTTP status — a thrown error
      // here means a genuine NETWORK failure (backend down / CORS-blocked). We
      // fail CLOSED: show the login form rather than the deck. Opening on error
      // would be a fail-open auth bypass (e.g. if a cross-origin 401 ever arrived
      // without CORS headers, fetch() rejects → we must NOT let that in).
      authSnapshot = null;
      setPhase("locked");
    }
  }, []);

  useEffect(() => {
    verify();
    // A 401 on any later request clears the token and re-locks the gate.
    return onAuthRequired(() => {
      clearAllComposerDrafts();
      clearAllComposerRecall();
      authSnapshot = null;
      setPassword("");
      setError("");
      setPhase("locked");
    });
  }, [verify]);

  const submit = useCallback(
    async (e: React.FormEvent) => {
      e.preventDefault();
      if (!password) {
        setError(t("login.empty"));
        return;
      }
      setBusy(true);
      setError("");
      try {
        const { ok } = await login(password);
        if (ok) {
          setPassword("");
          rememberAuth(true, true);
          setPhase("open");
        } else {
          setError(t("login.error"));
        }
      } catch {
        setError(t("login.error"));
      } finally {
        setBusy(false);
      }
    },
    [password, t]
  );

  if (phase === "open") return <>{children}</>;

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
