import { useQuery } from "@tanstack/react-query";
import { lazy, Suspense, useState, type ReactNode } from "react";
import { Link, useParams, useSearchParams } from "react-router-dom";
import { api } from "../api";
import { BackupJobForm, BackupJobs } from "../components/backups";
import { ServiceCachePanel } from "../components/cache";
import { DatabasePanel, ServiceLogs } from "../components/DatabasePanel";
import { Icon } from "../components/icons";
import { LogViewer } from "../components/logs";
import { MetricsGrid, SERVICE_CHARTS } from "../components/metrics";
import {
  DeploymentsPanel,
  DeployImageCard,
  DomainsPanel,
  ServiceAccess,
  EnvironmentPanel,
  GitSourceCard,
  RudderImagesCard,
  type ServiceDetail,
} from "../components/service-panels";
import { StackLogs, StackOverview, StackSourceEditor, useStack } from "../components/stacks";
import {
  Alert,
  Badge,
  Button,
  buttonClass,
  Card,
  Dialog,
  Empty,
  KindTile,
  SectionNav,
  Skeleton,
  StatusDot,
  toneOf,
  type NavGroup,
} from "../components/ui";
import { formatDate, formatRelative } from "../format";
import { msg, useT } from "../i18n";
import type { Schemas } from "../types";

const TerminalPanel = lazy(() =>
  import("../components/terminal").then((module) => ({ default: module.TerminalPanel })),
);

type Tab =
  | "general"
  | "source"
  | "deployments"
  | "environment"
  | "domains"
  | "cache"
  | "metrics"
  | "logs"
  | "terminal"
  | "backups";

const SECTIONS: readonly NavGroup<Tab>[] = [
  {
    label: msg("Deploy"),
    items: [
      { key: "general", label: msg("General"), icon: "overview" },
      { key: "source", label: msg("Source"), icon: "git" },
      { key: "deployments", label: msg("Deployments"), icon: "rocket" },
    ],
  },
  {
    label: msg("Configure"),
    items: [
      { key: "environment", label: msg("Environment"), icon: "sliders" },
      { key: "domains", label: msg("Domains & SSL"), icon: "globe" },
      { key: "cache", label: msg("Cache"), icon: "zap" },
    ],
  },
  {
    label: msg("Observe"),
    items: [
      { key: "metrics", label: msg("Metrics"), icon: "activity" },
      { key: "logs", label: msg("Logs"), icon: "logs" },
      { key: "terminal", label: msg("Terminal"), icon: "terminal" },
    ],
  },
  {
    label: msg("Protect"),
    items: [{ key: "backups", label: msg("Backups"), icon: "archive" }],
  },
];

const TABS = new Set<string>(SECTIONS.flatMap((group) => group.items.map((item) => item.key)));

const isTab = (value: string | null): value is Tab => value !== null && TABS.has(value);

type Server = Schemas["ServerView"];
type Summary = Schemas["ServiceSummary"];
type Domain = Schemas["DomainView"];
type Deployment = Schemas["DeploymentView"];

