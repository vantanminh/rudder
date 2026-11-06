import { useQuery } from "@tanstack/react-query";
import { useMemo, useRef, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import { api } from "../api";
import { useAuth } from "../auth";
import { Icon, type IconName } from "../components/icons";
import { ImageStatusCard } from "../components/image-status";
import {
  holdText,
  PanelLoadCard,
  reasonText,
  TopConsumers,
  useLoad,
  useLoadActions,
  type LoadView,
} from "../components/load";
import {
  Badge,
  buttonClass,
  Card,
  Empty,
  KindTile,
  Meter,
  Segmented,
  Skeleton,
  StatusDot,
} from "../components/ui";
import { errorMessage, formatBytes, formatRelative, formatTime } from "../format";
import { isIssue, statusOrder } from "../health";
import { useT } from "../i18n";
import type { Schemas } from "../types";

type Server = Schemas["ServerView"];
type Service = Schemas["ServiceSummary"];
type Usage = Schemas["ServerUsage"];
type Event = Schemas["EventView"];
type AlertRule = Schemas["AlertView"];

type Filter = "all" | "running" | "issues" | "stopped";

const LIVE = 10_000;
const PAGE = 8;

interface Issue {
  key: string;
  tone: "danger" | "warning" | "info";
  icon: IconName;
  title: ReactNode;
  detail?: ReactNode;
  action?: ReactNode;
}

export function DashboardPage() {
  const t = useT();
  const { user } = useAuth();
  const isAdmin = user?.role === "admin";
  const [filter, setFilter] = useState<Filter>("all");
  const servicesRef = useRef<HTMLDivElement>(null);

  const servers = useQuery({
    queryKey: ["servers"],
    queryFn: () => api<Server[]>("/api/v1/servers"),
    refetchInterval: LIVE,
  });
  const services = useQuery({
    queryKey: ["services"],
    queryFn: () => api<Service[]>("/api/v1/services"),
    refetchInterval: LIVE,
  });
  const overview = useQuery({
    queryKey: ["metrics-overview"],
    queryFn: () => api<Schemas["MetricsOverview"]>("/api/v1/metrics/overview"),
    refetchInterval: LIVE,
  });
  const alerts = useQuery({
    queryKey: ["alerts"],
    queryFn: () => api<AlertRule[]>("/api/v1/alerts"),
    refetchInterval: 30_000,
  });
  const events = useQuery({
    queryKey: ["events", "recent"],
    queryFn: () => api<Event[]>("/api/v1/events?limit=8"),
    refetchInterval: 30_000,
  });
  const load = useLoad();
  const { pause } = useLoadActions();

  const serverList = servers.data ?? [];
  const serviceList = services.data ?? [];
  const alertList = alerts.data ?? [];
  const usage = new Map((overview.data?.servers ?? []).map((row) => [row.server_id, row]));
  const serverName = new Map(serverList.map((server) => [server.id, server.name]));

  const online = serverList.filter((server) => server.status === "online").length;
  const offline = serverList.filter((server) => server.status === "offline");
  const running = serviceList.filter((service) => service.status === "running").length;
  const failing = serviceList.filter((service) => isIssue(service.status));
  const firing = alertList.filter((rule) => rule.enabled && rule.state === "firing");
  const pendingAlerts = alertList.filter((rule) => rule.enabled && rule.state === "pending");
  const loading = servers.isLoading || services.isLoading;

  const showIssues = () => {
    setFilter("issues");
    servicesRef.current?.scrollIntoView({ behavior: "smooth", block: "start" });
  };

  const issues: Issue[] = [];
  const view = load.data;
  if (view?.guard.level === "critical") {
    issues.push({
      key: "load",
      tone: "danger",
      icon: "gauge",
      title: t("The panel server is running out of memory"),
      detail: view.guard.reasons.map((reason) => reasonText(t, reason)).join(" · "),
      action: <IssueLink to="/load">{t("Open load controls")}</IssueLink>,
    });
  } else if (view?.guard.level === "busy") {
    issues.push({
      key: "load",
      tone: "warning",
      icon: "gauge",
      title: t("The panel server is busy"),
      detail: view.guard.reasons.map((reason) => reasonText(t, reason)).join(" · "),
      action: <IssueLink to="/load">{t("See what uses it")}</IssueLink>,
    });
  }
  if (view?.guard.settings.builds_paused) {
    issues.push({
      key: "paused",
      tone: "warning",
      icon: "pause",
      title: t("Builds are paused"),
      detail: view.builds.queued
        ? t("{count} builds wait until they are resumed.", { count: view.builds.queued })
        : t("New builds wait until they are resumed."),
      action: isAdmin ? (
        <button
          type="button"
          className={buttonClass({ variant: "secondary", size: "sm" })}
          disabled={pause.isPending}
          onClick={() => pause.mutate(false)}
        >
          <Icon name="play" size={14} />
          {t("Resume builds")}
        </button>
      ) : null,
    });
  }
  if (failing.length) {
    issues.push({
      key: "services",
      tone: "danger",
      icon: "rocket",
      title:
        failing.length === 1
          ? t("{count} service needs a look", { count: failing.length })
          : t("{count} services need a look", { count: failing.length }),
      detail: failing
        .slice(0, 3)
        .map((service) => service.name)
        .join(", "),
      action: (
        <button
          type="button"
          className={buttonClass({ variant: "secondary", size: "sm" })}
          onClick={showIssues}
        >
          {t("Show them")}
        </button>
      ),
    });
  }
  if (offline.length) {
    issues.push({
      key: "servers",
      tone: "danger",
      icon: "server",
      title:
        offline.length === 1
          ? t("{name} is offline", { name: offline[0]?.name ?? "" })
          : t("{count} servers are offline", { count: offline.length }),
      detail: offline[0]?.last_heartbeat_at
        ? t("Last seen {time}", { time: formatRelative(offline[0].last_heartbeat_at) })
        : undefined,
      action: <IssueLink to="/servers">{t("Servers")}</IssueLink>,
    });
  }
  if (firing.length) {
    issues.push({
      key: "alerts",
      tone: "danger",
      icon: "bell",
      title:
        firing.length === 1
          ? t("{count} alert is firing", { count: firing.length })
          : t("{count} alerts are firing", { count: firing.length }),
      detail: firing.map((rule) => rule.name).join(", "),
      action: <IssueLink to="/alerts">{t("Alerts")}</IssueLink>,
    });
  }

  const firstName = (user?.display_name || user?.email || "").split(/[\s@]/)[0] ?? "";
  const hour = new Date().getHours();
  const greeting =
    hour < 12
      ? t("Good morning, {name}", { name: firstName })
      : hour < 18
        ? t("Good afternoon, {name}", { name: firstName })
        : t("Good evening, {name}", { name: firstName });

  return (
    <>
      <div className="flex flex-wrap items-end justify-between gap-x-6 gap-y-4">
        <div className="min-w-0">
          <p className="text-sm font-medium text-pine">{greeting}</p>
          <h1 className="mt-1 text-2xl leading-tight font-semibold tracking-[-0.015em] text-ink">
            {t("Overview")}
          </h1>
          <p className="mt-1.5 max-w-2xl text-[14.5px] text-muted">
            {loading
              ? t("Checking your servers…")
              : t("{services} on {servers}. Refreshes every 10 seconds.", {
                  services:
                    serviceList.length === 1
                      ? t("{count} service", { count: serviceList.length })
                      : t("{count} services", { count: serviceList.length }),
                  servers:
                    serverList.length === 1
                      ? t("{count} server", { count: serverList.length })
                      : t("{count} servers", { count: serverList.length }),
                })}
          </p>
        </div>
        {isAdmin ? (
          <div className="flex flex-wrap items-center gap-2">
            <Link to="/templates" className={buttonClass({ variant: "secondary" })}>
              <Icon name="layers" />
              {t("Deploy a template")}
            </Link>
            <Link to="/projects" className={buttonClass()}>
              <Icon name="plus" />
              {t("New project")}
            </Link>
          </div>
        ) : null}
      </div>

      <HealthCard
        issues={issues}
        loading={loading}
        error={services.error ?? servers.error}
        updatedAt={services.dataUpdatedAt}
      />

      <div className="grid grid-cols-2 gap-3 sm:gap-4 xl:grid-cols-4">
        <StatTile
          to="/servers"
          icon="server"
          label={t("Servers online")}
          loading={servers.isLoading}
          value={online}
          total={serverList.length}
          caption={
            offline.length
              ? t("{count} offline", { count: offline.length })
              : serverList.length
                ? t("All reachable")
                : t("None added yet")
          }
          warn={offline.length > 0}
        />
        <StatTile
          to="/projects"
          icon="rocket"
          label={t("Services running")}
          loading={services.isLoading}
          value={running}
          total={serviceList.length}
          caption={
            failing.length
              ? t("{count} with problems", { count: failing.length })
              : serviceList.length - running
                ? t("{count} stopped", { count: serviceList.length - running })
                : serviceList.length
                  ? t("Everything is up")
                  : t("Nothing deployed yet")
          }
          warn={failing.length > 0}
        />
        <BuildsTile view={view} loading={load.isLoading} />
        <StatTile
          to="/alerts"
          icon="bell"
          label={t("Alerts firing")}
          loading={alerts.isLoading}
          value={firing.length}
          caption={
            firing[0]
              ? firing.length > 1
                ? t("{name} and {count} more", { name: firing[0].name, count: firing.length - 1 })
                : firing[0].name
              : alertList.length
                ? `${
                    alertList.length === 1
                      ? t("{count} rule", { count: alertList.length })
                      : t("{count} rules", { count: alertList.length })
                  }${pendingAlerts.length ? ` · ${t("{count} pending", { count: pendingAlerts.length })}` : ""}`
                : t("No rules yet")
          }
          warn={firing.length > 0}
        />
      </div>

      <div className="grid items-start gap-6 lg:grid-cols-[minmax(0,1fr)_340px] xl:grid-cols-[minmax(0,1fr)_380px]">
        <div className="grid min-w-0 gap-6">
          <div ref={servicesRef} className="scroll-mt-20">
            <ServicesCard
              services={serviceList}
              serverName={serverName}
              loading={services.isLoading}
              isAdmin={isAdmin}
              filter={filter}
              onFilter={setFilter}
            />
          </div>
          <TopConsumers />
        </div>
        <div className="grid min-w-0 gap-6">
          <PanelLoadCard />
          <ImageStatusCard />
          <ServersCard servers={serverList} usage={usage} loading={servers.isLoading} />
          <ActivityCard
            events={events.data ?? []}
            loading={events.isLoading}
            serverName={serverName}
          />
        </div>
      </div>
    </>
  );
}

function IssueLink({ to, children }: { to: string; children: ReactNode }) {
  return (
    <Link to={to} className={buttonClass({ variant: "secondary", size: "sm" })}>
      {children}
      <Icon name="arrowRight" size={14} />
    </Link>
  );
}

const issueTone: Record<Issue["tone"], string> = {
  danger: "border-danger-line bg-danger-soft text-danger",
  warning: "border-amber-line bg-amber-soft text-amber",
  info: "border-line bg-sunken text-ink-soft",
};

/** One sentence on the state of everything, and a row for each thing that needs a person. */
function HealthCard({
  issues,
  loading,
  error,
  updatedAt,
}: {
  issues: Issue[];
  loading: boolean;
  error: unknown;
  updatedAt: number;
}) {
  const t = useT();
  if (error) {
    return (
      <section className="flex items-start gap-4 rounded-card border border-danger-line bg-danger-soft p-5 text-danger">
        <Icon name="alert" size={22} className="mt-0.5 shrink-0" />
        <div>
          <h2 className="text-[15px] font-semibold">{t("Couldn't load the overview")}</h2>
          <p className="mt-1 text-sm">{errorMessage(error, t("The panel did not answer."))}</p>
        </div>
      </section>
    );
  }
  const danger = issues.some((issue) => issue.tone === "danger");
  const calm = !loading && issues.length === 0;
  return (
    <section className="overflow-hidden rounded-card border border-line bg-surface shadow-card">
      <div className="flex flex-wrap items-center gap-4 px-5 py-4 sm:px-6">
        <span
          className={`inline-flex size-10 shrink-0 items-center justify-center rounded-card border ${
            loading
              ? "border-line bg-sunken text-faint"
              : calm
                ? "border-pine-line bg-pine-soft text-pine"
                : danger
                  ? "border-danger-line bg-danger-soft text-danger"
                  : "border-amber-line bg-amber-soft text-amber"
          }`}
        >
          <Icon name={calm || loading ? "check" : "alert"} size={20} />
        </span>
        <div className="min-w-0 flex-1">
          <h2 className="text-base leading-snug font-semibold text-ink">
            {loading
              ? t("Checking your servers…")
              : calm
                ? t("Everything is running normally")
                : issues.length === 1
                  ? t("One thing needs attention")
                  : t("{count} things need attention", { count: issues.length })}
          </h2>
          <p className="mt-0.5 text-sm text-muted">
            {calm
              ? t("Servers are reachable, services are up and no alert is firing.")
              : t("Start with the first row; the panel keeps checking every few seconds.")}
          </p>
        </div>
        {updatedAt ? (
          <span className="text-xs text-faint" title={formatTime(new Date(updatedAt).toISOString())}>
            {t("Updated {time}", { time: formatRelative(new Date(updatedAt).toISOString()) })}
          </span>
        ) : null}
      </div>
      {issues.length ? (
        <ul className="divide-y divide-line border-t border-line">
          {issues.map((issue) => (
            <li
              key={issue.key}
              className="flex flex-wrap items-center gap-x-3 gap-y-2 px-5 py-3 sm:px-6"
            >
              <span
                className={`inline-flex size-8 shrink-0 items-center justify-center rounded-alert border ${issueTone[issue.tone]}`}
              >
                <Icon name={issue.icon} size={16} />
              </span>
              <div className="min-w-0 flex-1 basis-56">
                <p className="text-sm font-medium text-ink">{issue.title}</p>
                {issue.detail ? (
                  <p className="truncate text-[13px] text-muted">{issue.detail}</p>
                ) : null}
              </div>
              {issue.action ? <div className="shrink-0">{issue.action}</div> : null}
            </li>
          ))}
        </ul>
      ) : null}
    </section>
  );
}

function StatTile({
  to,
  icon,
  label,
  value,
  total,
  caption,
  loading,
  warn = false,
}: {
  to: string;
  icon: IconName;
  label: string;
  value: number;
  total?: number;
  caption: string;
  loading: boolean;
  warn?: boolean;
}) {
  const t = useT();
  const ratio = total ? (value / total) * 100 : null;
  return (
    <Link
      to={to}
      className="group min-w-0 rounded-card border border-line bg-surface p-4 shadow-card transition-[border-color,box-shadow] hover:border-line-strong hover:shadow-raised sm:p-5"
    >
      <div className="flex items-center justify-between text-[13px] font-medium text-muted">
        <span className="flex min-w-0 items-center gap-2">
          <span
            className={`hidden size-7 shrink-0 items-center justify-center rounded-alert sm:inline-flex ${warn ? "bg-danger-soft text-danger" : "bg-sunken text-ink-soft"}`}
          >
            <Icon name={icon} size={15} />
          </span>
          <span className="truncate">{label}</span>
        </span>
        <Icon
          name="arrowRight"
          size={14}
          className="-translate-x-1 text-faint opacity-0 transition group-hover:translate-x-0 group-hover:opacity-100"
        />
      </div>
      {loading ? (
        <Skeleton className="mt-4 h-8 w-20" />
      ) : (
        <p className="mt-4 flex items-baseline gap-1.5 tabular-nums">
          <span className="text-[28px] leading-none font-semibold tracking-[-0.02em] text-ink">
            {value}
          </span>
          {total != null ? (
            <span className="text-sm text-muted">{t("of {total}", { total })}</span>
          ) : null}
        </p>
      )}
      {total != null ? (
        <Meter
          value={loading ? null : ratio}
          label={t("{label} ratio", { label })}
          className="mt-3 [&>div]:bg-pine!"
        />
      ) : null}
      <p
        className={`mt-2.5 flex items-center gap-1.5 truncate text-[13px] ${warn ? "font-medium text-danger" : "text-muted"}`}
      >
        {warn ? <Icon name="alert" size={14} className="shrink-0" /> : null}
        <span className="truncate">{loading ? " " : caption}</span>
      </p>
    </Link>
  );
}

function BuildsTile({ view, loading }: { view: LoadView | undefined; loading: boolean }) {
  const t = useT();
  const running = view ? view.builds.running + view.builds.stack_builds : 0;
  const hold = view ? holdText(t, view.builds.hold) : null;
  const caption = !view
    ? t("Build queue")
    : view.builds.queued && hold
      ? hold
      : view.builds.queued
        ? t("{count} waiting", { count: view.builds.queued })
        : view.guard.settings.builds_paused
          ? t("Builds are paused")
          : t("Queue is empty");
  return (
    <Link
      to="/load"
      className="group min-w-0 rounded-card border border-line bg-surface p-4 shadow-card transition-[border-color,box-shadow] hover:border-line-strong hover:shadow-raised sm:p-5"
    >
      <div className="flex items-center justify-between text-[13px] font-medium text-muted">
        <span className="flex min-w-0 items-center gap-2">
          <span
            className={`hidden size-7 shrink-0 items-center justify-center rounded-alert sm:inline-flex ${hold ? "bg-amber-soft text-amber" : "bg-sunken text-ink-soft"}`}
          >
            <Icon name="zap" size={15} />
          </span>
          <span className="truncate">{t("Builds running")}</span>
        </span>
        <Icon
          name="arrowRight"
          size={14}
          className="-translate-x-1 text-faint opacity-0 transition group-hover:translate-x-0 group-hover:opacity-100"
        />
      </div>
      {loading ? (
        <Skeleton className="mt-4 h-8 w-20" />
      ) : (
        <p className="mt-4 flex items-baseline gap-1.5 tabular-nums">
          <span className="text-[28px] leading-none font-semibold tracking-[-0.02em] text-ink">
            {running}
          </span>
          {view?.builds.queued ? (
            <span className="text-sm text-muted">
              {t("+{count} waiting", { count: view.builds.queued })}
            </span>
          ) : null}
        </p>
      )}
      <div className="mt-3 flex h-1.5 gap-0.5" aria-hidden="true">
        {Array.from({ length: Math.max(view?.builds.concurrency ?? 1, 1) }, (_, index) => (
          <span
            key={index}
            className={`flex-1 rounded-full ${index < running ? "bg-pine animate-live" : "bg-sunken"}`}
          />
        ))}
      </div>
      <p
        className={`mt-2.5 flex items-center gap-1.5 truncate text-[13px] ${hold ? "font-medium text-amber" : "text-muted"}`}
      >
        {hold ? <Icon name="pause" size={14} className="shrink-0" /> : null}
        <span className="truncate">{loading ? " " : caption}</span>
      </p>
    </Link>
  );
}

function ServicesCard({
  services,
  serverName,
  loading,
  isAdmin,
  filter,
  onFilter,
}: {
  services: Service[];
  serverName: Map<string, string>;
  loading: boolean;
  isAdmin: boolean;
  filter: Filter;
  onFilter: (filter: Filter) => void;
}) {
  const t = useT();
  const [query, setQuery] = useState("");
  const [expanded, setExpanded] = useState(false);

  const counts = useMemo(
    () => ({
      all: services.length,
      running: services.filter((s) => s.status === "running").length,
      issues: services.filter((s) => isIssue(s.status)).length,
      stopped: services.filter((s) => s.status === "stopped").length,
    }),
    [services],
  );

  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return services
      .filter((service) => {
        if (filter === "running" && service.status !== "running") return false;
        if (filter === "issues" && !isIssue(service.status)) return false;
        if (filter === "stopped" && service.status !== "stopped") return false;
        if (!needle) return true;
        return [service.name, service.project_name, service.environment_name, service.kind]
          .join(" ")
          .toLowerCase()
          .includes(needle);
      })
      .sort(
        (a, b) =>
          (statusOrder[a.status] ?? 3) - (statusOrder[b.status] ?? 3) ||
          a.project_name.localeCompare(b.project_name) ||
          a.name.localeCompare(b.name),
      );
  }, [services, filter, query]);

  const shown = expanded ? visible : visible.slice(0, PAGE);

  if (!loading && services.length === 0) {
    return (
      <Card title={t("Services")}>
        <Empty
          icon="rocket"
          title={t("Deploy your first service")}
          action={
            isAdmin ? (
              <>
                <Link to="/templates" className={buttonClass({ variant: "secondary", size: "sm" })}>
                  {t("Browse templates")}
                </Link>
                <Link to="/projects" className={buttonClass({ size: "sm" })}>
                  {t("Create a project")}
                </Link>
              </>
            ) : null
          }
        >
          {t("Apps, databases and Compose stacks appear here once a project has them.")}
        </Empty>
      </Card>
    );
  }

  return (
    <Card
      title={t("Services")}
      description={t("Apps, databases and stacks across every project.")}
      flush
      footer={
        <>
          <span>
            {loading
              ? t("Loading…")
              : visible.length !== services.length
                ? t("Showing {shown} of {visible} ({total} total)", {
                    shown: shown.length,
                    visible: visible.length,
                    total: services.length,
                  })
                : t("Showing {shown} of {visible}", {
                    shown: shown.length,
                    visible: visible.length,
                  })}
          </span>
          <span className="flex items-center gap-3">
            {visible.length > PAGE ? (
              <button
                type="button"
                className="font-medium text-ink-soft hover:text-ink"
                onClick={() => setExpanded((value) => !value)}
              >
                {expanded ? t("Show fewer") : t("Show all {count}", { count: visible.length })}
              </button>
            ) : null}
            <Link to="/projects" className="inline-flex items-center gap-1 font-medium text-pine">
              {t("Projects")} <Icon name="arrowRight" size={14} />
            </Link>
          </span>
        </>
      }
    >
      <div className="flex flex-wrap items-center gap-2 border-b border-line px-5 pb-4 sm:px-6">
        <label className="relative min-w-0 flex-1 basis-48">
          <span className="sr-only">{t("Search services")}</span>
          <Icon
            name="search"
            className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-faint"
          />
          <input
            type="search"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder={t("Search by name, project or kind")}
            className="h-8 w-full rounded-control border border-line-strong bg-surface pr-3 pl-8 text-sm text-ink outline-none placeholder:text-faint focus:border-pine focus:shadow-[0_0_0_3px_color-mix(in_srgb,var(--pine)_14%,transparent)]"
          />
        </label>
        <div className="max-w-full overflow-x-auto">
          <Segmented<Filter>
            label={t("Filter services by status")}
            value={filter}
            onChange={onFilter}
            options={[
              ["all", t("All"), counts.all],
              ["running", t("Running"), counts.running],
              ["issues", t("Issues"), counts.issues],
              ["stopped", t("Stopped"), counts.stopped],
            ]}
          />
        </div>
      </div>
      {loading ? (
        <ul className="divide-y divide-line">
          {Array.from({ length: 4 }, (_, index) => (
            <li key={index} className="flex items-center gap-3 px-5 py-3.5 sm:px-6">
              <Skeleton className="size-8 rounded-alert" />
              <div className="flex-1 space-y-1.5">
                <Skeleton className="h-4 w-40" />
                <Skeleton className="h-3 w-56 max-w-full" />
              </div>
              <Skeleton className="h-5 w-16 rounded-full" />
            </li>
          ))}
        </ul>
      ) : visible.length === 0 ? (
        <Empty icon="search" title={t("No matching services")}>
          {t("Try another name or clear the filter.")}
        </Empty>
      ) : (
        <ul className="divide-y divide-line">
          {shown.map((service) => (
            <li key={service.id}>
              <Link
                to={`/services/${service.id}`}
                className="group grid grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-x-3 px-5 py-3 transition-colors hover:bg-paper sm:px-6 md:grid-cols-[auto_minmax(0,1fr)_120px_118px]"
              >
                <span className="relative">
                  <KindTile kind={service.kind} size={32} />
                  <span className="absolute -right-0.5 -bottom-0.5 inline-flex rounded-full bg-surface p-[2px]">
                    <StatusDot status={service.status} />
                  </span>
                </span>
                <span className="min-w-0">
                  <span className="block truncate text-sm font-medium text-ink group-hover:text-pine">
                    {service.name}
                  </span>
                  <span className="block truncate text-[13px] text-muted">
                    {service.project_name}
                    <span className="px-1 text-faint">/</span>
                    {service.environment_name}
                  </span>
                </span>
                <span className="hidden truncate text-[13px] text-muted md:flex md:items-center md:gap-1.5">
                  <Icon name="server" size={14} className="shrink-0 text-faint" />
                  <span className="truncate">
                    {serverName.get(service.server_id) ?? t("Unknown")}
                  </span>
                </span>
                <span className="flex items-center justify-end gap-2">
                  <Badge value={service.status} />
                  <Icon
                    name="chevronRight"
                    size={14}
                    className="hidden text-faint transition-transform group-hover:translate-x-0.5 sm:block"
                  />
                </span>
              </Link>
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}

function ServersCard({
  servers,
  usage,
  loading,
}: {
  servers: Server[];
  usage: Map<string, Usage>;
  loading: boolean;
}) {
  const t = useT();
  return (
    <Card
      title={t("Servers")}
      flush
      actions={
        <Link to="/monitoring" className={buttonClass({ variant: "ghost", size: "sm" })}>
          {t("Monitoring")}
          <Icon name="arrowRight" size={14} />
        </Link>
      }
    >
      {loading ? (
        <div className="space-y-3 px-5 pb-5 sm:px-6">
          <Skeleton className="h-16 w-full" />
          <Skeleton className="h-16 w-full" />
        </div>
      ) : servers.length === 0 ? (
        <Empty icon="server" title={t("No servers yet")}>
          {t("{link} to start deploying.", { link: <Link to="/servers">{t("Add a server")}</Link> })}
        </Empty>
      ) : (
        <ul className="divide-y divide-line border-t border-line">
          {servers.map((server) => {
            const row = usage.get(server.id);
            return (
              <li key={server.id} className="px-5 py-4 sm:px-6">
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0">
                    <p className="flex items-center gap-2 text-sm font-medium text-ink">
                      <StatusDot status={server.status} />
                      <span className="truncate">{server.name}</span>
                      {server.kind === "local" ? (
                        <span className="text-xs font-normal text-faint">{t("panel")}</span>
                      ) : null}
                    </p>
                    <p className="mt-0.5 truncate pl-4 font-mono text-xs text-muted">
                      {[server.public_ip, server.hostname].filter(Boolean).join(" · ") ||
                        server.kind}
                    </p>
                  </div>
                  <Badge value={server.status} />
                </div>
                {row && server.status === "online" ? (
                  <dl className="mt-3 grid grid-cols-3 gap-3 pl-4">
                    <Gauge label={t("CPU")} value={row.cpu_percent} />
                    <Gauge label={t("Memory")} value={row.memory_percent} />
                    <Gauge label={t("Disk")} value={row.disk_percent} />
                  </dl>
                ) : (
                  <p className="mt-2 pl-4 text-xs text-muted">
                    {server.last_heartbeat_at
                      ? t("Last seen {time}", { time: formatRelative(server.last_heartbeat_at) })
                      : server.memory_bytes
                        ? `${server.cpu_count ?? "?"} vCPU · ${formatBytes(server.memory_bytes)}`
                        : t("No samples yet")}
                  </p>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </Card>
  );
}

function Gauge({ label, value }: { label: string; value: number | null | undefined }) {
  const t = useT();
  return (
    <div className="min-w-0">
      <div className="flex items-baseline justify-between gap-1 text-xs">
        <dt className="text-muted">{label}</dt>
        <dd className="font-medium text-ink-soft tabular-nums">
          {value == null ? "—" : `${Math.round(value)}%`}
        </dd>
      </div>
      <Meter value={value} label={t("{label} usage", { label })} className="mt-1.5" />
    </div>
  );
}

const infoStyle: [string, IconName] = ["border-line bg-sunken text-ink-soft", "info"];

const severityStyle: Record<string, [string, IconName]> = {
  critical: ["border-danger-line bg-danger-soft text-danger", "alert"],
  error: ["border-danger-line bg-danger-soft text-danger", "alert"],
  warning: ["border-amber-line bg-amber-soft text-amber", "alert"],
  info: ["border-line bg-sunken text-ink-soft", "info"],
};

function ActivityCard({
  events,
  loading,
  serverName,
}: {
  events: Event[];
  loading: boolean;
  serverName: Map<string, string>;
}) {
  const t = useT();
  return (
    <Card
      title={t("Recent activity")}
      flush
      actions={
        <Link to="/logs" className={buttonClass({ variant: "ghost", size: "sm" })}>
          {t("Logs")}
          <Icon name="arrowRight" size={14} />
        </Link>
      }
    >
      {loading ? (
        <div className="space-y-3 px-5 pb-5 sm:px-6">
          <Skeleton className="h-10 w-full" />
          <Skeleton className="h-10 w-full" />
          <Skeleton className="h-10 w-full" />
        </div>
      ) : events.length === 0 ? (
        <Empty icon="history" title={t("All quiet")}>
          {t("Deploys, backups and alerts will show up here.")}
        </Empty>
      ) : (
        <ol className="border-t border-line px-5 py-2 sm:px-6">
          {events.map((event, index) => {
            const [cls, icon] = event.kind.startsWith("guard.")
              ? ([
                  (severityStyle[event.severity] ?? infoStyle)[0],
                  "gauge",
                ] as [string, IconName])
              : (severityStyle[event.severity] ?? infoStyle);
            return (
              <li key={event.id} className="relative flex gap-3 py-2.5">
                {index < events.length - 1 ? (
                  <span
                    className="absolute top-9 bottom-0 left-[13px] w-px bg-line"
                    aria-hidden="true"
                  />
                ) : null}
                <span
                  className={`relative inline-flex size-7 shrink-0 items-center justify-center rounded-full border ${cls}`}
                >
                  <Icon name={icon} size={14} />
                </span>
                <div className="min-w-0 flex-1">
                  <p className="flex items-baseline justify-between gap-2">
                    <span className="truncate text-sm font-medium text-ink">{event.title}</span>
                    <time
                      dateTime={event.created_at}
                      title={formatTime(event.created_at)}
                      className="shrink-0 text-xs text-faint tabular-nums"
                    >
                      {formatRelative(event.created_at)}
                    </time>
                  </p>
                  <p className="line-clamp-2 text-[13px] text-muted">
                    {event.server_id && serverName.get(event.server_id) ? (
                      <span className="text-ink-soft">{serverName.get(event.server_id)} · </span>
                    ) : null}
                    {event.message}
                  </p>
                </div>
              </li>
            );
          })}
        </ol>
      )}
    </Card>
  );
}

