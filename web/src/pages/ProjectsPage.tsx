import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo, useState, type FormEvent } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { toast } from "sonner";
import { api, ApiError } from "../api";
import { useAuth } from "../auth";
import { Icon } from "../components/icons";
import { DeleteDialog } from "../components/DeleteDialog";
import { CreateStackFields } from "../components/stacks";
import {
  Alert,
  Badge,
  Button,
  buttonClass,
  Card,
  Dialog,
  Empty,
  Field,
  KindTile,
  PageHeader,
  PickField,
  QuickPick,
  SearchBox,
  Segmented,
  Select,
  Skeleton,
  StatCard,
  StatusDot,
  TextInput,
} from "../components/ui";
import { formatDate, formatTime } from "../format";
import { isIssue, statusOrder, summarize, type HealthCounts } from "../health";
import { useT } from "../i18n";
import { ENVIRONMENT_NAMES, IMAGES, MAIN_SERVICES, SERVICE_KINDS } from "../presets";
import type { Schemas } from "../types";

type Project = Schemas["ProjectView"];
type Environment = Schemas["EnvironmentView"];
type Service = Schemas["ServiceSummary"];
type Server = Schemas["ServerView"];

type Filter = "all" | "running" | "issues" | "stopped";
type DialogName =
  | "project"
  | "environment"
  | "service"
  | "delete-project"
  | "delete-environment";

const LIVE = 10_000;

