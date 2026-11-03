import { useMutation, useQuery } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { toast } from "sonner";
import { api, ApiError } from "../api";
import { Button, Card, Field, TextInput } from "../components/ui";
import { useT } from "../i18n";

interface Scope {
  id: string;
  group: string;
  write: boolean;
  summary: string;
}

interface Template {
  id: string;
  name: string;
  summary: string;
  scopes: string[];
}

interface Catalog {
  scopes: Scope[];
  templates: Template[];
}

interface ClientView {
  client_id: string;
  client_name: string;
  redirect_uris: string[];
}

function templateCopy(t: (key: string) => string, id: string): { name: string; summary: string } {
  switch (id) {
    case "full":
      return {
        name: t("Full control"),
        summary: t("Read and change every setting and all data."),
      };
    case "read-all":
      return {
        name: t("Read everything"),
        summary: t("Read every setting and all data. Cannot change anything."),
      };
    case "operator":
      return {
        name: t("Operate"),
        summary: t(
          "Deploy and manage apps, Access, security, caches, logs, and monitors. Cannot change users, tokens, or panel updates.",
        ),
      };
    case "observer":
      return {
        name: t("Watch"),
        summary: t("Read health, logs, metrics, cache stats, and security checks."),
      };
    default:
      return { name: t("Custom"), summary: t("Choose each area this agent may read or change.") };
  }
}

function areaLabel(t: (key: string) => string, group: string): string {
  switch (group) {
    case "auth":
      return t("Users, sessions, API tokens, and the audit log");
    case "catalog":
      return t("Servers, projects, environments, and services");
    case "agent":
      return t("Agent install tokens");
    case "edge":
      return t("Domains, certificates, registries, and deploys");
    case "source":
      return t("Git providers and builds");
    case "databases":
      return t("Managed databases");
    case "backups":
      return t("Backups and S3 destinations");
    case "stacks":
      return t("Compose stacks and app templates");
    case "cache":
      return t("HTTP cache policies, purge, and stats");
    case "logs":
      return t("Logs and retention");
    case "monitoring":
      return t("Metrics, alerts, and notifications");
    case "access":
      return t("Rudder Access people, departments, and app sign-in");
    case "security":
      return t("Security checks, SSL activity, and rate limits");
    case "cloudflare":
      return t("Cloudflare tunnel, Access, and SSH for the optional Zero Trust mode");
    default:
      return t("Panel settings, load, and updates");
  }
}

