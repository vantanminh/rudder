import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { useSearchParams } from "react-router-dom";
import { toast } from "sonner";
import { api, ApiError } from "../api";
import { Badge, Button, Card, Field, PickField, QuickPick, TextInput } from "../components/ui";
import { formatTime } from "../format";
import { useT } from "../i18n";
import { TOKEN_NAMES, TOKEN_SCOPES } from "../presets";
import type { CreatedToken, Schemas, TokenView } from "../types";

type CliAuthPublicView = Schemas["CliAuthRequestView"];
type CliAuthApproveResponse = Schemas["CliAuthApproveResponse"];

export function TokensPage() {
  const t = useT();
  const queryClient = useQueryClient();
  const [params, setParams] = useSearchParams();
  const cliRequestId = params.get("cli_request") ?? "";

  const tokens = useQuery({
    queryKey: ["tokens"],
    queryFn: () => api<TokenView[]>("/api/v1/tokens"),
  });
  const cliRequest = useQuery({
    queryKey: ["cli-auth-request", cliRequestId],
    queryFn: () => api<CliAuthPublicView>(`/api/v1/cli/auth/requests/${cliRequestId}`),
    enabled: Boolean(cliRequestId),
  });

  const [name, setName] = useState("");
  const [write, setWrite] = useState(false);
  const [revealed, setRevealed] = useState<string | null>(null);
  const [deviceCode, setDeviceCode] = useState("");

  const create = useMutation({
    mutationFn: () =>
      api<CreatedToken>("/api/v1/tokens", {
        method: "POST",
        body: JSON.stringify({
          name,
          scopes: write ? ["read", "write"] : ["read"],
        }),
      }),
    onSuccess: async (created) => {
      setName("");
      setRevealed(created.token);
      toast.success(t("Token created. Copy it now; it is not shown again."));
      await queryClient.invalidateQueries({ queryKey: ["tokens"] });
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : t("Could not create token")),
  });

  const approveCli = useMutation({
    mutationFn: (payload: { request_id?: string; device_code?: string }) =>
      api<CliAuthApproveResponse>("/api/v1/cli/auth/approve", {
        method: "POST",
        body: JSON.stringify(payload),
      }),
    onSuccess: async (approved) => {
      setDeviceCode("");
      await queryClient.invalidateQueries({ queryKey: ["tokens"] });
      if (approved.callback_url && approved.token) {
        const sep = approved.callback_url.includes("?") ? "&" : "?";
        window.location.href = `${approved.callback_url}${sep}token=${encodeURIComponent(approved.token)}`;
        return;
      }
      if (cliRequestId) {
        params.delete("cli_request");
        setParams(params, { replace: true });
      }
      toast.success(t("CLI login approved. Return to your terminal."));
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : t("Could not approve CLI login")),
  });

  const remove = useMutation({
    mutationFn: (id: string) => api(`/api/v1/tokens/${id}`, { method: "DELETE" }),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["tokens"] });
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : t("Could not revoke token")),
  });

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    create.mutate();
  }

  function authMethodLabel(method?: string) {
    if (method === "browser") return t("Browser login");
    if (method === "device_code") return t("Device code");
    return t("API key");
  }

  return (
    <>
      <h1 className="text-2xl font-semibold">{t("API tokens")}</h1>
      {cliRequest.data && cliRequest.data.status === "pending" ? (
        <Card
          title={t("Authorize rudder CLI in browser")}
          description={t("Approve this browser sign-in request to connect your terminal.")}
        >
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div>
              <p className="font-medium">{cliRequest.data.client_name}</p>
              <p className="text-sm text-muted">{cliRequest.data.scopes.join(", ")}</p>
            </div>
            <Button
              onClick={() => approveCli.mutate({ request_id: cliRequest.data.id })}
              disabled={approveCli.isPending}
            >
              {t("Approve CLI login")}
            </Button>
          </div>
        </Card>
      ) : null}
      <Card
        title={t("Authorize CLI with device code")}
        description={t(
          "Ran `rudder login --device`? Enter the 8-character alphanumeric code shown in your terminal.",
        )}
      >
        <form
          className="grid gap-3 md:grid-cols-[1fr_auto]"
          onSubmit={(event) => {
            event.preventDefault();
            approveCli.mutate({ device_code: deviceCode.trim().toUpperCase() });
          }}
        >
          <Field label={t("8-character device code")}>
            <TextInput
              required
              maxLength={12}
              placeholder="A1B2C3D4"
              value={deviceCode}
              onChange={(event) => setDeviceCode(event.target.value.toUpperCase())}
            />
          </Field>
          <div className="flex items-end">
            <Button type="submit" disabled={approveCli.isPending || !deviceCode.trim()}>
              {t("Authorize CLI")}
            </Button>
          </div>
        </form>
      </Card>
      <Card title={t("New token")}>
        <form className="grid gap-3 md:grid-cols-[1fr_auto_auto]" onSubmit={onSubmit}>
          <Field
            label={t("Name")}
            presets={
              <QuickPick
                label={t("Name suggestions")}
                options={TOKEN_NAMES}
                value={name}
                onPick={setName}
              />
            }
          >
            <TextInput required value={name} onChange={(event) => setName(event.target.value)} />
          </Field>
          <PickField
            label={t("Access")}
            options={TOKEN_SCOPES}
            value={write ? "write" : "read"}
            onPick={(scope) => setWrite(scope === "write")}
          />
          <div className="flex items-end">
            <Button type="submit" disabled={create.isPending}>
              {t("Create")}
            </Button>
          </div>
        </form>
        {revealed ? (
          <p className="mt-4 break-all rounded-md bg-paper p-3 font-mono text-sm">{revealed}</p>
        ) : null}
      </Card>
      <Card title={t("Active tokens")}>
        <ul className="divide-y divide-line">
          {(tokens.data ?? []).map((token) => (
            <li key={token.id} className="flex items-center justify-between gap-3 py-3">
              <div>
                <div className="flex flex-wrap items-center gap-2">
                  <p className="font-medium">{token.name}</p>
                  <Badge value={authMethodLabel(token.auth_method)} tone="neutral" />
                </div>
                <p className="text-sm text-muted">
                  {token.scopes.join(", ")} · {t("added {time}", { time: formatTime(token.created_at) })}
                </p>
              </div>
              <Button quiet onClick={() => remove.mutate(token.id)}>
                {t("Revoke")}
              </Button>
            </li>
          ))}
        </ul>
      </Card>
    </>
  );
}
