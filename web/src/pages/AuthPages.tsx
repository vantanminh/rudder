import { useState, type FormEvent } from "react";
import { Navigate, useSearchParams } from "react-router-dom";
import { toast } from "sonner";
import { api, ApiError } from "../api";
import { rememberSession, useAuth } from "../auth";
import { Icon, KnotreeMark } from "../components/icons";
import { InsecureBanner } from "../components/insecure-banner";
import { Button, Field, TextInput } from "../components/ui";
import { LANGUAGES, useLanguage, useT } from "../i18n";
import type { SessionResponse } from "../types";

export function SetupPage() {
  const t = useT();
  const { needsSetup, loading, refresh, user } = useAuth();
  const [email, setEmail] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [pending, setPending] = useState(false);

  if (!loading && user) {
    return <Navigate to="/" replace />;
  }
  if (!loading && !needsSetup) {
    return <Navigate to="/login" replace />;
  }

  async function onSubmit(event: FormEvent) {
    event.preventDefault();
    setPending(true);
    try {
      const body = await api<SessionResponse>("/api/v1/setup", {
        method: "POST",
        body: JSON.stringify({
          email,
          password,
          display_name: displayName || undefined,
        }),
      });
      rememberSession(body);
      await refresh();
    } catch (error) {
      toast.error(error instanceof ApiError ? error.message : t("Setup failed"));
    } finally {
      setPending(false);
    }
  }

  return (
    <AuthFrame title={t("Create the owner")} subtitle={t("This account administers the panel.")}>
      <form className="grid gap-4" onSubmit={(event) => void onSubmit(event)}>
        <Field label={t("Email")}>
          <TextInput
            type="email"
            required
            value={email}
            onChange={(event) => setEmail(event.target.value)}
          />
        </Field>
        <Field label={t("Display name")}>
          <TextInput value={displayName} onChange={(event) => setDisplayName(event.target.value)} />
        </Field>
        <Field
          label={t("Password")}
          hint={t("Use at least 12 characters. A short phrase works well.")}
        >
          <TextInput
            type="password"
            required
            minLength={12}
            value={password}
            onChange={(event) => setPassword(event.target.value)}
          />
        </Field>
        <Button type="submit" size="lg" disabled={pending} className="mt-2 w-full">
          {pending ? t("Creating…") : t("Create owner")}
        </Button>
      </form>
    </AuthFrame>
  );
}

/** Only the agent approval flow may leave the sign-in page for a specific path. */
function safeNext(value: string | null): string | null {
  if (!value || !value.startsWith("/") || value.startsWith("//") || value.includes("\\")) {
    return null;
  }
  const path = value.split("?")[0];
  if (path === "/oauth/authorize" || path === "/mcp/consent" || path === "/tokens") {
    return value;
  }
  return null;
}

export function LoginPage() {
  const t = useT();
  const { needsSetup, loading, refresh, user } = useAuth();
  const [params] = useSearchParams();
  const next = safeNext(params.get("next"));
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [totp, setTotp] = useState("");
  const [needsTotp, setNeedsTotp] = useState(false);
  const [pending, setPending] = useState(false);

  if (!loading && needsSetup) {
    return <Navigate to="/setup" replace />;
  }
  if (!loading && user) {
    return <Navigate to={next ?? "/"} replace />;
  }

  async function onSubmit(event: FormEvent) {
    event.preventDefault();
    setPending(true);
    try {
      const body = await api<SessionResponse>("/api/v1/login", {
        method: "POST",
        body: JSON.stringify({
          email,
          password,
          totp_code: totp || undefined,
        }),
      });
      rememberSession(body);
      await refresh();
    } catch (error) {
      if (error instanceof ApiError && error.code === "totp_required") {
        setNeedsTotp(true);
        toast.message(t("Enter the authenticator code"));
      } else {
        toast.error(error instanceof ApiError ? error.message : t("Sign-in failed"));
      }
    } finally {
      setPending(false);
    }
  }

  return (
    <AuthFrame title={t("Sign in")} subtitle={t("Use your Rudder panel account.")}>
      <form className="grid gap-4" onSubmit={(event) => void onSubmit(event)}>
        <Field label={t("Email")}>
          <TextInput
            type="email"
            required
            value={email}
            onChange={(event) => setEmail(event.target.value)}
          />
        </Field>
        <Field label={t("Password")}>
          <TextInput
            type="password"
            required
            value={password}
            onChange={(event) => setPassword(event.target.value)}
          />
        </Field>
        {needsTotp ? (
          <Field label={t("Authenticator code")}>
            <TextInput
              inputMode="numeric"
              autoComplete="one-time-code"
              placeholder="000000"
              className="h-12 text-center font-mono text-xl! tracking-[0.35em]"
              value={totp}
              onChange={(event) => setTotp(event.target.value)}
            />
          </Field>
        ) : null}
        <Button type="submit" size="lg" disabled={pending} className="mt-2 w-full">
          {pending ? t("Signing in…") : t("Sign in")}
        </Button>
      </form>
    </AuthFrame>
  );
}

function AuthFrame({
  title,
  subtitle,
  children,
}: {
  title: string;
  subtitle: string;
  children: React.ReactNode;
}) {
  const t = useT();
  const { language, setLanguage } = useLanguage();
  return (
    <div className="flex min-h-screen flex-col">
      <InsecureBanner link={false} />
      <header className="flex items-center gap-2.5 px-5 py-5 sm:px-8">
        <KnotreeMark size={26} />
        <span className="text-[15px] font-semibold tracking-[-0.01em] text-ink">Knotree</span>
        <span className="border-l border-line-strong pl-2.5 text-[15px] text-muted">Rudder</span>
      </header>
      <main className="flex flex-1 items-center justify-center px-4 pb-16">
        <div className="w-full max-w-[400px] animate-fade-up rounded-auth border border-line bg-surface p-6 shadow-raised sm:p-8">
          <span className="mb-5 inline-flex size-10 items-center justify-center rounded-alert border border-pine-line bg-pine-soft text-pine">
            <Icon name="key" size={20} />
          </span>
          <h1 className="text-[22px] leading-tight font-semibold tracking-[-0.015em]">{title}</h1>
          <p className="mt-1.5 mb-6 text-[14.5px] text-muted">{subtitle}</p>
          {children}
        </div>
      </main>
      <footer className="flex flex-col items-center gap-2 px-5 py-5 text-center text-xs text-faint sm:px-8">
        <span>{t("Self-hosted deployment panel")}</span>
        <div role="group" aria-label={t("Language")} className="flex items-center gap-1">
          {LANGUAGES.map((item) => (
            <button
              key={item.code}
              type="button"
              aria-pressed={language === item.code}
              onClick={() => setLanguage(item.code)}
              className={`rounded-control px-2 py-1 transition-colors hover:text-ink ${language === item.code ? "bg-sunken font-medium text-ink" : ""}`}
            >
              {item.label}
            </button>
          ))}
        </div>
      </footer>
    </div>
  );
}