export function ProjectsPage() {
  const t = useT();
  const { user } = useAuth();
  const isAdmin = user?.role === "admin";
  const [params, setParams] = useSearchParams();
  const [dialog, setDialog] = useState<DialogName | null>(null);
  const [projectQuery, setProjectQuery] = useState("");

  const projects = useQuery({
    queryKey: ["projects"],
    queryFn: () => api<Project[]>("/api/v1/projects"),
  });
  const services = useQuery({
    queryKey: ["services"],
    queryFn: () => api<Service[]>("/api/v1/services"),
    refetchInterval: LIVE,
  });
  const servers = useQuery({
    queryKey: ["servers"],
    queryFn: () => api<Server[]>("/api/v1/servers"),
  });

  const projectList = projects.data ?? [];
  const serviceList = services.data ?? [];
  const activeId =
    projectList.find((project) => project.id === params.get("project"))?.id ??
    projectList[0]?.id ??
    null;
  const active = projectList.find((project) => project.id === activeId) ?? null;

  const environments = useQuery({
    queryKey: ["environments", activeId],
    queryFn: () => api<Environment[]>(`/api/v1/projects/${activeId}/environments`),
    enabled: Boolean(activeId),
  });
  const environmentList = environments.data ?? [];
  const activeEnvironment =
    environmentList.find((environment) => environment.id === params.get("env")) ??
    environmentList[0] ??
    null;

  const perProject = useMemo(() => {
    const groups = new Map<string, Service[]>();
    for (const service of serviceList) {
      groups.set(service.project_id, [...(groups.get(service.project_id) ?? []), service]);
    }
    return new Map([...groups].map(([id, list]) => [id, summarize(list)]));
  }, [serviceList]);

  const totals = summarize(serviceList);
  const projectsWithServices = perProject.size;
  const loading = projects.isLoading || services.isLoading;

  function selectProject(id: string) {
    setParams({ project: id });
  }

  function selectEnvironment(id: string) {
    if (activeId) setParams({ project: activeId, env: id });
  }

  const visibleProjects = useMemo(() => {
    const needle = projectQuery.trim().toLowerCase();
    return needle
      ? projectList.filter((project) => project.name.toLowerCase().includes(needle))
      : projectList;
  }, [projectList, projectQuery]);

  return (
    <>
      <PageHeader
        title={t("Projects")}
        description={t(
          "Group services by project and environment, then open one to deploy, configure and monitor it.",
        )}
        actions={
          isAdmin ? (
            <>
              <Link to="/templates" className={buttonClass({ variant: "secondary" })}>
                <Icon name="layers" />
                {t("Deploy a template")}
              </Link>
              <Button icon="plus" onClick={() => setDialog("project")}>
                {t("New project")}
              </Button>
            </>
          ) : null
        }
      />

      {projects.error || services.error ? (
        <Alert tone="error" title={t("Couldn't load projects")}>
          {t("The panel did not answer.")}
        </Alert>
      ) : null}

      <div className="grid grid-cols-2 gap-3 sm:gap-4 xl:grid-cols-4">
        <StatCard
          icon="folder"
          label={t("Projects")}
          loading={loading}
          value={projectList.length}
          caption={
            projectList.length
              ? t("{count} with services", { count: projectsWithServices })
              : t("None yet")
          }
        />
        <StatCard
          icon="layers"
          label={t("Services")}
          loading={loading}
          value={totals.total}
          caption={
            totals.total ? t("{count} running", { count: totals.running }) : t("Nothing deployed yet")
          }
        />
        <StatCard
          icon="rocket"
          label={t("Running")}
          loading={loading}
          value={totals.running}
          total={totals.total}
          caption={
            totals.total === 0
              ? t("Nothing deployed yet")
              : totals.running === totals.total
                ? t("Everything is up")
                : t("{count} stopped or starting", { count: totals.total - totals.running })
          }
        />
        <StatCard
          icon="alert"
          label={t("Needs attention")}
          loading={loading}
          value={totals.issues}
          tone={totals.issues > 0 ? "danger" : "neutral"}
          caption={
            totals.issues > 0
              ? totals.issues === 1
                ? t("{count} service failing or degraded", { count: totals.issues })
                : t("{count} services failing or degraded", { count: totals.issues })
              : t("No problems")
          }
        />
      </div>

      {!loading && projectList.length === 0 ? (
        <Card>
          <Empty
            icon="folder"
            title={t("Create your first project")}
            action={
              isAdmin ? (
                <Button icon="plus" onClick={() => setDialog("project")}>
                  {t("New project")}
                </Button>
              ) : null
            }
          >
            {t("A project keeps the services, databases and stacks of one product together.")}
          </Empty>
        </Card>
      ) : (
        <div className="grid items-start gap-6 lg:grid-cols-[300px_minmax(0,1fr)]">
          <ProjectList
            projects={visibleProjects}
            total={projectList.length}
            loading={projects.isLoading}
            stats={perProject}
            activeId={activeId}
            query={projectQuery}
            onQuery={setProjectQuery}
            onSelect={selectProject}
          />
          {active ? (
            <ProjectDetail
              project={active}
              stats={perProject.get(active.id)}
              environments={environmentList}
              environment={activeEnvironment}
              environmentsLoading={environments.isLoading}
              services={serviceList.filter(
                (service) => service.environment_id === activeEnvironment?.id,
              )}
              servers={servers.data ?? []}
              servicesLoading={services.isLoading}
              isAdmin={isAdmin}
              onEnvironment={selectEnvironment}
              onDialog={setDialog}
            />
          ) : (
            <Card>
              <Skeleton className="h-40 w-full" />
            </Card>
          )}
        </div>
      )}

      {dialog === "project" ? (
        <NewProjectDialog
          onClose={() => setDialog(null)}
          onCreated={(project) => {
            setDialog(null);
            selectProject(project.id);
          }}
        />
      ) : null}
      {dialog === "environment" && active ? (
        <NewEnvironmentDialog
          project={active}
          onClose={() => setDialog(null)}
          onCreated={(environment) => {
            setDialog(null);
            selectEnvironment(environment.id);
          }}
        />
      ) : null}
      {dialog === "delete-project" && active ? (
        <DeleteProjectDialog
          project={active}
          serviceCount={perProject.get(active.id)?.total ?? 0}
          onClose={() => setDialog(null)}
          onDeleted={() => {
            setDialog(null);
            setParams({});
          }}
        />
      ) : null}
      {dialog === "delete-environment" && active && activeEnvironment ? (
        <DeleteEnvironmentDialog
          project={active}
          environment={activeEnvironment}
          serviceCount={
            serviceList.filter((service) => service.environment_id === activeEnvironment.id).length
          }
          onClose={() => setDialog(null)}
          onDeleted={() => {
            setDialog(null);
            setParams({ project: active.id });
          }}
        />
      ) : null}
      {dialog === "service" && active && activeEnvironment ? (
        <NewServiceDialog
          environment={activeEnvironment}
          servers={servers.data ?? []}
          onClose={() => setDialog(null)}
        />
      ) : null}
    </>
  );
}

