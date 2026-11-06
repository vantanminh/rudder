import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { toast } from "sonner";
import {
  Bar,
  BarChart,
  CartesianGrid,
  Legend,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import { api } from "../api";
import { useAuth } from "../auth";
import { AXIS_LINE, COLORS, TICK, TOOLTIP_STYLES } from "../components/metrics";
import { Icon } from "../components/icons";
import {
  Alert,
  Badge,
  Button,
  Card,
  CodeBlock,
  DataTable,
  Empty,
  Field,
  PageHeader,
  Select,
  StatCard,
  Tabs,
  Td,
  TextInput,
  Th,
} from "../components/ui";
import { errorMessage, formatBytes, formatDate, formatRelative, formatTime } from "../format";
import { msg, useT } from "../i18n";
import type { Schemas } from "../types";

type Overview = Schemas["ImageStoreOverview"];
type SetupStep = Schemas["ImageSetupStep"];
type Destination = Schemas["DestinationView"];
type Settings = Schemas["ImageStoreSettingsView"];
type RudderImage = Schemas["RudderImageView"];
type TestResult = Schemas["TestResult"];

const TABS = [
  ["overview", msg("Overview")],
  ["images", msg("Images")],
  ["setup", msg("Setup")],
] as const;
type Tab = (typeof TABS)[number][0];

/** What each setup step is called. The server sends the key and the detail text. */
const STEP_TITLES: Record<string, string> = {
  bucket: msg("R2 / S3 bucket"),
  retention: msg("Retention"),
  client: msg("CLI client"),
  first_push: msg("First image pushed"),
  auto_deploy: msg("Deploy on push"),
};

export function useImageOverview() {
  return useQuery({
    queryKey: ["image-store-overview"],
    queryFn: () => api<Overview>("/api/v1/image-store/overview"),
    refetchInterval: 30_000,
  });
}

/** Live check of the bucket. Needs the write scope, so a viewer sees "unknown" instead. */
export function useBucketCheck(enabled: boolean) {
  return useQuery({
    queryKey: ["image-store-test"],
    queryFn: () => api<TestResult>("/api/v1/image-store/test", { method: "POST" }),
    enabled,
    staleTime: 60_000,
    retry: false,
  });
}

export function RudderImagePage() {
  const t = useT();
  const [tab, setTab] = useState<Tab>("overview");
  const overview = useImageOverview();
  return (
    <>
      <PageHeader
        title={t("Rudder Image")}
        description={t(
          "Docker images built on your machine, stored on Cloudflare R2 or any S3 bucket, and deployed to your servers. Track the setup, the pushes, and the clients that use it.",
        )}
        actions={
          <Link
            to="/s3-destinations"
            className="text-sm text-muted underline-offset-2 hover:text-ink hover:underline"
          >
            {t("Manage S3 destinations")}
          </Link>
        }
      />
      <Tabs tabs={TABS} active={tab} onChange={setTab} />
      {overview.isError ? (
        <Alert tone="error">{errorMessage(overview.error, t("Could not load Rudder Image"))}</Alert>
      ) : null}
      {tab === "overview" ? <OverviewTab overview={overview.data} loading={overview.isLoading} /> : null}
      {tab === "images" ? <ImagesTab overview={overview.data} /> : null}
      {tab === "setup" ? <SetupTab overview={overview.data} /> : null}
    </>
  );
}

function stepBadge(step: SetupStep, t: ReturnType<typeof useT>) {
  if (step.state === "ok") return <Badge value={t("ready")} tone="success" />;
  if (step.state === "optional") return <Badge value={t("optional")} tone="outline" />;
  return <Badge value={t("to do")} tone="warning" />;
}

function stepDetail(step: SetupStep, t: ReturnType<typeof useT>): string {
  // The server's detail carries names and counts; translate the fixed sentences only.
  const fixed: Record<string, string> = {
    "Choose the Cloudflare R2 / S3 bucket that stores the images.": msg(
      "Choose the Cloudflare R2 / S3 bucket that stores the images.",
    ),
    "Old versions are pruned from the bucket.": msg("Old versions are pruned from the bucket."),
    "Every version stays in the bucket until you delete it.": msg(
      "Every version stays in the bucket until you delete it.",
    ),
    "No command-line client has signed in yet (rudder login).": msg(
      "No command-line client has signed in yet (rudder login).",
    ),
    "Nothing pushed yet (rudder build, then rudder push).": msg(
      "Nothing pushed yet (rudder build, then rudder push).",
    ),
    "Pushes only notify; deploy by hand or turn auto-deploy on per service.": msg(
      "Pushes only notify; deploy by hand or turn auto-deploy on per service.",
    ),
  };
  return fixed[step.detail] ? t(fixed[step.detail]!) : step.detail;
}

function OverviewTab({ overview, loading }: { overview: Overview | undefined; loading: boolean }) {
  const t = useT();
  const hasBucket = Boolean(overview?.destination);
  const check = useBucketCheck(hasBucket);
  const queryClient = useQueryClient();
  const totals = overview?.totals;
  const builds = overview?.builds;
  const finishedBuilds = (builds?.succeeded_30d ?? 0) + (builds?.failed_30d ?? 0);
  const bucketTone = !hasBucket
    ? "warning"
    : check.isFetching
      ? "neutral"
      : check.data?.ok
        ? "success"
        : check.data
          ? "danger"
          : "neutral";

  const days = (overview?.daily ?? []).map((d) => ({
    ...d,
    label: new Date(`${d.day}T00:00:00`).toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
    }),
  }));
  const anyActivity = days.some((d) => d.pushes || d.builds || d.image_deploys);

  return (
    <div className="space-y-6">
      <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
        <StatCard
          icon="bucket"
          label={t("Versions on R2")}
          value={totals?.r2_images ?? 0}
          caption={t("{pruned} pruned by retention", { pruned: totals?.pruned_images ?? 0 })}
          loading={loading}
        />
        <StatCard
          icon="archive"
          label={t("Bucket usage")}
          value={formatBytes(totals?.r2_bytes ?? 0)}
          caption={t("{count} services with images", { count: totals?.services ?? 0 })}
          loading={loading}
        />
        <StatCard
          icon="rocket"
          label={t("Pushes, 7 days")}
          value={totals?.pushes_7d ?? 0}
          caption={t("{day} in 24 h · {month} in 30 days", {
            day: totals?.pushes_24h ?? 0,
            month: totals?.pushes_30d ?? 0,
          })}
          loading={loading}
        />
        <StatCard
          icon="box"
          label={t("Builds, 30 days")}
          value={builds?.builds_30d ?? 0}
          tone={(builds?.failed_30d ?? 0) > 0 ? "warning" : "neutral"}
          caption={
            finishedBuilds
              ? t("{ok} succeeded · {failed} failed", {
                  ok: builds?.succeeded_30d ?? 0,
                  failed: builds?.failed_30d ?? 0,
                })
              : t("{count} image deploys", { count: builds?.image_deploys_30d ?? 0 })
          }
          loading={loading}
        />
      </div>

      <div className="grid gap-6 lg:grid-cols-5">
        <Card
          className="lg:col-span-2"
          title={t("Status")}
          description={t("What is ready and what is left to set up.")}
          actions={
            hasBucket ? (
              <Button
                quiet
                size="sm"
                icon="activity"
                disabled={check.isFetching}
                onClick={() => {
                  void queryClient.invalidateQueries({ queryKey: ["image-store-test"] });
                }}
              >
                {t("Check bucket")}
              </Button>
            ) : null
          }
        >
          <ul className="space-y-3">
            <li className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <p className="text-sm font-medium text-ink">{t("Bucket connection")}</p>
                <p className="mt-0.5 text-[13px] break-words text-muted">
                  {!hasBucket
                    ? t("No bucket is selected yet.")
                    : check.isFetching
                      ? t("Writing, reading, and deleting a probe object…")
                      : check.data
                        ? check.data.ok
                          ? t("Write, read, and delete succeeded.")
                          : check.data.message
                        : check.isError
                          ? t("Could not run the check (it needs the write permission).")
                          : ""}
                </p>
              </div>
              <Badge
                value={
                  !hasBucket
                    ? t("not set")
                    : check.isFetching
                      ? t("checking")
                      : check.data?.ok
                        ? t("connected")
                        : check.data
                          ? t("failing")
                          : t("unknown")
                }
                tone={bucketTone}
              />
            </li>
            {(overview?.setup ?? []).map((step) => (
              <li key={step.key} className="flex items-start justify-between gap-3">
                <div className="min-w-0">
                  <p className="text-sm font-medium text-ink">
                    {t(STEP_TITLES[step.key] ?? step.key)}
                  </p>
                  <p className="mt-0.5 text-[13px] break-words text-muted">
                    {stepDetail(step, t)}
                  </p>
                </div>
                {stepBadge(step, t)}
              </li>
            ))}
          </ul>
        </Card>

        <Card
          className="lg:col-span-3"
          title={t("Activity, last 30 days")}
          description={t("Image pushes, builds on the panel, and deploys from an image.")}
        >
          {anyActivity ? (
            <ResponsiveContainer width="100%" height={232}>
              <BarChart data={days} margin={{ top: 4, right: 8, bottom: 0, left: -16 }}>
                <CartesianGrid strokeDasharray="3 3" stroke="var(--line)" vertical={false} />
                <XAxis
                  dataKey="label"
                  tick={TICK}
                  axisLine={AXIS_LINE}
                  tickLine={false}
                  minTickGap={24}
                />
                <YAxis tick={TICK} axisLine={false} tickLine={false} allowDecimals={false} />
                <Tooltip {...TOOLTIP_STYLES} />
                <Legend wrapperStyle={{ fontSize: 12, color: "var(--ink-soft)" }} />
                <Bar dataKey="pushes" name={t("Pushes")} fill={COLORS[0]} isAnimationActive={false} />
                <Bar dataKey="builds" name={t("Builds")} fill={COLORS[2]} isAnimationActive={false} />
                <Bar
                  dataKey="image_deploys"
                  name={t("Image deploys")}
                  fill={COLORS[1]}
                  isAnimationActive={false}
                />
              </BarChart>
            </ResponsiveContainer>
          ) : (
            <Empty icon="rocket" title={t("No activity yet")}>
              {t("Pushes, builds, and deploys appear here as soon as they happen.")}
            </Empty>
          )}
        </Card>
      </div>

      <Card
        flush
        title={t("Services")}
        description={t("Applications with image versions or deploy-on-push turned on.")}
      >
        {overview && overview.services.length === 0 ? (
          <Empty>{t("No service has pushed an image yet.")}</Empty>
        ) : (
          <DataTable
            head={
              <>
                <Th>{t("Service")}</Th>
                <Th>{t("Versions")}</Th>
                <Th>{t("On R2")}</Th>
                <Th>{t("Latest tag")}</Th>
                <Th>{t("Deploy on push")}</Th>
                <Th>{t("Last push")}</Th>
              </>
            }
          >
            {(overview?.services ?? []).map((s) => (
              <tr key={s.service_id}>
                <Td>
                  <Link
                    to={`/services/${s.service_id}`}
                    className="font-medium text-ink hover:underline"
                  >
                    {s.path}
                  </Link>
                </Td>
                <Td className="tabular-nums">{s.images}</Td>
                <Td className="tabular-nums">
                  {s.r2_images} · {formatBytes(s.r2_bytes)}
                </Td>
                <Td className="font-mono text-[13px]">{s.last_tag ?? "—"}</Td>
                <Td>
                  <Badge
                    value={s.auto_deploy ? t("deploys") : t("notifies")}
                    tone={s.auto_deploy ? "success" : "outline"}
                  />
                </Td>
                <Td className="text-muted">{formatRelative(s.last_push_at)}</Td>
              </tr>
            ))}
          </DataTable>
        )}
      </Card>

      <div className="grid gap-6 xl:grid-cols-2">
        <Card flush title={t("Recent pushes")}>
          {overview && overview.recent_pushes.length === 0 ? (
            <Empty>{t("Nothing pushed yet.")}</Empty>
          ) : (
            <DataTable
              minWidth={480}
              head={
                <>
                  <Th>{t("Image")}</Th>
                  <Th>{t("Size")}</Th>
                  <Th>{t("By")}</Th>
                  <Th>{t("When")}</Th>
                </>
              }
            >
              {(overview?.recent_pushes ?? []).map((p) => (
                <tr key={p.id}>
                  <Td>
                    <p className="font-medium text-ink">{p.path}</p>
                    <p className="font-mono text-[13px] text-muted">
                      {p.tag}
                      {p.git_sha ? ` · ${p.git_sha.slice(0, 7)}` : ""}
                      {p.r2_present ? "" : ` · ${t("pruned")}`}
                    </p>
                  </Td>
                  <Td className="tabular-nums">{formatBytes(p.size_bytes)}</Td>
                  <Td className="text-muted">{p.pushed_by ?? "—"}</Td>
                  <Td className="text-muted" title={formatTime(p.created_at)}>
                    {formatRelative(p.created_at)}
                  </Td>
                </tr>
              ))}
            </DataTable>
          )}
        </Card>

        <Card
          flush
          title={t("Signed-in clients")}
          description={t("Command-line sessions created with `rudder login --browser` or `--device`.")}
          actions={
            <Link to="/tokens" className="text-sm text-muted hover:text-ink hover:underline">
              {t("Manage tokens")}
            </Link>
          }
        >
          {overview && overview.clients.length === 0 ? (
            <Empty icon="terminal" title={t("No client yet")}>
              {t("Run `rudder login --url <panel URL> --browser` on your machine.")}
            </Empty>
          ) : (
            <DataTable
              minWidth={480}
              head={
                <>
                  <Th>{t("Client")}</Th>
                  <Th>{t("Signed in with")}</Th>
                  <Th>{t("Last used")}</Th>
                </>
              }
            >
              {(overview?.clients ?? []).map((c) => (
                <tr key={c.id}>
                  <Td>
                    <p className="font-medium text-ink">{c.name}</p>
                    <p className="text-[13px] text-muted">{c.user_email}</p>
                  </Td>
                  <Td>
                    <Badge
                      value={c.auth_method === "browser" ? t("browser") : t("device code")}
                      tone="outline"
                    />
                  </Td>
                  <Td className="text-muted" title={formatTime(c.last_used_at)}>
                    {c.last_used_at ? formatRelative(c.last_used_at) : t("never")}
                  </Td>
                </tr>
              ))}
            </DataTable>
          )}
        </Card>
      </div>
    </div>
  );
}

