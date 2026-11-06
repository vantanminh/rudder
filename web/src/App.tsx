import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { lazy, Suspense, useState, type ReactNode } from "react";
import { BrowserRouter, Navigate, Route, Routes, useLocation } from "react-router-dom";
import { Toaster } from "sonner";
import { ApiError } from "./api";
import { AuthProvider, useAuth } from "./auth";
import { LanguageProvider, useT } from "./i18n";
import { Shell } from "./layout/Shell";
import { useDarkTheme } from "./theme";
import { DashboardPage } from "./pages/Dashboard";
import { LoginPage, SetupPage } from "./pages/AuthPages";

const AgentConsentPage = lazy(() =>
  import("./pages/AgentConsentPage").then((m) => ({ default: m.AgentConsentPage })),
);
const AgentsPage = lazy(() =>
  import("./pages/AgentsPage").then((m) => ({ default: m.AgentsPage })),
);
const AccessAdminPage = lazy(() =>
  import("./pages/AccessAdminPage").then((m) => ({ default: m.AccessAdminPage })),
);
const AccessPortalPage = lazy(() =>
  import("./pages/AccessPortalPage").then((m) => ({ default: m.AccessPortalPage })),
);
const AccountPage = lazy(() =>
  import("./pages/AccountPage").then((m) => ({ default: m.AccountPage })),
);
const AlertsPage = lazy(() =>
  import("./pages/AlertsPage").then((m) => ({ default: m.AlertsPage })),
);
const AuditPage = lazy(() => import("./pages/AuditPage").then((m) => ({ default: m.AuditPage })));
const BackupsPage = lazy(() =>
  import("./pages/BackupsPage").then((m) => ({ default: m.BackupsPage })),
);
const CachePoliciesPage = lazy(() =>
  import("./pages/CachePoliciesPage").then((m) => ({ default: m.CachePoliciesPage })),
);
const CertificatesPage = lazy(() =>
  import("./pages/CertificatesPage").then((m) => ({ default: m.CertificatesPage })),
);
const GitHubCallbackPage = lazy(() =>
  import("./pages/GitHubCallbackPage").then((m) => ({ default: m.GitHubCallbackPage })),
);
const GitProvidersPage = lazy(() =>
  import("./pages/GitProvidersPage").then((m) => ({ default: m.GitProvidersPage })),
);
const LoadPage = lazy(() => import("./pages/LoadPage").then((m) => ({ default: m.LoadPage })));
const LogsPage = lazy(() => import("./pages/LogsPage").then((m) => ({ default: m.LogsPage })));
const MonitoringPage = lazy(() =>
  import("./pages/MonitoringPage").then((m) => ({ default: m.MonitoringPage })),
);
const ProjectsPage = lazy(() =>
  import("./pages/ProjectsPage").then((m) => ({ default: m.ProjectsPage })),
);
const RegistriesPage = lazy(() =>
  import("./pages/RegistriesPage").then((m) => ({ default: m.RegistriesPage })),
);
const RudderImagePage = lazy(() =>
  import("./pages/RudderImagePage").then((m) => ({ default: m.RudderImagePage })),
);
const S3DestinationsPage = lazy(() =>
  import("./pages/S3DestinationsPage").then((m) => ({ default: m.S3DestinationsPage })),
);
const ServersPage = lazy(() =>
  import("./pages/ServersPage").then((m) => ({ default: m.ServersPage })),
);
const ServicePage = lazy(() =>
  import("./pages/ServicePage").then((m) => ({ default: m.ServicePage })),
);
const SecurityPage = lazy(() =>
  import("./pages/SecurityPage").then((m) => ({ default: m.SecurityPage })),
);
const SystemPage = lazy(() =>
  import("./pages/SystemPage").then((m) => ({ default: m.SystemPage })),
);
const TemplatesPage = lazy(() =>
  import("./pages/TemplatesPage").then((m) => ({ default: m.TemplatesPage })),
);
const TokensPage = lazy(() =>
  import("./pages/TokensPage").then((m) => ({ default: m.TokensPage })),
);
const UsersPage = lazy(() => import("./pages/UsersPage").then((m) => ({ default: m.UsersPage })));

