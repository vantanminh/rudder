import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { api } from "../api";
import { Badge, Card, Skeleton } from "./ui";
import { formatBytes, formatRelative } from "../format";
import { useT } from "../i18n";
import type { Schemas } from "../types";

type Overview = Schemas["ImageStoreOverview"];

/** Dashboard card: is Rudder Image set up, and how much is it used. */
export function ImageStatusCard() {
  const t = useT();
  const overview = useQuery({
    queryKey: ["image-store-overview"],
    queryFn: () => api<Overview>("/api/v1/image-store/overview"),
    refetchInterval: 60_000,
  });
  const data = overview.data;
  const pending = (data?.setup ?? []).filter((step) => step.state === "todo").length;
  const ready = data ? pending === 0 : false;
  const lastPush = data?.recent_pushes[0];

  return (
    <Card
      title={t("Rudder Image")}
      actions={
        <Link to="/rudder-image" className="text-sm text-muted hover:text-ink hover:underline">
          {t("Open")}
        </Link>
      }
    >
      {overview.isLoading ? (
        <Skeleton className="h-16 w-full" />
      ) : !data ? (
        <p className="text-sm text-muted">{t("Could not load Rudder Image")}</p>
      ) : (
        <dl className="space-y-2.5 text-sm">
          <div className="flex items-center justify-between gap-3">
            <dt className="text-muted">{t("Bucket")}</dt>
            <dd className="min-w-0 truncate">
              {data.destination ? (
                <span title={data.destination.bucket}>{data.destination.name}</span>
              ) : (
                <Badge value={t("not set")} tone="warning" />
              )}
            </dd>
          </div>
          <div className="flex items-center justify-between gap-3">
            <dt className="text-muted">{t("CLI client")}</dt>
            <dd>
              <Badge
                value={
                  data.clients.length > 0
                    ? t("{count} signed in", { count: data.clients.length })
                    : t("none yet")
                }
                tone={data.clients.length > 0 ? "success" : "outline"}
              />
            </dd>
          </div>
          <div className="flex items-center justify-between gap-3">
            <dt className="text-muted">{t("Versions on R2")}</dt>
            <dd className="tabular-nums">
              {data.totals.r2_images} · {formatBytes(data.totals.r2_bytes)}
            </dd>
          </div>
          <div className="flex items-center justify-between gap-3">
            <dt className="text-muted">{t("Pushes, 7 days")}</dt>
            <dd className="tabular-nums">{data.totals.pushes_7d}</dd>
          </div>
          <div className="flex items-center justify-between gap-3">
            <dt className="text-muted">{t("Last push")}</dt>
            <dd className="text-muted">{lastPush ? formatRelative(lastPush.created_at) : "—"}</dd>
          </div>
          <div className="flex items-center justify-between gap-3 border-t border-line pt-2.5">
            <dt className="text-muted">{t("Setup")}</dt>
            <dd>
              <Badge
                value={ready ? t("ready") : t("{count} steps left", { count: pending })}
                tone={ready ? "success" : "warning"}
              />
            </dd>
          </div>
        </dl>
      )}
    </Card>
  );
}
