import { useState } from "react";
import type { FormEvent } from "react";
import type { KoiApiClient, AuthUser } from "../api/client";
import { useI18n } from "../i18n";

export function AuthScreen({
  api,
  onAuthenticated,
}: {
  api: KoiApiClient;
  onAuthenticated: (user: AuthUser) => void;
}) {
  const { t, locale, setLocale } = useI18n();
  const [mode, setMode] = useState<"login" | "register">("login");
  const [email, setEmail] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const user =
        mode === "register"
          ? await api.register({ email, username, password })
          : await api.login({ email, password });
      onAuthenticated(user);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "Unable to authenticate");
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="auth-shell">
      <section className="auth-card">
        <div className="auth-brand">
          <div className="brand-mark" aria-hidden="true">
            <span />
            <span />
            <span />
          </div>
          <span>koi</span>
          <button
            type="button"
            className="auth-lang"
            onClick={() => setLocale(locale === "zh-CN" ? "en" : "zh-CN")}
          >
            {t("language")}
          </button>
        </div>
        <h1>{mode === "login" ? t("loginTitle") : t("registerTitle")}</h1>
        <p className="auth-copy">{mode === "login" ? t("authCopyLogin") : t("authCopyRegister")}</p>
        <form onSubmit={submit} className="auth-form">
          <label>
            {t("email")}
            <input
              type="email"
              value={email}
              onChange={(event) => setEmail(event.target.value)}
              required
            />
          </label>
          {mode === "register" && (
            <label>
              {t("username")}
              <input
                value={username}
                onChange={(event) => setUsername(event.target.value)}
                minLength={3}
                maxLength={64}
                required
              />
            </label>
          )}
          <label>
            {t("password")}
            <input
              type="password"
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              minLength={12}
              required
            />
          </label>
          {error && <p className="auth-error">{error}</p>}
          <button type="submit" className="button button-primary auth-submit" disabled={busy}>
            {busy ? "…" : mode === "login" ? t("login") : t("register")}
          </button>
        </form>
        <button
          type="button"
          className="auth-switch"
          onClick={() => {
            setMode(mode === "login" ? "register" : "login");
            setError(null);
          }}
        >
          {mode === "login" ? t("authSwitchToRegister") : t("authSwitchToLogin")}
        </button>
      </section>
    </main>
  );
}
