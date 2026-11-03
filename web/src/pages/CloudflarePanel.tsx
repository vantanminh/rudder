import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent, type ReactNode } from "react";
import { toast } from "sonner";
import { api, ApiError } from "../api";
import {
  Alert,
  Badge,
  Button,
  Card,
  CodeBlock,
  Field,
  Select,
  TextArea,
  TextInput,
} from "../components/ui";
import { useT } from "../i18n";
import type { Schemas } from "../types";

type Status = Schemas["CloudflareStatus"];

const STATUS_KEY = ["cloudflare"];
const READINESS_KEY = ["cloudflare-readiness"];

/**
 * The optional Cloudflare Zero Trust mode: a tunnel in front of every app, Access in
 * front of the panel, and SSH through Cloudflare. Nothing here is needed to run Rudder.
 */
export function CloudflarePanel() {
  const t = useT();
  const status = useQuery({
    queryKey: STATUS_KEY,
    queryFn: () => api<Status>("/api/v1/cloudflare"),
  });
  const data = status.data;
  return (
    <div className="grid gap-4">
      <Alert title={t("Advanced and optional")}>
        {t(
          "Cloudflare Zero Trust puts every app and the panel behind Cloudflare's network, so the server's public ports can be closed. Nothing changes until you turn it on, and each step below can be undone.",
        )}
      </Alert>
      {status.isError ? (
        <Alert tone="error">{t("Could not load the Cloudflare settings.")}</Alert>
      ) : null}
      {data ? (
        <>
          <Account status={data} />
          <Tunnel status={data} />
          {data.tunnel_id ? <Publish status={data} /> : null}
          {data.tunnel_id ? <Access status={data} /> : null}
          {data.tunnel_id ? <Ssh status={data} /> : null}
          {data.tunnel_id ? <Ready status={data} /> : null}
        </>
      ) : (
        <p className="text-sm text-muted">{t("Loading…")}</p>
      )}
    </div>
  );
}

function useSave<T, R = unknown>(
  fn: (value: T) => Promise<R>,
  success: string,
  failure: string,
  after?: (result: R) => void,
) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: fn,
    onSuccess: async (result) => {
      toast.success(success);
      after?.(result);
      await queryClient.invalidateQueries({ queryKey: STATUS_KEY });
      await queryClient.invalidateQueries({ queryKey: READINESS_KEY });
    },
    onError: (error) => toast.error(error instanceof ApiError ? error.message : failure),
  });
}

function Step({ number, children }: { number: number; children: ReactNode }) {
  return (
    <span className="flex items-center gap-2">
      <span className="grid size-5 place-items-center rounded-full bg-sunken text-xs font-medium text-ink-soft">
        {number}
      </span>
      {children}
    </span>
  );
}

function Account({ status }: { status: Status }) {
  const t = useT();
  const [accountId, setAccountId] = useState(status.account_id);
  const [zoneId, setZoneId] = useState(status.zone_id);
  const [token, setToken] = useState("");
  const save = useSave(
    () =>
      api("/api/v1/cloudflare/credentials", {
        method: "PUT",
        body: JSON.stringify({
          account_id: accountId.trim(),
          zone_id: zoneId.trim(),
          api_token: token.trim() || undefined,
        }),
      }),
    t("Cloudflare account saved"),
    t("Could not save the Cloudflare account"),
    () => setToken(""),
  );
  return (
    <Card
      title={<Step number={1}>{t("Cloudflare account")}</Step>}
      description={t(
        "Create an API token with: Account · Cloudflare Tunnel · Edit, Account · Access: Apps and Policies · Edit, Account · Access: SSH Auditing · Edit, Account · Cloudflare One Connectors/Networks · Edit, and Zone · DNS · Edit. The zone must already be on Cloudflare.",
      )}
      actions={
        status.token_saved ? <Badge value="connected" tone="success" /> : <Badge value="not connected" tone="neutral" />
      }
    >
      <form
        className="grid gap-3 md:grid-cols-3"
        onSubmit={(event: FormEvent) => {
          event.preventDefault();
          save.mutate(undefined);
        }}
      >
        <Field label={t("Account ID")}>
          <TextInput
            required
            value={accountId}
            onChange={(event) => setAccountId(event.target.value)}
            disabled={Boolean(status.tunnel_id)}
          />
        </Field>
        <Field label={t("Zone ID")} hint={status.zone_name ? status.zone_name : undefined}>
          <TextInput
            required
            value={zoneId}
            onChange={(event) => setZoneId(event.target.value)}
            disabled={Boolean(status.tunnel_id)}
          />
        </Field>
        <Field
          label={t("API token")}
          hint={status.token_saved ? t("Saved. Leave blank to keep it.") : undefined}
        >
          <TextInput
            type="password"
            autoComplete="off"
            required={!status.token_saved}
            value={token}
            onChange={(event) => setToken(event.target.value)}
          />
        </Field>
        <div>
          <Button type="submit" disabled={save.isPending}>
            {t("Check and save")}
          </Button>
        </div>
      </form>
    </Card>
  );
}

