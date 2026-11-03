import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { toast } from "sonner";
import { api, ApiError } from "../api";
import { Button, Card, Field, Tabs, TextInput } from "../components/ui";
import { useT } from "../i18n";

import { CloudflarePanel } from "./CloudflarePanel";
import type { Schemas } from "../types";

type Tab = "overview" | "ssl" | "checks" | "limits" | "cloudflare";

export function SecurityPage() {
  const t = useT();
  const [tab, setTab] = useState<Tab>("overview");
  return (
    <>
      <h1 className="text-2xl font-semibold">{t("Security")}</h1>
      <p className="text-sm text-muted">
        {t("Certificates, exposed files, plain HTTP, raw IP access, and shared rate limits.")}
      </p>
      <Tabs
        tabs={[
          ["overview", "Overview"],
          ["ssl", "SSL"],
          ["checks", "Checks"],
          ["limits", "Rate limits"],

          ["cloudflare", "Cloudflare"],
        ]}
        active={tab}
        onChange={setTab}
      />
      {tab === "overview" ? <Overview /> : null}
      {tab === "ssl" ? <Ssl /> : null}
      {tab === "checks" ? <Checks /> : null}
      {tab === "limits" ? <Limits /> : null}

      {tab === "cloudflare" ? <CloudflarePanel /> : null}
    </>
  );
}

function Overview() {
  const t = useT();
  const overview = useQuery({
    queryKey: ["security-overview"],
    queryFn: () => api<Schemas["SecurityOverview"]>("/api/v1/security/overview"),
  });
  const edge = useQuery({
    queryKey: ["security-edge"],
    queryFn: () => api<Schemas["EdgeStats"]>("/api/v1/security/edge"),
  });
  const data = overview.data;
  return (
    <div className="grid gap-4">
      <Card title={t("Right now")}>
        {data ? (
          <ul className="grid gap-2 text-sm md:grid-cols-3">
            <li>{t("{count} certificates", { count: data.certificates })}</li>
            <li>{t("{count} expiring within 14 days", { count: data.expiring })}</li>
            <li>{t("{count} expired", { count: data.expired })}</li>
            <li>{t("{count} open findings", { count: data.open_findings })}</li>
            <li>{t("{count} plain HTTP requests", { count: edge.data?.http_requests ?? data.http_requests })}</li>
            <li>{t("{count} requests by IP address", { count: edge.data?.ip_requests ?? data.ip_requests })}</li>
          </ul>
        ) : (
          <p className="text-sm text-muted">{t("Loading…")}</p>
        )}
      </Card>
      <Card title={t("Recent blocks")} description={t("These are the requests a rate limit stopped in the last day. A spike here may mean a real visitor is being blocked.")}>
        <ul className="space-y-1 text-sm">
          {(edge.data?.recent_blocks ?? []).length === 0 ? (
            <li className="text-muted">{t("No blocked requests yet.")}</li>
          ) : (
            edge.data?.recent_blocks.map((block) => (
              <li key={`${block.created_at}-${block.ip}-${block.path}`}>
                {block.host}
                {block.path} · {block.ip}
              </li>
            ))
          )}
        </ul>
      </Card>
    </div>
  );
}

function Ssl() {
  const t = useT();
  const summary = useQuery({
    queryKey: ["security-ssl"],
    queryFn: () => api<Schemas["SslSummary"]>("/api/v1/security/ssl"),
  });
  const events = useQuery({
    queryKey: ["security-ssl-events"],
    queryFn: () => api<Schemas["SslEventView"][]>("/api/v1/security/ssl/events"),
  });
  return (
    <div className="grid gap-4">
      <Card title={t("How domains are served")}>
        {summary.data ? (
          <p className="text-sm">
            {t("Let's Encrypt {le} · uploaded {custom} · none {none}", {
              le: summary.data.letsencrypt,
              custom: summary.data.custom,
              none: summary.data.none,
            })}
          </p>
        ) : null}
        <ul className="mt-3 space-y-1 text-sm">
          {(summary.data?.certificates ?? []).map((cert) => (
            <li key={cert.id}>
              {cert.name}
              {cert.days_left !== null && cert.days_left !== undefined
                ? t(" · {days} days left", { days: cert.days_left })
                : ""}
            </li>
          ))}
        </ul>
      </Card>
      <Card title={t("SSL events")}>
        <ul className="space-y-1 text-sm">
          {(events.data ?? []).map((event) => (
            <li key={event.id}>
              {event.kind}: {event.summary}
            </li>
          ))}
        </ul>
      </Card>
    </div>
  );
}

