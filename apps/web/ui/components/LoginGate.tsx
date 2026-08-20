"use client";

import { useCallback, useEffect, useState } from "react";
import { checkAuth, login, onAuthRequired } from "@/lib/useRun";
import { useT } from "@/lib/i18n";

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
 */
export function LoginGate({ children }: { children: React.ReactNode }) {
  const t = useT();
  const [phase, setPhase] = useState<"checking" | "locked" | "open">("checking");
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const verify = useCallback(async () => {
    try {
      const { authenticated, authRequired } = await checkAuth();
      // authRequired=false → server has no password → always open.
      setPhase(!authRequired || authenticated ? "open" : "locked");
    } catch {
      // checkAuth() resolves (never throws) for any HTTP status — a thrown error
      // here means a genuine NETWORK failure (backend down / CORS-blocked). We
      // fail CLOSED: show the login form rather than the deck. Opening on error
      // would be a fail-open auth bypass (e.g. if a cross-origin 401 ever arrived
      // without CORS headers, fetch() rejects → we must NOT let that in).
      setPhase("locked");
    }
  }, []);

  useEffect(() => {
    verify();
    // A 401 on any later request clears the token and re-locks the gate.
    return onAuthRequired(() => {
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
        <div className="login-gate-checking">{t("login.checking")}</div>
      ) : (
        <form className="login-gate-card" onSubmit={submit}>
          <div className="login-gate-copy">
            <div className="login-gate-title">Project Muteki</div>
            <div className="login-gate-sub">{t("login.subtitle")}</div>
          </div>
          <input
            className={`login-gate-input ${error ? "error" : ""}`}
            type="password"
            autoFocus
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            placeholder={t("login.placeholder")}
            disabled={busy}
          />
          {error ? <div className="login-gate-error">{error}</div> : null}
          <button className="login-gate-submit" type="submit" disabled={busy}>
            {busy ? t("login.checking") : t("login.submit")}
          </button>
        </form>
      )}
    </div>
  );
}