export function ServicePage() {
  const t = useT();
  const { id = "" } = useParams();
  const [params, setParams] = useSearchParams();
  const tab: Tab = isTab(params.get("tab")) ? (params.get("tab") as Tab) : "general";

  const service = useQuery({
    queryKey: ["service", id],
    queryFn: () => api<ServiceDetail>(`/api/v1/services/${id}`),
    enabled: Boolean(id),
  });
  const summaries = useQuery({
    queryKey: ["services"],
    queryFn: () => api<Summary[]>("/api/v1/services"),
  });
  const servers = useQuery({
    queryKey: ["servers"],
    queryFn: () => api<Server[]>("/api/v1/servers"),
  });
  const domains = useQuery({
    queryKey: ["domains", id],
    queryFn: () => api<Domain[]>(`/api/v1/services/${id}/domains`),
    enabled: Boolean(id),
  });
  const deployments = useQuery({
    queryKey: ["deployments", id],
    queryFn: () => api<Deployment[]>(`/api/v1/services/${id}/deployments`),
    enabled: Boolean(id),
    refetchInterval: 4000,
  });

  const current = service.data;
  const isStack = current?.kind === "compose";
  const stack = useStack(id, isStack);
  const stackServices = isStack ? (stack.data?.services ?? []).map((item) => item.name) : [];
  const context = summaries.data?.find((item) => item.id === id);
  const server = servers.data?.find((item) => item.id === current?.server_id);
  const siteDomain =
    (domains.data ?? []).find((domain) => domain.published && !domain.is_preview) ??
    (domains.data ?? []).find((domain) => !domain.is_preview);

  function openTab(next: Tab) {
    setParams(next === "general" ? {} : { tab: next }, { replace: true });
  }

  if (service.error && !current) {
    return (
      <>
        <Breadcrumb />
        <Alert tone="error" title={t("Couldn't load this service")}>
          {service.error instanceof Error ? service.error.message : t("The panel did not answer.")}
        </Alert>
      </>
    );
  }

  return (
    <>
      <Breadcrumb context={context} />
      <header className="flex flex-wrap items-start justify-between gap-x-6 gap-y-4">
        <div className="flex min-w-0 items-start gap-3.5">
          {current ? <KindTile kind={current.kind} size={48} /> : <Skeleton className="size-12" />}
          <div className="min-w-0">
            {current ? (
              <>
                <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
                  <h1 className="truncate text-2xl leading-tight font-semibold tracking-[-0.015em] text-ink">
                    {current.name}
                  </h1>
                  <Badge value={current.status} />
                </div>
                <p className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-muted">
                  <Badge value={current.kind} tone="outline" />
                  {server ? (
                    <span className="inline-flex items-center gap-1.5">
                      <Icon name="server" size={14} className="text-faint" />
                      {server.name}
                    </span>
                  ) : null}
                  {current.spec?.image ? (
                    <span className="max-w-full truncate font-mono text-[13px]">
                      {current.spec.image}
                    </span>
                  ) : null}
                </p>
              </>
            ) : (
              <div className="space-y-2">
                <Skeleton className="h-7 w-48" />
                <Skeleton className="h-4 w-64" />
              </div>
            )}
          </div>
        </div>
        {siteDomain ? (
          <a
            href={`${siteDomain.ssl_mode === "none" ? "http" : "https"}://${siteDomain.host}${siteDomain.path_prefix}`}
            target="_blank"
            rel="noreferrer"
            className={buttonClass({ variant: "secondary" })}
          >
            <Icon name="external" />
            {siteDomain.host}
          </a>
        ) : null}
      </header>

      {current ? (
        <SummaryBar
          service={current}
          server={server}
          domains={domains.data ?? []}
          deployments={deployments.data ?? []}
          stack={stack.data}
        />
      ) : (
        <Skeleton className="h-24 w-full rounded-card" />
      )}

      <div className="grid items-start gap-x-8 gap-y-5 lg:grid-cols-[188px_minmax(0,1fr)]">
        <SectionNav groups={SECTIONS} active={tab} onChange={openTab} label={t("Service sections")} />
        <div className="min-w-0 space-y-6" aria-live="polite">
          {current ? (
            <Panel
              tab={tab}
              service={current}
              isStack={isStack}
              stackServices={stackServices}
              mainService={stack.data?.main_service}
            />
          ) : (
            <>
              <Skeleton className="h-48 w-full rounded-card" />
              <Skeleton className="h-32 w-full rounded-card" />
            </>
          )}
        </div>
      </div>
    </>
  );
}

function Breadcrumb({ context }: { context?: Summary }) {
  const t = useT();
  const crumb = "rounded-sm hover:text-ink hover:underline underline-offset-4";
  return (
    <nav aria-label={t("Breadcrumb")} className="flex flex-wrap items-center gap-1.5 text-sm text-muted">
      <Link to="/projects" className={crumb}>
        {t("Projects")}
      </Link>
      {context ? (
        <>
          <Icon name="chevronRight" size={13} className="text-faint" />
          <Link to={`/projects?project=${context.project_id}`} className={crumb}>
            {context.project_name}
          </Link>
          <Icon name="chevronRight" size={13} className="text-faint" />
          <Link
            to={`/projects?project=${context.project_id}&env=${context.environment_id}`}
            className={crumb}
          >
            {context.environment_name}
          </Link>
        </>
      ) : null}
    </nav>
  );
}

interface Fact {
  label: string;
  value: ReactNode;
  caption?: ReactNode;
}