function Checks() {
  const t = useT();
  const queryClient = useQueryClient();
  const findings = useQuery({
    queryKey: ["security-findings"],
    queryFn: () => api<Schemas["SecurityFindingView"][]>("/api/v1/security/findings"),
  });
  const scan = useMutation({
    mutationFn: () => api("/api/v1/security/scans", { method: "POST" }),
    onSuccess: async () => {
      toast.success(t("Security check started"));
      await queryClient.invalidateQueries({ queryKey: ["security-findings"] });
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : t("Could not start the check")),
  });
  return (
    <Card
      title={t("Exposed files")}
      description={t("Rudder requests a short list of well-known paths on your domains and reports the ones that answer. The response body is not saved.")}
      actions={
        <Button disabled={scan.isPending} onClick={() => scan.mutate()}>
          {t("Run check")}
        </Button>
      }
    >
      <ul className="space-y-2 text-sm">
        {(findings.data ?? []).length === 0 ? (
          <li className="text-muted">{t("No findings from the latest check.")}</li>
        ) : (
          findings.data?.map((finding) => (
            <li key={finding.id}>
              <span className="font-medium">{finding.severity}</span> {finding.host}
              {finding.path ?? ""} — {finding.summary}
            </li>
          ))
        )}
      </ul>
    </Card>
  );
}

function Limits() {
  const t = useT();
  const queryClient = useQueryClient();
  const rules = useQuery({
    queryKey: ["rate-limits"],
    queryFn: () => api<Schemas["RateLimitView"][]>("/api/v1/security/rate-limits"),
  });
  const stats = useQuery({
    queryKey: ["rate-limit-stats"],
    queryFn: () => api<Schemas["RateLimitStat"][]>("/api/v1/security/rate-limits/stats"),
  });
  const [name, setName] = useState("");
  const [host, setHost] = useState("");
  const [limit, setLimit] = useState("60");
  const [windowSecs, setWindowSecs] = useState("60");
  const create = useMutation({
    mutationFn: () =>
      api("/api/v1/security/rate-limits", {
        method: "POST",
        body: JSON.stringify({
          name,
          host: host || undefined,
          limit_count: Number(limit),
          window_secs: Number(windowSecs),
          key_kind: "ip",
        }),
      }),
    onSuccess: async () => {
      setName("");
      toast.success(t("Rate limit saved"));
      await queryClient.invalidateQueries({ queryKey: ["rate-limits"] });
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : t("Could not save the rate limit")),
  });
  return (
    <Card
      title={t("Shared rate limits")}
      description={t("A rule with no hostname applies to every app domain. The panel and the Access portal are not limited, so you cannot lock yourself out.")}
    >
      <ul className="mb-4 space-y-1 text-sm">
        {(rules.data ?? []).map((rule) => {
          const stat = stats.data?.find((item) => item.rule_id === rule.id);
          return (
            <li key={rule.id}>
              {rule.name} · {rule.limit_count}/{rule.window_secs}s · {rule.host || t("all app domains")}
              {stat ? t(" · allowed {allowed} · blocked {blocked}", { allowed: stat.allowed, blocked: stat.blocked }) : ""}
            </li>
          );
        })}
      </ul>
      <form
        className="grid gap-3 md:grid-cols-4"
        onSubmit={(event: FormEvent) => {
          event.preventDefault();
          create.mutate();
        }}
      >
        <Field label={t("Name")}>
          <TextInput required value={name} onChange={(event) => setName(event.target.value)} />
        </Field>
        <Field label={t("Hostname")} hint={t("Blank applies to every app.")}>
          <TextInput value={host} onChange={(event) => setHost(event.target.value)} />
        </Field>
        <Field label={t("Requests")}>
          <TextInput value={limit} onChange={(event) => setLimit(event.target.value)} />
        </Field>
        <Field label={t("Window (seconds)")}>
          <TextInput value={windowSecs} onChange={(event) => setWindowSecs(event.target.value)} />
        </Field>
        <Button type="submit">{t("Add")}</Button>
      </form>
    </Card>
  );
}