function ImagesTab({ overview }: { overview: Overview | undefined }) {
  const t = useT();
  const { user } = useAuth();
  const canWrite = user?.role === "admin";
  const queryClient = useQueryClient();
  const [serviceId, setServiceId] = useState("");
  const services = overview?.services ?? [];
  useEffect(() => {
    if (!serviceId && services[0]) setServiceId(services[0].service_id);
  }, [serviceId, services]);

  const images = useQuery({
    queryKey: ["service-images", serviceId],
    queryFn: () => api<RudderImage[]>(`/api/v1/services/${serviceId}/images`),
    enabled: Boolean(serviceId),
  });
  const refresh = async () => {
    await queryClient.invalidateQueries({ queryKey: ["service-images", serviceId] });
    await queryClient.invalidateQueries({ queryKey: ["image-store-overview"] });
  };
  const deploy = useMutation({
    mutationFn: (id: string) =>
      api(`/api/v1/services/${serviceId}/images/${id}/deploy`, { method: "POST" }),
    onSuccess: async () => {
      toast.success(t("Image deployed"));
      await refresh();
    },
    onError: (error) => toast.error(errorMessage(error, t("Could not deploy this image"))),
  });
  const remove = useMutation({
    mutationFn: ({ id, r2Only }: { id: string; r2Only: boolean }) =>
      api(`/api/v1/services/${serviceId}/images/${id}${r2Only ? "?r2_only=true" : ""}`, {
        method: "DELETE",
      }),
    onSuccess: async () => {
      toast.success(t("Image removed"));
      await refresh();
    },
    onError: (error) => toast.error(errorMessage(error, t("Could not remove this image"))),
  });

  if (overview && services.length === 0) {
    return (
      <Card>
        <Empty icon="box" title={t("No images yet")}>
          {t("Build and push an image with the rudder CLI; it will be listed here.")}
        </Empty>
      </Card>
    );
  }
  return (
    <Card
      flush
      title={t("Image versions")}
      description={t("Every version pushed for a service, and whether it is still on the bucket.")}
      actions={
        <div className="w-64 max-w-full">
          <Select
            aria-label={t("Service")}
            value={serviceId}
            onChange={(event) => setServiceId(event.target.value)}
          >
            {services.map((s) => (
              <option key={s.service_id} value={s.service_id}>
                {s.path}
              </option>
            ))}
          </Select>
        </div>
      }
    >
      <DataTable
        head={
          <>
            <Th>{t("Tag")}</Th>
            <Th>{t("Size")}</Th>
            <Th>{t("Storage")}</Th>
            <Th>{t("Pushed")}</Th>
            <Th />
          </>
        }
      >
        {(images.data ?? []).map((image) => (
          <tr key={image.id}>
            <Td>
              <p className="font-mono text-[13px] font-medium text-ink">{image.tag}</p>
              <p className="font-mono text-xs text-muted">
                {image.git_sha ? image.git_sha.slice(0, 7) : image.sha256.slice(0, 12)}
              </p>
            </Td>
            <Td className="tabular-nums">{formatBytes(image.size_bytes)}</Td>
            <Td>
              <Badge
                value={image.r2_present ? t("on R2") : t("pruned")}
                tone={image.r2_present ? "success" : "outline"}
              />
            </Td>
            <Td className="text-muted" title={formatTime(image.created_at)}>
              {formatDate(image.created_at)}
            </Td>
            <Td className="text-right whitespace-nowrap">
              {canWrite ? (
                <span className="inline-flex gap-2">
                  <Button
                    quiet
                    size="sm"
                    disabled={!image.r2_present || deploy.isPending}
                    onClick={() => deploy.mutate(image.id)}
                  >
                    {t("Deploy")}
                  </Button>
                  {image.r2_present ? (
                    <Button
                      quiet
                      size="sm"
                      disabled={remove.isPending}
                      onClick={() => {
                        if (window.confirm(t("Delete {tag} from the bucket?", { tag: image.tag }))) {
                          remove.mutate({ id: image.id, r2Only: true });
                        }
                      }}
                    >
                      {t("Free bucket space")}
                    </Button>
                  ) : null}
                  <Button
                    variant="danger"
                    size="sm"
                    icon="trash"
                    aria-label={t("Delete")}
                    disabled={remove.isPending}
                    onClick={() => {
                      if (window.confirm(t("Forget {tag} completely?", { tag: image.tag }))) {
                        remove.mutate({ id: image.id, r2Only: false });
                      }
                    }}
                  />
                </span>
              ) : null}
            </Td>
          </tr>
        ))}
      </DataTable>
      {images.data?.length === 0 ? <Empty>{t("No versions for this service.")}</Empty> : null}
    </Card>
  );
}

