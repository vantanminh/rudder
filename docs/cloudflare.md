# Cloudflare Zero Trust

An optional, advanced mode in **Security → Cloudflare**. It puts the panel and every app on this server behind Cloudflare, so the server can stop listening on ports 80, 443, and 3000 and SSH can go through Cloudflare too. Nothing in it is required: an install that never opens the tab behaves exactly as before, and each step can be undone.

The decision record is [0025](decisions/0025-cloudflare-zero-trust.md).

## What you need

- A domain whose DNS is on Cloudflare (the zone).
- A Cloudflare account with Zero Trust enabled (the free plan is enough for a few people).
- An API token with these permissions:
  - Account · Cloudflare Tunnel · Edit
  - Account · Access: Apps and Policies · Edit
  - Account · Access: SSH Auditing · Edit (only for SSH)
  - Account · Cloudflare One Connectors/Networks · Edit (only for SSH)
  - Zone · DNS · Edit

The token and the tunnel token are encrypted with the master key and are never returned by the API. Leave the token field blank to keep the saved one.

## Steps

Do them in order. The page shows what each step did and what is still missing.

1. **Cloudflare account.** Save the account id, zone id, and token. Rudder checks each permission and says which one is missing.
2. **Tunnel.** Create one tunnel and choose one to five replicas. Each replica is a `rudder-cloudflared-N` container on the `rudder-proxy` network that connects out to Cloudflare, so no inbound port is needed. With two or more, the apps stay online when one replica or one Cloudflare connection is lost. Changing the number adds or removes containers without restarting the others.
3. **Publish domains.** Sends every app domain on this server, the panel domain, and the Rudder Access portal through the tunnel, and points their DNS at it with proxied CNAME records. Rudder does not replace an existing A, AAAA, or CNAME record unless you tick the box for it; the page lists the names it skipped. From then on Traefik serves those domains on HTTPS with Cloudflare's certificate, with no Let's Encrypt request and no HTTP redirect router, because port 80 can be closed. Domains added later are published when you publish again.
4. **Cloudflare Access for the panel.** Enter your team domain (`myteam.cloudflareaccess.com`) and the emails allowed in. Rudder creates an Access application for the panel domain. Rudder's own sign-in, authenticator, and Rudder Access for internal apps stay in place behind it.
   - **Let AI agents and API clients through** adds Access bypass applications for `/mcp`, `/oauth`, `/.well-known`, and `/api/health`, so MCP clients can still reach the panel. They still need their own tokens.
   - **Require Access** makes the panel refuse requests for its own domain that do not carry a valid `Cf-Access-Jwt-Assertion` (signature, issuer, audience, and expiry are checked against the team's published keys). Turn it on only after you opened the panel through its Cloudflare address and signed in with Access; Rudder refuses to turn it on from a request that does not already carry a valid token, so you cannot lock yourself out. Turning it off is always allowed.
5. **SSH through Cloudflare** (optional). Cloudflare signs a short-lived SSH certificate for each login, so no SSH keys are kept on the server.
   - Enter an IPv4 address the tunnel can reach port 22 on (for example the Docker gateway of the `rudder-proxy` network) and the Unix accounts that may log in.
   - Rudder creates the route, the infrastructure target and application, and reads the account's SSH CA. It does not edit `sshd_config`. Instead it shows a script to run as root on the server; the script makes `sshd` trust the CA and checks the configuration before reloading.
   - After you connected once, press **I ran it and connected**. Until then SSH is not counted as ready.
   - Each person needs the Cloudflare One Client with the Gateway proxy for TCP turned on.
6. **Close the public ports.** The page runs a preflight: tunnel connected, replicas running, domains published, Access enforced, and SSH confirmed when it was set up. When every required check passes it shows the installer command:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/vantanminh/rudder/main/deploy/install.sh | sudo sh -s -- --close-ports
   ```

   `--close-ports` refuses to run unless a tunnel replica is running. It stops publishing ports 80 and 443 on Traefik and publishes the panel on `127.0.0.1:3000` only; port 7443 stays open for agents on remote servers. The choice is remembered in `/etc/rudder/ports.closed`, so upgrading later does not reopen anything. Run the installer with `--open-ports` to undo it.

   Rudder never closes its own listener from inside the panel, and it does not change the firewall. Port 22 belongs to your provider's firewall: close it there once SSH through Cloudflare works.

## Removing it

**Remove the tunnel** deletes the replicas, the DNS records Rudder created, the Access and SSH applications, the route, and the target. Domains stop working until you point them back at the server (the page lists them). Routing returns to direct mode with Let's Encrypt on the next publish. If the ports were closed, run the installer with `--open-ports` first.

## Behind the tunnel

- The visitor's address comes from `CF-Connecting-IP`, trusted only when the tunnel is on and the request arrived from a private or loopback hop. It feeds rate limits, the Access gate, and the audit log.
- The panel's Access check is cached for 30 seconds and Cloudflare's keys for 10 minutes.
- A Cloudflare error is shown as a message on the page (HTTP 424); it never makes the panel look offline.

## For AI agents

Everything above is available through the HTTP API and the MCP server under the `cloudflare` scope. **Full control** can change it; the **Operate** template can only read the status and the preflight. Audit actions start with `cloudflare.`.

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/api/v1/cloudflare` | Settings, tunnel health, and what each step has done |
| PUT | `/api/v1/cloudflare/credentials` | Check and save the account id, zone id, and token |
| POST, DELETE | `/api/v1/cloudflare/tunnel` | Create the tunnel, or remove it and everything it made |
| PUT | `/api/v1/cloudflare/tunnel/replicas` | Set the number of replicas (1 to 5) |
| POST | `/api/v1/cloudflare/tunnel/publish` | Publish the domains; `overwrite_dns` replaces existing records |
| PUT | `/api/v1/cloudflare/access` | Create or update the Access application |
| PUT | `/api/v1/cloudflare/access/enforcement` | Require or stop requiring the Access token |
| PUT, DELETE | `/api/v1/cloudflare/ssh` | Set up or remove SSH through Cloudflare |
| GET | `/api/v1/cloudflare/ssh/script` | The script that makes `sshd` trust the CA |
| PUT | `/api/v1/cloudflare/ssh/confirmation` | Record that an SSH login worked |
| GET | `/api/v1/cloudflare/preflight` | The checks to pass before closing the ports |

## Settings

| Variable | Default | Meaning |
| --- | --- | --- |
| `RUDDER_CLOUDFLARE_API_URL` | `https://api.cloudflare.com/client/v4` | Cloudflare API address; only honored by development builds, official releases always use Cloudflare |
| `RUDDER_CLOUDFLARED_IMAGE` | `cloudflare/cloudflared:latest` | Image the replicas run |