function Tunnel({ status }: { status: Status }) {
  const t = useT();
  const [replicas, setReplicas] = useState(String(status.replicas));
  const options = Array.from({ length: status.max_replicas }, (_, index) => index + 1);
  const create = useSave(
    () =>
      api<Status>("/api/v1/cloudflare/tunnel", {
        method: "POST",
        body: JSON.stringify({ replicas: Number(replicas) }),
      }),
    t("Tunnel created"),
    t("Could not create the tunnel"),
  );
  const scale = useSave(
    () =>
      api<Status>("/api/v1/cloudflare/tunnel/replicas", {
        method: "PUT",
        body: JSON.stringify({ replicas: Number(replicas) }),
      }),
    t("Replicas updated"),
    t("Could not change the replicas"),
  );
  const remove = useSave(
    () => api<Schemas["TeardownReport"]>("/api/v1/cloudflare/tunnel", { method: "DELETE" }),
    t("Tunnel removed"),
    t("Could not remove the tunnel"),
    (report) => {
      if (report.removed_dns.length > 0) {
        toast.message(
          t("Point these domains back at this server: {hosts}", { hosts: report.removed_dns.join(", ") }),
        );
      }
      if (report.warnings.length > 0) {
        toast.warning(report.warnings.join(" · "));
      }
    },
  );
  const replicaError = create.data?.replica_error ?? scale.data?.replica_error;
  const picker = (
    <Field label={t("Replicas")} hint={t("Each replica opens its own connections to Cloudflare. Two or more keep the apps online if one stops.")}>
      <Select value={replicas} onChange={(event) => setReplicas(event.target.value)}>
        {options.map((count) => (
          <option key={count} value={count}>
            {count}
          </option>
        ))}
      </Select>
    </Field>
  );
  return (
    <Card
      title={<Step number={2}>{t("Tunnel")}</Step>}
      description={t(
        "One tunnel for this server. The replicas run as containers next to Traefik and connect out to Cloudflare, so no inbound port is needed.",
      )}
      actions={
        status.tunnel_id ? (
          <Badge
            value={
              (status.connections ?? 0) > 0 && status.replicas_running >= status.replicas
                ? "healthy"
                : "degraded"
            }
          />
        ) : null
      }
    >
      {status.tunnel_id ? (
        <div className="grid gap-4">
          <p className="text-sm">
            {t("{name} · {running} of {wanted} replicas running · {connections} connections", {
              name: status.tunnel_name,
              running: status.replicas_running,
              wanted: status.replicas,
              connections: status.connections ?? 0,
            })}
          </p>
          {replicaError ? (
            <Alert tone="warning" title={t("The replicas did not start")}>
              {replicaError}
            </Alert>
          ) : null}
          <div className="grid items-end gap-3 md:grid-cols-3">
            {picker}
            <div>
              <Button quiet disabled={scale.isPending} onClick={() => scale.mutate(undefined)}>
                {t("Apply")}
              </Button>
            </div>
          </div>
          <div>
            <Button
              variant="danger"
              disabled={remove.isPending}
              onClick={() => {
                if (
                  window.confirm(
                    t(
                      "Remove the tunnel? Rudder deletes its replicas, the DNS records it made, and the Access and SSH applications. Your domains stop working until you point them back at this server.",
                    ),
                  )
                ) {
                  remove.mutate(undefined);
                }
              }}
            >
              {t("Remove the tunnel")}
            </Button>
          </div>
        </div>
      ) : (
        <div className="grid items-end gap-3 md:grid-cols-3">
          {picker}
          <div>
            <Button
              disabled={create.isPending || !status.token_saved}
              onClick={() => create.mutate(undefined)}
            >
              {t("Create the tunnel")}
            </Button>
          </div>
        </div>
      )}
    </Card>
  );
}