function HealthBar({ stats }: { stats: HealthCounts | undefined }) {
  const t = useT();
  if (!stats || stats.total === 0) {
    return <div className="h-1.5 rounded-full bg-sunken" aria-hidden="true" />;
  }
  const width = (count: number) => `${(count / stats.total) * 100}%`;
  return (
    <div
      role="img"
      aria-label={t("{running} running, {issues} with issues, {other} idle", {
        running: stats.running,
        issues: stats.issues,
        other: stats.other,
      })}
      className="flex h-1.5 gap-px overflow-hidden rounded-full bg-sunken"
    >
      <div className="bg-pine" style={{ width: width(stats.running) }} />
      <div className="bg-danger" style={{ width: width(stats.issues) }} />
      <div className="bg-faint/50" style={{ width: width(stats.other) }} />
    </div>
  );
}

function ProjectList({
  projects,
  total,
  loading,
  stats,
  activeId,
  query,
  onQuery,
  onSelect,
}: {
  projects: Project[];
  total: number;
  loading: boolean;
  stats: Map<string, HealthCounts>;
  activeId: string | null;
  query: string;
  onQuery: (value: string) => void;
  onSelect: (id: string) => void;
}) {
  const t = useT();
  return (
    <Card title={t("All projects")} flush className="lg:sticky lg:top-20">
      {total > 5 ? (
        <div className="px-5 pb-3 sm:px-6">
          <SearchBox
            value={query}
            onChange={onQuery}
            label={t("Search projects")}
            placeholder={t("Search projects")}
          />
        </div>
      ) : null}
      {loading ? (
        <div className="space-y-3 px-5 pb-5 sm:px-6">
          <Skeleton className="h-12 w-full" />
          <Skeleton className="h-12 w-full" />
        </div>
      ) : projects.length === 0 ? (
        <Empty>{t("No matching projects")}</Empty>
      ) : (
        <ul className="space-y-1 px-2 pb-3">
          {projects.map((project) => {
            const row = stats.get(project.id);
            const selected = project.id === activeId;
            return (
              <li key={project.id}>
                <button
                  type="button"
                  aria-current={selected ? "true" : undefined}
                  onClick={() => onSelect(project.id)}
                  className={`w-full rounded-alert border px-3 py-2.5 text-left transition-colors ${
                    selected
                      ? "border-pine-line bg-pine-soft/60"
                      : "border-transparent hover:bg-paper"
                  }`}
                >
                  <span className="flex items-center justify-between gap-2">
                    <span className="truncate text-sm font-medium text-ink">{project.name}</span>
                    {row && row.issues > 0 ? <StatusDot status="failed" /> : null}
                  </span>
                  <span className="mt-0.5 block text-[13px] text-muted">
                    {row
                      ? row.total === 1
                        ? t("{count} service", { count: row.total })
                        : t("{count} services", { count: row.total })
                      : t("No services")}
                  </span>
                  <span className="mt-2 block">
                    <HealthBar stats={row} />
                  </span>
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </Card>
  );
}

function ProjectDetail({
  project,
  stats,
  environments,
  environment,
  environmentsLoading,
  services,
  servers,
  servicesLoading,
  isAdmin,
  onEnvironment,
  onDialog,
}: {
  project: Project;
  stats: HealthCounts | undefined;
  environments: Environment[];
  environment: Environment | null;
  environmentsLoading: boolean;
  services: Service[];
  servers: Server[];
  servicesLoading: boolean;
  isAdmin: boolean;
  onEnvironment: (id: string) => void;
  onDialog: (name: DialogName) => void;
}) {
  const t = useT();
  const [filter, setFilter] = useState<Filter>("all");
  const [query, setQuery] = useState("");
  const serverName = new Map(servers.map((server) => [server.id, server.name]));

  const counts = useMemo(() => {
    const row = summarize(services);
    return { all: row.total, running: row.running, issues: row.issues, stopped: row.stopped };
  }, [services]);

  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return services
      .filter((service) => {
        if (filter === "running" && service.status !== "running") return false;
        if (filter === "issues" && !isIssue(service.status)) return false;
        if (filter === "stopped" && service.status !== "stopped") return false;
        return !needle || `${service.name} ${service.kind}`.toLowerCase().includes(needle);
      })
      .sort(
        (a, b) =>
          (statusOrder[a.status] ?? 3) - (statusOrder[b.status] ?? 3) || a.name.localeCompare(b.name),
      );
  }, [services, filter, query]);

  const details = [
    t("Created {time}", { time: formatDate(project.created_at) }),
    stats
      ? stats.total === 1
        ? t("{count} service", { count: stats.total })
        : t("{count} services", { count: stats.total })
      : t("No services"),
    environments.length === 1
      ? t("{count} environment", { count: environments.length })
      : t("{count} environments", { count: environments.length }),
  ];

  return (
    <Card
      title={<span className="text-lg">{project.name}</span>}
      description={
        <>
          {project.description ? <span className="block">{project.description}</span> : null}
          <span title={formatTime(project.created_at)}>{details.join(" · ")}</span>
        </>
      }
      actions={
        isAdmin ? (
          <>
            <Button variant="secondary" icon="plus" onClick={() => onDialog("environment")}>
              {t("Add environment")}
            </Button>
            <Button icon="plus" disabled={!environment} onClick={() => onDialog("service")}>
              {t("Add service")}
            </Button>
            <Button
              variant="danger"
              disabled={!environment}
              onClick={() => onDialog("delete-environment")}
            >
              {t("Delete environment")}
            </Button>
            <Button variant="danger" onClick={() => onDialog("delete-project")}>
              {t("Delete project")}
            </Button>
          </>
        ) : null
      }
      flush
    >
      {environmentsLoading ? (
        <div className="px-5 pb-5 sm:px-6">
          <Skeleton className="h-9 w-64" />
        </div>
      ) : environments.length === 0 ? (
        <Empty
          icon="layers"
          title={t("No environments yet")}
          action={
            isAdmin ? (
              <Button icon="plus" onClick={() => onDialog("environment")}>
                {t("Add environment")}
              </Button>
            ) : null
          }
        >
          {t("Add an environment such as prod or staging before creating services.")}
        </Empty>
      ) : (
        <>
          <div className="flex flex-wrap items-center gap-2 border-b border-line px-5 pb-4 sm:px-6">
            <div className="max-w-full overflow-x-auto">
              <Segmented
                label={t("Environment")}
                value={environment?.id ?? ""}
                onChange={onEnvironment}
                options={environments.map(
                  (item) =>
                    [
                      item.id,
                      item.name,
                      services.length && item.id === environment?.id ? services.length : undefined,
                    ] as const,
                )}
              />
            </div>
            <SearchBox
              className="ml-auto w-full sm:w-60"
              value={query}
              onChange={setQuery}
              label={t("Search services")}
              placeholder={t("Search by name or kind")}
            />
            <div className="max-w-full overflow-x-auto">
              <Segmented<Filter>
                label={t("Filter services by status")}
                value={filter}
                onChange={setFilter}
                options={[
                  ["all", t("All"), counts.all],
                  ["running", t("Running"), counts.running],
                  ["issues", t("Issues"), counts.issues],
                  ["stopped", t("Stopped"), counts.stopped],
                ]}
              />
            </div>
          </div>
          {servicesLoading ? (
            <ul className="divide-y divide-line">
              {Array.from({ length: 3 }, (_, index) => (
                <li key={index} className="flex items-center gap-3 px-5 py-3.5 sm:px-6">
                  <Skeleton className="size-9" />
                  <div className="flex-1 space-y-1.5">
                    <Skeleton className="h-4 w-40" />
                    <Skeleton className="h-3 w-24" />
                  </div>
                  <Skeleton className="h-5 w-16 rounded-full" />
                </li>
              ))}
            </ul>
          ) : services.length === 0 ? (
            <Empty
              icon="rocket"
              title={t("No services in this environment")}
              action={
                isAdmin ? (
                  <>
                    <Link
                      to="/templates"
                      className={buttonClass({ variant: "secondary", size: "sm" })}
                    >
                      {t("Browse templates")}
                    </Link>
                    <Button size="sm" icon="plus" onClick={() => onDialog("service")}>
                      {t("Add service")}
                    </Button>
                  </>
                ) : null
              }
            >
              {t("Deploy an app, a database or a Compose stack here.")}
            </Empty>
          ) : visible.length === 0 ? (
            <Empty icon="search" title={t("No matching services")}>
              {t("Try another name or clear the filter.")}
            </Empty>
          ) : (
            <>
              <div className="hidden grid-cols-[auto_minmax(0,1fr)_96px_140px_118px] gap-x-3 border-b border-line bg-paper/60 px-5 py-2 text-xs font-medium text-muted sm:px-6 md:grid">
                <span className="w-9" />
                <span>{t("Service")}</span>
                <span>{t("Kind")}</span>
                <span>{t("Server")}</span>
                <span className="text-right">{t("Status")}</span>
              </div>
              <ul className="divide-y divide-line">
                {visible.map((service) => (
                  <li key={service.id}>
                    <Link
                      to={`/services/${service.id}`}
                      className="group grid grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-x-3 px-5 py-3 transition-colors hover:bg-paper sm:px-6 md:grid-cols-[auto_minmax(0,1fr)_96px_140px_118px]"
                    >
                      <KindTile kind={service.kind} />
                      <span className="min-w-0">
                        <span className="block truncate text-sm font-medium text-ink group-hover:text-pine">
                          {service.name}
                        </span>
                        <span className="block truncate text-[13px] text-muted md:hidden">
                          {t(service.kind)} · {serverName.get(service.server_id) ?? t("Unknown")}
                        </span>
                      </span>
                      <span className="hidden justify-self-start md:block">
                        <Badge value={service.kind} tone="outline" />
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
            </>
          )}
        </>
      )}
    </Card>
  );
}

function NewProjectDialog({
  onClose,
  onCreated,
}: {
  onClose: () => void;
  onCreated: (project: Project) => void;
}) {
  const t = useT();
  const queryClient = useQueryClient();
  const [name, setName] = useState("");
  const create = useMutation({
    mutationFn: () =>
      api<Project>("/api/v1/projects", { method: "POST", body: JSON.stringify({ name }) }),
    onSuccess: async (project) => {
      toast.success(t("Project created"));
      await queryClient.invalidateQueries({ queryKey: ["projects"] });
      onCreated(project);
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : t("Could not create project")),
  });
  return (
    <Dialog
      open
      onClose={onClose}
      title={t("New project")}
      description={t("A project keeps the services, databases and stacks of one product together.")}
    >
      <form
        className="grid gap-5"
        onSubmit={(event: FormEvent) => {
          event.preventDefault();
          create.mutate();
        }}
      >
        <Field label={t("Name")}>
          <TextInput
            required
            autoFocus
            value={name}
            onChange={(event) => setName(event.target.value)}
          />
        </Field>
        <div className="flex justify-end gap-2">
          <Button type="button" variant="secondary" onClick={onClose}>
            {t("Cancel")}
          </Button>
          <Button type="submit" disabled={create.isPending}>
            {t("Create")}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}

function NewEnvironmentDialog({
  project,
  onClose,
  onCreated,
}: {
  project: Project;
  onClose: () => void;
  onCreated: (environment: Environment) => void;
}) {
  const t = useT();
  const queryClient = useQueryClient();
  const [name, setName] = useState("prod");
  const create = useMutation({
    mutationFn: () =>
      api<Environment>(`/api/v1/projects/${project.id}/environments`, {
        method: "POST",
        body: JSON.stringify({ name }),
      }),
    onSuccess: async (environment) => {
      toast.success(t("Environment created"));
      await queryClient.invalidateQueries({ queryKey: ["environments", project.id] });
      onCreated(environment);
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : t("Could not create environment")),
  });
  return (
    <Dialog
      open
      onClose={onClose}
      title={t("Add environment")}
      description={t("An environment is a separate copy of {project}, such as prod or staging.", {
        project: project.name,
      })}
    >
      <form
        className="grid gap-5"
        onSubmit={(event: FormEvent) => {
          event.preventDefault();
          create.mutate();
        }}
      >
        <Field
          label={t("Name")}
          presets={
            <QuickPick
              label={t("Environment name presets")}
              options={ENVIRONMENT_NAMES}
              value={name}
              onPick={setName}
            />
          }
        >
          <TextInput
            required
            autoFocus
            value={name}
            onChange={(event) => setName(event.target.value)}
          />
        </Field>
        <div className="flex justify-end gap-2">
          <Button type="button" variant="secondary" onClick={onClose}>
            {t("Cancel")}
          </Button>
          <Button type="submit" disabled={create.isPending}>
            {t("Add environment")}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}

function NewServiceDialog({
  environment,
  servers,
  onClose,
}: {
  environment: Environment;
  servers: Server[];
  onClose: () => void;
}) {
  const t = useT();
  const queryClient = useQueryClient();
  const [serviceName, setServiceName] = useState("");
  const [kind, setKind] = useState("application");
  const [serverId, setServerId] = useState("");
  const [image, setImage] = useState("");
  const [engine, setEngine] = useState("postgres");
  const [version, setVersion] = useState("");
  const [stackSource, setStackSource] = useState("inline");
  const [stackYaml, setStackYaml] = useState("");
  const [stackGit, setStackGit] = useState<Schemas["StackGit"]>({
    provider_id: "",
    repo: "",
    branch: "main",
    compose_file: "docker-compose.yml",
  });
  const [mainService, setMainService] = useState("");
  const [stackProject, setStackProject] = useState("");
  const providers = useQuery({
    queryKey: ["git-providers"],
    queryFn: () => api<Schemas["GitProviderView"][]>("/api/v1/git-providers"),
    enabled: kind === "compose",
  });
  const engines = useQuery({
    queryKey: ["database-engines"],
    queryFn: () => api<Schemas["DatabaseEngineView"][]>("/api/v1/database-engines"),
    enabled: kind === "database",
  });
  const engineVersions = engines.data?.find((item) => item.engine === engine)?.versions ?? [];
  const target = serverId || servers[0]?.id;

  const create = useMutation({
    mutationFn: () =>
      kind === "database"
        ? api(`/api/v1/environments/${environment.id}/databases`, {
            method: "POST",
            body: JSON.stringify({
              name: serviceName,
              engine,
              version: version || engineVersions[0],
              server_id: target,
            }),
          })
        : kind === "compose"
          ? api(`/api/v1/environments/${environment.id}/stacks`, {
              method: "POST",
              body: JSON.stringify({
                name: serviceName,
                server_id: target,
                source: stackSource,
                compose_yaml: stackSource === "inline" ? stackYaml : undefined,
                git: stackSource === "git" ? stackGit : undefined,
                main_service: mainService || undefined,
                project: stackProject.trim() || undefined,
              }),
            })
          : api(`/api/v1/environments/${environment.id}/services`, {
              method: "POST",
              body: JSON.stringify({
                name: serviceName,
                kind,
                server_id: target,
                spec: image ? { image } : {},
              }),
            }),
    onSuccess: async () => {
      toast.success(
        kind === "database"
          ? t("Database created with a generated password")
          : kind === "compose"
            ? t("Stack saved; deploy it from its page")
            : t("Service created"),
      );
      await queryClient.invalidateQueries({ queryKey: ["services"] });
      onClose();
    },
    onError: async (error) => {
      toast.error(error instanceof ApiError ? error.message : t("Could not create service"));
      await queryClient.invalidateQueries({ queryKey: ["services"] });
    },
  });

  return (
    <Dialog
      open
      variant="drawer"
      onClose={onClose}
      title={t("Add service")}
      description={t("It is created in {environment}. Deploy it from its own page.", {
        environment: environment.name,
      })}
    >
      <form
        className="grid gap-5"
        onSubmit={(event: FormEvent) => {
          event.preventDefault();
          create.mutate();
        }}
      >
        <PickField label={t("Kind")} options={SERVICE_KINDS} value={kind} onPick={setKind} />
        <div className="grid gap-5 sm:grid-cols-2">
          <Field label={t("Name")}>
            <TextInput
              required
              autoFocus
              value={serviceName}
              onChange={(event) => setServiceName(event.target.value)}
            />
          </Field>
          <Field label={t("Server")}>
            <Select value={serverId} onChange={(event) => setServerId(event.target.value)}>
              <option value="">{t("Select")}</option>
              {servers.map((server) => (
                <option key={server.id} value={server.id}>
                  {server.name}
                </option>
              ))}
            </Select>
          </Field>
        </div>
        {kind === "database" ? (
          <div className="grid gap-5">
            <PickField
              label={t("Engine")}
              options={(engines.data ?? []).map((item) => ({
                label: item.engine,
                value: item.engine,
              }))}
              value={engine}
              onPick={(next) => {
                setEngine(next);
                setVersion("");
              }}
            />
            <PickField
              label={t("Version")}
              options={engineVersions.map((item) => ({ label: item, value: item }))}
              value={version || engineVersions[0] || ""}
              onPick={setVersion}
            />
          </div>
        ) : kind === "compose" ? (
          <div className="grid gap-5">
            <Field
              label={t("Main service (receives domains)")}
              presets={
                <QuickPick
                  label={t("Main service presets")}
                  options={MAIN_SERVICES}
                  value={mainService}
                  onPick={setMainService}
                />
              }
            >
              <TextInput
                value={mainService}
                placeholder="web"
                onChange={(event) => setMainService(event.target.value)}
              />
            </Field>
            <Field
              label={t("Compose project name")}
              hint={t(
                "Optional. Use the name of a project already running on the server to take over its containers and volumes; otherwise one is generated.",
              )}
            >
              <TextInput
                value={stackProject}
                placeholder="my-app"
                pattern="[a-z0-9][a-z0-9\-]*"
                onChange={(event) => setStackProject(event.target.value)}
              />
            </Field>
            <CreateStackFields
              source={stackSource}
              yaml={stackYaml}
              git={stackGit}
              providers={providers.data ?? []}
              onSource={setStackSource}
              onYaml={setStackYaml}
              onGit={setStackGit}
            />
          </div>
        ) : (
          <Field
            label={t("Image")}
            hint={t("Optional. You can also build from Git later.")}
            presets={
              <QuickPick
                label={t("Image suggestions")}
                options={IMAGES}
                value={image}
                onPick={setImage}
              />
            }
          >
            <TextInput
              value={image}
              placeholder="ghcr.io/org/app:1"
              onChange={(event) => setImage(event.target.value)}
            />
          </Field>
        )}
        <p className="text-sm text-muted">
          {t("Or start from a ready-made app in {link}.", {
            link: (
              <Link className="text-accent" to="/templates">
                {t("Templates")}
              </Link>
            ),
          })}
        </p>
        <div className="flex justify-end gap-2 border-t border-line pt-4">
          <Button type="button" variant="secondary" onClick={onClose}>
            {t("Cancel")}
          </Button>
          <Button type="submit" disabled={create.isPending}>
            {t("Add service")}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}

function useDeletion(path: string, done: string, failed: string, onDeleted: () => void) {
  const t = useT();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => api(path, { method: "DELETE" }),
    onSuccess: async () => {
      toast.success(t(done));
      await queryClient.invalidateQueries();
      onDeleted();
    },
    onError: (error) => toast.error(error instanceof ApiError ? error.message : t(failed)),
  });
}

function DeleteProjectDialog({
  project,
  serviceCount,
  onClose,
  onDeleted,
}: {
  project: Project;
  serviceCount: number;
  onClose: () => void;
  onDeleted: () => void;
}) {
  const t = useT();
  const remove = useDeletion(
    `/api/v1/projects/${project.id}`,
    "Project deleted",
    "Could not delete project",
    onDeleted,
  );
  return (
    <DeleteDialog
      title={t("Delete project")}
      name={project.name}
      pending={remove.isPending}
      onClose={onClose}
      onConfirm={() => remove.mutate()}
    >
      {t(
        "This removes {project}, all its environments and its {count} services, and stops and removes their containers. This cannot be undone.",
        { project: project.name, count: serviceCount },
      )}
    </DeleteDialog>
  );
}

function DeleteEnvironmentDialog({
  project,
  environment,
  serviceCount,
  onClose,
  onDeleted,
}: {
  project: Project;
  environment: Environment;
  serviceCount: number;
  onClose: () => void;
  onDeleted: () => void;
}) {
  const t = useT();
  const remove = useDeletion(
    `/api/v1/environments/${environment.id}`,
    "Environment deleted",
    "Could not delete environment",
    onDeleted,
  );
  return (
    <DeleteDialog
      title={t("Delete environment")}
      name={environment.name}
      pending={remove.isPending}
      onClose={onClose}
      onConfirm={() => remove.mutate()}
    >
      {t(
        "This removes {environment} from {project} and its {count} services, and stops and removes their containers. This cannot be undone.",
        { environment: environment.name, project: project.name, count: serviceCount },
      )}
    </DeleteDialog>
  );
}