function SummaryBar({
  service,
  server,
  domains,
  deployments,
  stack,
}: {
  service: ServiceDetail;
  server?: Server;
  domains: Domain[];
  deployments: Deployment[];
  stack?: Schemas["StackView"];
}) {
  const t = useT();
  const spec = service.spec as Record<string, unknown>;
  const latest = deployments[0];
  const site = domains.find((domain) => !domain.is_preview);

  const facts: Fact[] = [
    {
      label: t("Status"),
      value: (
        <span className="inline-flex items-center gap-2">
          <StatusDot status={service.status} />
          <span className="inline-block first-letter:uppercase">
            {t(service.status.replace(/_/g, " "))}
          </span>
        </span>
      ),
      caption: t("Created {time}", { time: formatDate(service.created_at) }),
    },
  ];

  if (service.kind === "database") {
    facts.push(
      {
        label: t("Engine"),
        value: `${String(spec.engine ?? "—")} ${String(spec.version ?? "")}`.trim(),
        caption: spec.container ? `${String(spec.container)}:${String(spec.port ?? "")}` : undefined,
      },
      {
        label: t("Public port"),
        value: spec.public_port ? String(spec.public_port) : t("Internal only"),
        caption: spec.public_port ? t("Reachable from outside") : t("Private to the server"),
      },
    );
  } else if (service.kind === "compose") {
    const running = stack?.services.filter((item) => item.running).length ?? 0;
    facts.push(
      {
        label: t("Containers"),
        value: stack ? `${running} / ${stack.services.length}` : "—",
        caption: stack?.main_service
          ? t("Main service: {name}", { name: stack.main_service })
          : undefined,
      },
      {
        label: t("Domains"),
        value: domains.length,
        caption: site?.host ?? t("No domains yet"),
      },
    );
  } else {
    facts.push(
      {
        label: t("Last deployment"),
        value: latest ? (
          <span className={`inline-block first-letter:uppercase ${deployTone(latest.status)}`}>
            {t(latest.status.replace(/_/g, " "))}
          </span>
        ) : (
          "—"
        ),
        caption: latest
          ? `${t(latest.trigger)} · ${formatRelative(latest.created_at)}`
          : t("No deployments yet"),
      },
      {
        label: t("Domains"),
        value: domains.length,
        caption: site?.host ?? t("No domains yet"),
      },
    );
  }

  facts.push({
    label: t("Server"),
    value: server?.name ?? t("Unknown"),
    caption: server
      ? [t(server.status), server.public_ip].filter(Boolean).join(" · ")
      : undefined,
  });

  return (
    <dl className="grid grid-cols-2 gap-px overflow-hidden rounded-card border border-line bg-line shadow-card lg:grid-cols-4">
      {facts.map((fact) => (
        <div key={fact.label} className="min-w-0 bg-surface px-4 py-3.5 sm:px-5">
          <dt className="text-xs font-medium text-muted">{fact.label}</dt>
          <dd className="mt-1 truncate text-lg font-semibold tracking-[-0.01em] text-ink tabular-nums">
            {fact.value}
          </dd>
          {fact.caption ? (
            <dd className="mt-0.5 truncate text-[13px] text-muted">{fact.caption}</dd>
          ) : null}
        </div>
      ))}
    </dl>
  );
}

function deployTone(status: string): string {
  const tone = toneOf(status);
  return tone === "danger"
    ? "text-danger"
    : tone === "warning"
      ? "text-amber"
      : tone === "success"
        ? "text-pine"
        : "text-ink";
}

function Panel({
  tab,
  service,
  isStack,
  stackServices,
  mainService,
}: {
  tab: Tab;
  service: ServiceDetail;
  isStack: boolean;
  stackServices: string[];
  mainService?: string | null;
}) {
  const t = useT();
  const id = service.id;
  const [creatingJob, setCreatingJob] = useState(false);

  switch (tab) {
    case "general":
      return service.kind === "database" ? (
        <DatabasePanel service={service as unknown as Schemas["ServiceView"]} />
      ) : service.kind === "compose" ? (
        <StackOverview serviceId={id} />
      ) : (
        <>
          <DeployImageCard service={service} />
          <RudderImagesCard service={service} />
        </>
      );
    case "source":
      return isStack ? <StackSourceEditor serviceId={id} /> : <GitSourceCard service={service} />;
    case "deployments":
      return <DeploymentsPanel serviceId={id} isStack={isStack} />;
    case "environment":
      return <EnvironmentPanel serviceId={id} isStack={isStack} />;
    case "domains":
      return (
        <>
          <ServiceAccess serviceId={id} kind={service.kind} spec={service.spec} />
          <DomainsPanel serviceId={id} stackServices={stackServices} mainService={mainService} />
        </>
      );
    case "cache":
      return <ServiceCachePanel serviceId={id} />;
    case "metrics":
      return <MetricsGrid charts={SERVICE_CHARTS} target={{ service_id: id }} />;
    case "logs":
      return (
        <>
          <Card
            title={t("Stored logs")}
            description={t("Searchable history collected from the app and its proxy.")}
          >
            <LogViewer fixed={{ service_id: id }} sources={["app", "proxy"]} />
          </Card>
          {isStack ? <StackLogs serviceId={id} /> : <ServiceLogs serviceId={id} />}
        </>
      );
    case "terminal":
      return (
        <Card
          title={t("Terminal")}
          description={t("An interactive shell inside the running container.")}
        >
          <Suspense fallback={<Empty>{t("Loading terminal…")}</Empty>}>
            <TerminalPanel serviceId={id} containers={stackServices} />
          </Suspense>
        </Card>
      );
    case "backups":
      return (
        <>
          <Card
            title={t("Backup jobs")}
            description={t("Scheduled and on-demand backups of this service's data.")}
            actions={
              <Button icon="plus" onClick={() => setCreatingJob(true)}>
                {t("New backup job")}
              </Button>
            }
          >
            <BackupJobs serviceId={id} />
          </Card>
          {creatingJob ? (
            <Dialog
              open
              variant="drawer"
              wide
              onClose={() => setCreatingJob(false)}
              title={t("New backup job")}
              description={t("Choose what to back up, when, and where to store it.")}
            >
              <BackupJobForm
                target={{
                  serviceId: id,
                  serverId: service.server_id,
                  isDatabase: service.kind === "database",
                }}
                onSaved={() => setCreatingJob(false)}
              />
            </Dialog>
          ) : null}
        </>
      );
  }
}