function Publish({ status }: { status: Status }) {
  const t = useT();
  const [overwrite, setOverwrite] = useState(false);
  const publish = useSave(
    () =>
      api<Schemas["SyncReport"]>("/api/v1/cloudflare/tunnel/publish", {
        method: "POST",
        body: JSON.stringify({ overwrite_dns: overwrite }),
      }),
    t("Domains published through the tunnel"),
    t("Could not publish the domains"),
  );
  const report = publish.data;
  return (
    <Card
      title={<Step number={3}>{t("Publish domains")}</Step>}
      description={t(
        "Sends every app domain, the panel domain, and the Rudder Access portal through the tunnel and points their DNS at it. Traefik then serves them without Let's Encrypt, because Cloudflare presents the public certificate.",
      )}
      actions={status.tunnel_published ? <Badge value="published" tone="success" /> : null}
    >
      <div className="grid gap-3">
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={overwrite}
            onChange={(event) => setOverwrite(event.target.checked)}
          />
          {t("Replace existing A, AAAA, or CNAME records on these names")}
        </label>
        <div>
          <Button disabled={publish.isPending} onClick={() => publish.mutate(undefined)}>
            {status.tunnel_published ? t("Publish again") : t("Publish")}
          </Button>
        </div>
        {status.published_hosts.length > 0 ? (
          <p className="text-sm text-muted">
            {t("Published: {hosts}", { hosts: status.published_hosts.join(", ") })}
          </p>
        ) : null}
        {report ? (
          <ul className="space-y-1 text-sm">
            {report.dns.map((item) => (
              <li key={item.host}>
                {item.host} — {t(item.action)}
                {item.action === "conflict" && item.detail
                  ? ` (${item.detail}). ${t("Tick the box above to replace it.")}`
                  : ""}
              </li>
            ))}
            {report.skipped.map((item) => (
              <li key={item.host} className="text-muted">
                {item.host} — {item.reason}
              </li>
            ))}
          </ul>
        ) : null}
      </div>
    </Card>
  );
}