function Guard({ children }: { children: ReactNode }) {
  const { user, loading, needsSetup } = useAuth();
  const t = useT();
  const location = useLocation();
  if (loading) {
    return <p className="p-6 text-sm text-muted">{t("Loading Rudder…")}</p>;
  }
  if (needsSetup) {
    return <Navigate to="/setup" replace />;
  }
  if (!user) {
    // `rudder login` sends people to /tokens?cli_request=…; bring them back there after sign-in.
    const target = location.pathname + location.search;
    const next = target !== "/" ? `?next=${encodeURIComponent(target)}` : "";
    return <Navigate to={`/login${next}`} replace />;
  }
  return children;
}

function AppRoutes() {
  return (
    <Routes>
      <Route path="/setup" element={<SetupPage />} />
      <Route path="/login" element={<LoginPage />} />
      <Route
        path="/access"
        element={
          <Suspense fallback={null}>
            <AccessPortalPage />
          </Suspense>
        }
      />
      <Route
        element={
          <Guard>
            <Shell />
          </Guard>
        }
      >
        <Route index element={<DashboardPage />} />
        <Route path="servers" element={<ServersPage />} />
        <Route path="projects" element={<ProjectsPage />} />
        <Route path="services/:id" element={<ServicePage />} />
        <Route path="templates" element={<TemplatesPage />} />
        <Route path="monitoring" element={<MonitoringPage />} />
        <Route path="load" element={<LoadPage />} />
        <Route path="alerts" element={<AlertsPage />} />
        <Route path="logs" element={<LogsPage />} />
        <Route path="backups" element={<BackupsPage />} />
        <Route path="s3-destinations" element={<S3DestinationsPage />} />
        <Route path="rudder-image" element={<RudderImagePage />} />
        <Route path="cache" element={<CachePoliciesPage />} />
        <Route path="certificates" element={<CertificatesPage />} />
        <Route path="security" element={<SecurityPage />} />
        <Route path="access-admin" element={<AccessAdminPage />} />
        <Route path="registries" element={<RegistriesPage />} />
        <Route path="git" element={<GitProvidersPage />} />
        <Route path="git/github" element={<GitHubCallbackPage />} />
        <Route path="users" element={<UsersPage />} />
        <Route path="tokens" element={<TokensPage />} />
        <Route path="agents" element={<AgentsPage />} />
        <Route path="mcp/consent" element={<AgentConsentPage />} />
        <Route path="audit" element={<AuditPage />} />
        <Route path="system" element={<SystemPage />} />
        <Route path="account" element={<AccountPage />} />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}

function ThemedToaster() {
  const t = useT();
  const dark = useDarkTheme();
  return <Toaster theme={dark ? "dark" : "light"} containerAriaLabel={t("Notifications")} />;
}

/** Retry what may pass a moment later (no answer, busy, server errors), not what never will. */
function retry(failures: number, error: unknown): boolean {
  if (error instanceof ApiError && error.status >= 400 && error.status < 500) {
    return error.status === 408 || error.status === 429 ? failures < 3 : false;
  }
  return failures < 3;
}

export function App() {
  const [client] = useState(
    () =>
      new QueryClient({
        defaultOptions: {
          queries: {
            retry,
            // Back off up to 15 s, so an overloaded panel is not hammered by every open tab.
            retryDelay: (attempt) => Math.min(1000 * 2 ** attempt, 15_000),
          },
        },
      }),
  );
  return (
    <QueryClientProvider client={client}>
      <LanguageProvider>
        <AuthProvider>
          <BrowserRouter>
            <AppRoutes />
          </BrowserRouter>
          <ThemedToaster />
        </AuthProvider>
      </LanguageProvider>
    </QueryClientProvider>
  );
}