export function AgentConsentPage() {
  const t = useT();
  const [params] = useSearchParams();
  const clientId = params.get("client_id") ?? "";
  const redirectUri = params.get("redirect_uri") ?? "";
  const challenge = params.get("code_challenge") ?? "";
  const method = params.get("code_challenge_method") ?? "";
  const state = params.get("state") ?? "";
  const resource = params.get("resource");
  const catalog = useQuery({
    queryKey: ["mcp-catalog"],
    queryFn: () => api<Catalog>("/api/v1/mcp/catalog"),
  });
  const client = useQuery({
    queryKey: ["mcp-client", clientId],
    queryFn: () => api<ClientView>(`/api/v1/mcp/clients/${encodeURIComponent(clientId)}`),
    enabled: clientId.length > 0,
  });
  const [template, setTemplate] = useState("full");
  const [custom, setCustom] = useState<string[]>([]);
  const [name, setName] = useState("");

  const groups = useMemo(() => {
    const order: string[] = [];
    const byGroup = new Map<string, Scope[]>();
    for (const scope of catalog.data?.scopes ?? []) {
      if (!byGroup.has(scope.group)) {
        order.push(scope.group);
        byGroup.set(scope.group, []);
      }
      byGroup.get(scope.group)?.push(scope);
    }
    return order.map((group) => ({
      group,
      summary: byGroup.get(group)?.[0]?.summary ?? group,
      scopes: byGroup.get(group) ?? [],
    }));
  }, [catalog.data]);

  const approve = useMutation({
    mutationFn: () =>
      api<{ redirect: string }>("/api/v1/mcp/consent", {
        method: "POST",
        body: JSON.stringify({
          client_id: clientId,
          redirect_uri: redirectUri,
          code_challenge: challenge,
          code_challenge_method: method,
          state,
          resource,
          template: template === "custom" ? "custom" : template,
          scopes: template === "custom" ? custom : [],
          name,
        }),
      }),
    onSuccess: (body) => {
      window.location.assign(body.redirect);
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : t("Could not approve the agent")),
  });

  function toggle(id: string, write: boolean, checked: boolean) {
    setTemplate("custom");
    setCustom((current) => {
      const next = new Set(current);
      const read = id.replace(":write", ":read");
      if (checked) {
        next.add(id);
        if (write) next.add(read);
      } else {
        next.delete(id);
        if (!write) next.delete(id.replace(":read", ":write"));
      }
      return [...next];
    });
  }

  const missing = !clientId || !redirectUri || !challenge || !state;

  return (
    <>
      <div>
        <h1 className="text-2xl font-semibold">{t("Approve an agent")}</h1>
        <p className="text-sm text-muted">
          {client.data
            ? t("{name} wants to control this panel.", { name: client.data.client_name })
            : t("An agent wants to control this panel.")}
        </p>
      </div>
      {missing ? (
        <Card title={t("Something is missing")}>
          <p className="text-sm text-muted">
            {t("Open this page from the agent. The sign-in request was incomplete.")}
          </p>
        </Card>
      ) : (
        <Card title={t("Permissions")}>
          <p className="mb-4 break-all text-sm text-muted">
            {t("After you approve, the browser returns to {url}.", { url: redirectUri })}
          </p>
          <div className="grid gap-3">
            {(catalog.data?.templates ?? []).map((item) => (
              <label
                key={item.id}
                className={`cursor-pointer rounded-md border p-3 ${template === item.id ? "border-pine bg-pine-soft" : "border-line"}`}
              >
                <span className="flex items-start gap-2">
                  <input
                    type="radio"
                    name="template"
                    className="mt-1"
                    checked={template === item.id}
                    onChange={() => setTemplate(item.id)}
                  />
                  <span>
                    <span className="block font-medium">{templateCopy(t, item.id).name}</span>
                    <span className="block text-sm text-muted">{templateCopy(t, item.id).summary}</span>
                  </span>
                </span>
              </label>
            ))}
            <label
              className={`cursor-pointer rounded-md border p-3 ${template === "custom" ? "border-pine bg-pine-soft" : "border-line"}`}
            >
              <span className="flex items-start gap-2">
                <input
                  type="radio"
                  name="template"
                  className="mt-1"
                  checked={template === "custom"}
                  onChange={() => setTemplate("custom")}
                />
                <span>
                  <span className="block font-medium">{t("Custom")}</span>
                  <span className="block text-sm text-muted">
                    {t("Choose each area this agent may read or change.")}
                  </span>
                </span>
              </span>
            </label>
          </div>
          {template === "custom" ? (
            <ul className="mt-4 space-y-3">
              {groups.map((group) => (
                <li key={group.group} className="rounded-md border border-line p-3">
                  <p className="text-sm font-medium">{areaLabel(t, group.group)}</p>
                  <div className="mt-2 flex flex-wrap gap-4 text-sm">
                    {group.scopes.map((scope) => (
                      <label key={scope.id} className="flex items-center gap-2">
                        <input
                          type="checkbox"
                          checked={custom.includes(scope.id)}
                          onChange={(event) => toggle(scope.id, scope.write, event.target.checked)}
                        />
                        {scope.write ? t("Change") : t("Read")}
                      </label>
                    ))}
                  </div>
                </li>
              ))}
            </ul>
          ) : null}
          <form
            className="mt-4 grid gap-3"
            onSubmit={(event) => {
              event.preventDefault();
              approve.mutate();
            }}
          >
            <Field label={t("Name")} hint={t("Shown in the list of approved agents. Optional.")}>
              <TextInput
                value={name}
                onChange={(event) => setName(event.target.value)}
                placeholder={client.data?.client_name ?? ""}
              />
            </Field>
            <div>
              <Button type="submit" disabled={approve.isPending}>
                {approve.isPending ? t("Approving…") : t("Approve")}
              </Button>
            </div>
          </form>
        </Card>
      )}
    </>
  );
}