function Access({ status }: { status: Status }) {
  const t = useT();
  const [team, setTeam] = useState(status.team_domain);
  const [emails, setEmails] = useState(status.access_emails.join("\n"));
  const [bypass, setBypass] = useState(status.agent_bypass);
  const save = useSave(
    () =>
      api("/api/v1/cloudflare/access", {
        method: "PUT",
        body: JSON.stringify({
          team_domain: team.trim(),
          emails: emails
            .split(/[\s,;]+/)
            .map((email) => email.trim())
            .filter(Boolean),
          agent_bypass: bypass,
        }),
      }),
    t("Cloudflare Access saved"),
    t("Could not save Cloudflare Access"),
  );
  const enforce = useSave(
    (enforced: boolean) =>
      api("/api/v1/cloudflare/access/enforcement", {
        method: "PUT",
        body: JSON.stringify({ enforced }),
      }),
    t("Access enforcement updated"),
    t("Could not change enforcement"),
  );
  return (
    <Card
      title={<Step number={4}>{t("Protect the panel with Cloudflare Access")}</Step>}
      description={t(
        "Creates an Access application for the panel domain that only the listed emails can open. Rudder's own sign-in stays in place behind it.",
      )}
      actions={
        status.access_enforced ? (
          <Badge value="enforced" tone="success" />
        ) : status.access_configured ? (
          <Badge value="saved" tone="warning" />
        ) : null
      }
    >
      <form
        className="grid gap-3 md:grid-cols-2"
        onSubmit={(event: FormEvent) => {
          event.preventDefault();
          save.mutate(undefined);
        }}
      >
        <Field
          label={t("Team domain")}
          hint={t("Found under Zero Trust → Settings → Custom pages, such as myteam.cloudflareaccess.com.")}
        >
          <TextInput required value={team} onChange={(event) => setTeam(event.target.value)} />
        </Field>
        <Field label={t("Allowed emails")} hint={t("One per line.")}>
          <TextArea required value={emails} onChange={(event) => setEmails(event.target.value)} />
        </Field>
        <label className="flex items-start gap-2 text-sm md:col-span-2">
          <input
            type="checkbox"
            className="mt-1"
            checked={bypass}
            onChange={(event) => setBypass(event.target.checked)}
          />
          <span>
            {t(
              "Let AI agents and API clients through. Skips the Access login for /mcp, /oauth, /.well-known, and /api/health; they still need their own tokens.",
            )}
          </span>
        </label>
        <div>
          <Button type="submit" disabled={save.isPending}>
            {t("Save")}
          </Button>
        </div>
      </form>
      {status.access_configured ? (
        <div className="mt-4 grid gap-3 border-t border-line pt-4">
          <p className="text-sm text-muted">
            {t(
              "Turn enforcement on only after you opened the panel through its Cloudflare address and signed in with Access. Rudder then refuses requests for the panel domain that do not carry a valid Access token.",
            )}
          </p>
          <div>
            {status.access_enforced ? (
              <Button quiet disabled={enforce.isPending} onClick={() => enforce.mutate(false)}>
                {t("Stop requiring Access")}
              </Button>
            ) : (
              <Button disabled={enforce.isPending} onClick={() => enforce.mutate(true)}>
                {t("Require Access")}
              </Button>
            )}
          </div>
        </div>
      ) : null}
    </Card>
  );
}