function SetupTab({ overview }: { overview: Overview | undefined }) {
  const t = useT();
  const { user } = useAuth();
  const canWrite = user?.role === "admin";
  const queryClient = useQueryClient();
  const destinations = useQuery({
    queryKey: ["s3-destinations"],
    queryFn: () => api<Destination[]>("/api/v1/s3-destinations"),
  });
  const [destinationId, setDestinationId] = useState("");
  const [prefix, setPrefix] = useState("rudder-images");
  const [keepCount, setKeepCount] = useState("");
  const [keepDays, setKeepDays] = useState("");

  const settings = overview?.settings;
  useEffect(() => {
    if (!settings) return;
    setDestinationId(settings.destination_id ?? "");
    setPrefix(settings.path_prefix || "rudder-images");
    setKeepCount(settings.r2_keep_count != null ? String(settings.r2_keep_count) : "");
    setKeepDays(settings.r2_keep_days != null ? String(settings.r2_keep_days) : "");
  }, [settings]);

  const save = useMutation({
    mutationFn: () =>
      api<Settings>("/api/v1/image-store/settings", {
        method: "PUT",
        body: JSON.stringify({
          destination_id: destinationId || null,
          path_prefix: prefix || "rudder-images",
          r2_keep_count: keepCount.trim() ? Number(keepCount) : null,
          r2_keep_days: keepDays.trim() ? Number(keepDays) : null,
        }),
      }),
    onSuccess: async () => {
      toast.success(t("Rudder Image store settings saved"));
      await queryClient.invalidateQueries({ queryKey: ["image-store-overview"] });
      await queryClient.invalidateQueries({ queryKey: ["image-store-test"] });
    },
    onError: (error) => toast.error(errorMessage(error, t("Could not save image store settings"))),
  });

  const origin = typeof window === "undefined" ? "https://panel.example.com" : window.location.origin;
  const quickstart = [
    `rudder login --url ${origin} --browser`,
    "rudder init my-project/production/api",
    "rudder build",
    "rudder push --deploy",
  ].join("\n");

  return (
    <div className="space-y-6">
      <Card
        title={t("Image store")}
        description={t(
          "Pick the Cloudflare R2 or S3 bucket that holds pushed images. Older versions above the retention limit are pruned from the bucket; they stay on the developer's machine.",
        )}
      >
        <form
          className="grid gap-4 md:grid-cols-2"
          onSubmit={(event) => {
            event.preventDefault();
            save.mutate();
          }}
        >
          <Field
            label={t("Bucket")}
            hint={
              (destinations.data ?? []).length === 0
                ? t("Add a destination first under S3 destinations.")
                : undefined
            }
          >
            <Select
              disabled={!canWrite}
              value={destinationId}
              onChange={(event) => setDestinationId(event.target.value)}
            >
              <option value="">{t("Disabled")}</option>
              {(destinations.data ?? []).map((dest) => (
                <option key={dest.id} value={dest.id}>
                  {dest.name} ({dest.bucket})
                </option>
              ))}
            </Select>
          </Field>
          <Field label={t("Object key prefix")}>
            <TextInput
              required
              disabled={!canWrite}
              value={prefix}
              onChange={(event) => setPrefix(event.target.value)}
            />
          </Field>
          <Field
            label={t("Keep on R2 (versions per service)")}
            hint={t("Leave empty to keep all versions on R2.")}
          >
            <TextInput
              type="number"
              min={1}
              placeholder="10"
              disabled={!canWrite}
              value={keepCount}
              onChange={(event) => setKeepCount(event.target.value)}
            />
          </Field>
          <Field
            label={t("Keep on R2 (days)")}
            hint={t("Versions older than this are pruned. Leave empty to keep them.")}
          >
            <TextInput
              type="number"
              min={1}
              placeholder="30"
              disabled={!canWrite}
              value={keepDays}
              onChange={(event) => setKeepDays(event.target.value)}
            />
          </Field>
          <div className="flex items-center gap-3 md:col-span-2">
            <Button type="submit" disabled={!canWrite || save.isPending}>
              {t("Save image store")}
            </Button>
            <Link to="/s3-destinations" className="text-sm text-muted hover:text-ink hover:underline">
              {t("Add or test a bucket")}
            </Link>
          </div>
        </form>
      </Card>

      <Card
        title={t("Connect the rudder CLI")}
        description={t(
          "Install the CLI from the release page, sign in once, then build and push from your project.",
        )}
        actions={
          <a
            href="https://github.com/vantanminh/rudder/releases/latest"
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1 text-sm text-muted hover:text-ink hover:underline"
          >
            {t("Releases")} <Icon name="external" size={13} />
          </a>
        }
      >
        <CodeBlock>{quickstart}</CodeBlock>
        <p className="mt-3 text-[13px] text-muted">
          {t(
            "Choose deploy-on-push or notify-only, and the per-service retention, on each service's page.",
          )}
        </p>
      </Card>
    </div>
  );
}
