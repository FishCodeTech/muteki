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
    <div
      style={{
        position: "fixed",
        inset: 0,
        display: "grid",
        placeItems: "center",
        background: "var(--bg)",
        zIndex: 9999,
      }}
    >
      {phase === "checking" ? (
        <div style={{ color: "var(--muted)", fontSize: 14 }}>{t("login.checking")}</div>
      ) : (
        <form
          onSubmit={submit}
          style={{
            display: "flex",
            flexDirection: "column",
            gap: 14,
            width: 320,
            padding: 28,
            background: "var(--panel)",
            border: "1px solid var(--line)",
            borderRadius: 14,
            boxShadow: "0 10px 40px rgba(8,12,20,0.12)",
          }}
        >
          <div style={{ textAlign: "center" }}>
            <div style={{ fontSize: 17, fontWeight: 650, color: "var(--bright)" }}>
              Project Muteki
            </div>
            <div style={{ marginTop: 4, fontSize: 13, color: "var(--muted)" }}>
              {t("login.subtitle")}
            </div>
          </div>
          <input
            type="password"
            autoFocus
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            placeholder={t("login.placeholder")}
            disabled={busy}
            style={{
              padding: "10px 12px",
              fontSize: 14,
              color: "var(--text)",
              background: "var(--panel2)",
              border: `1px solid ${error ? "var(--red)" : "var(--line2)"}`,
              borderRadius: 9,
              outline: "none",
            }}
          />
          {error ? (
            <div style={{ fontSize: 12.5, color: "var(--red)" }}>{error}</div>
          ) : null}
          <button
            type="submit"
            disabled={busy}
            style={{
              padding: "10px 12px",
              fontSize: 14,
              fontWeight: 600,
              color: "#fff",
              background: "var(--accent)",
              border: "none",
              borderRadius: 9,
              cursor: busy ? "default" : "pointer",
              opacity: busy ? 0.7 : 1,
            }}
          >
            {busy ? t("login.checking") : t("login.submit")}
          </button>
        </form>
      )}
    </div>
  );
}