function Ssh({ status }: { status: Status }) {
  const t = useT();
  const queryClient = useQueryClient();
  const [address, setAddress] = useState(status.ssh_address || "172.18.0.1");
  const [users, setUsers] = useState(status.ssh_users.join(", "));
  const script = useQuery({
    queryKey: ["cloudflare-ssh-script"],
    queryFn: () => api<Schemas["SshScript"]>("/api/v1/cloudflare/ssh/script"),
    enabled: status.ssh_configured,
  });
  const save = useSave(
    () =>
      api<Schemas["SshSetup"]>("/api/v1/cloudflare/ssh", {
        method: "PUT",
        body: JSON.stringify({
          address: address.trim(),
          users: users
            .split(/[\s,;]+/)
            .map((user) => user.trim())
            .filter(Boolean),
        }),
      }),
    t("SSH through Cloudflare saved"),
    t("Could not set up SSH"),
    () => void queryClient.invalidateQueries({ queryKey: ["cloudflare-ssh-script"] }),
  );
  const confirm = useSave(
    (confirmed: boolean) =>
      api("/api/v1/cloudflare/ssh/confirmation", {
        method: "PUT",
        body: JSON.stringify({ confirmed }),
      }),
    t("Saved"),
    t("Could not save"),
  );
  const remove = useSave(
    () => api("/api/v1/cloudflare/ssh", { method: "DELETE" }),
    t("SSH through Cloudflare removed"),
    t("Could not remove SSH"),
  );
  return (
    <Card
      title={<Step number={5}>{t("SSH through Cloudflare")}</Step>}
      description={t(
        "Optional. Cloudflare signs a short-lived SSH certificate for each login, so port 22 can be closed and no SSH keys are kept on the server. Each person needs the Cloudflare One Client with the Gateway proxy for TCP turned on.",
      )}
      actions={
        status.ssh_ready ? (
          <Badge value="ready" tone="success" />
        ) : status.ssh_configured ? (
          <Badge value="saved" tone="warning" />
        ) : null
      }
    >
      <form
        className="grid gap-3 md:grid-cols-2"
        onSubmit={(event: FormEvent) => {
          event.preventDefault();
          save.mutate(undefined);
        }}
      >
        <Field
          label={t("Server address")}
          hint={t("An IPv4 address the tunnel can reach port 22 on, such as the Docker gateway of the rudder-proxy network.")}
        >
          <TextInput required value={address} onChange={(event) => setAddress(event.target.value)} />
        </Field>
        <Field label={t("Unix accounts")} hint={t("Comma separated, such as deploy, root.")}>
          <TextInput required value={users} onChange={(event) => setUsers(event.target.value)} />
        </Field>
        <div className="flex gap-2">
          <Button type="submit" disabled={save.isPending}>
            {t("Save")}
          </Button>
          {status.ssh_configured ? (
            <Button
              variant="danger"
              type="button"
              disabled={remove.isPending}
              onClick={() => remove.mutate(undefined)}
            >
              {t("Remove")}
            </Button>
          ) : null}
        </div>
      </form>
      {status.ssh_configured && script.data ? (
        <div className="mt-4 grid gap-3 border-t border-line pt-4">
          <p className="text-sm text-muted">
            {t(
              "Run this as root on the server. It makes sshd trust Cloudflare's certificates and checks the configuration before reloading.",
            )}
          </p>
          <CodeBlock copy>{script.data.script}</CodeBlock>
          <div>
            {status.ssh_ready ? (
              <Button quiet disabled={confirm.isPending} onClick={() => confirm.mutate(false)}>
                {t("Mark as not tested")}
              </Button>
            ) : (
              <Button disabled={confirm.isPending} onClick={() => confirm.mutate(true)}>
                {t("I ran it and connected")}
              </Button>
            )}
          </div>
        </div>
      ) : null}
    </Card>
  );
}

function Ready({ status }: { status: Status }) {
  const t = useT();
  const readiness = useQuery({
    queryKey: READINESS_KEY,
    queryFn: () => api<Schemas["Readiness"]>("/api/v1/cloudflare/preflight"),
  });
  const data = readiness.data;
  return (
    <Card
      title={<Step number={6}>{t("Close the public ports")}</Step>}
      description={t(
        "Rudder never closes its own ports, so a mistake cannot lock you out from inside. When every required check passes, run the installer on the server.",
      )}
      actions={
        data ? <Badge value={data.ready ? "ready" : "not ready"} tone={data.ready ? "success" : "warning"} /> : null
      }
    >
      <ul className="space-y-1.5 text-sm">
        {(data?.checks ?? []).map((check) => (
          <li key={check.id}>
            <span className={check.ok ? "text-pine" : check.required ? "text-danger" : "text-muted"}>
              {check.ok ? "✓" : check.required ? "✗" : "–"}
            </span>{" "}
            {check.message}
            {check.required ? "" : ` ${t("(optional)")}`}
          </li>
        ))}
      </ul>
      {status.tunnel_published && data?.ready ? (
        <div className="mt-4 grid gap-2">
          <p className="text-sm text-muted">
            {t(
              "Stops Rudder from listening on ports 80, 443, and 3000. Port 7443 stays open for remote servers' agents. Port 22 belongs to your provider's firewall: close it there once SSH through Cloudflare works. To undo it, run the same command with --open-ports.",
            )}
          </p>
          <CodeBlock copy>
            {"curl -fsSL https://raw.githubusercontent.com/vantanminh/rudder/main/deploy/install.sh | sudo sh -s -- --close-ports"}
          </CodeBlock>
        </div>
      ) : null}
    </Card>
  );
}
