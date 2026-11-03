---
created_at: "2026-11-03T14:57:34.366452500+00:00"
doc: docs/decisions/0025-cloudflare-zero-trust.md
id: "0025"
notes: Opt-in. Tunnel replicas on the local server; Traefik tunnel variant; Access JWT enforced by the panel; ports closed only by an explicit installer step.
status: accepted
title: "Optional Cloudflare Zero Trust mode: tunnel, Access, and SSH"
type: decision
updated_at: "2026-11-03T14:57:34.366457500+00:00"
verify: null
---

# Optional Cloudflare Zero Trust mode: tunnel, Access, and SSH

Date: 2026-11-03

## Status

Accepted

## Context

A fresh Rudder server answers on its public IP: ports 80 and 443 for apps, 3000 for the panel, 7443 for agents, and 22 for SSH. Once the address is known, anyone can probe SSH, flood the proxy, or open the panel sign-in page. Rate limits and Rudder Access sit behind that address, so they cannot stop traffic that already reached the host.

People who want a stricter setup need the origin to have no public listener at all, the panel to sit behind an identity provider, and SSH to stay possible. Cloudflare Tunnel (`cloudflared`) makes outbound-only connections, Cloudflare Access puts an identity check in front of a hostname, and Access for Infrastructure issues short-lived SSH certificates.

## Decision

Add an **optional** mode under **Security**. Nothing changes for installations that do not turn it on.

1. **One remotely managed tunnel for the local server.** The panel creates it through the Cloudflare API and runs one to five `cloudflared` replicas as containers on the `rudder-proxy` network. Each replica opens its own connections to Cloudflare, so a lost connection, container, or data center does not take the apps offline. Remote servers are not part of this change.
2. **Hostnames are published from Rudder's own data.** Every domain on the local server, the panel domain, and the Access portal host become ingress rules that point to Traefik over HTTPS on the Docker network with certificate verification off and the hostname as SNI. DNS records are created as proxied CNAMEs to `<tunnel>.cfargotunnel.com`. An existing record is replaced only when the operator says so.
3. **Traefik renders a tunnel variant.** When the tunnel is on, every route is served on the `websecure` entry point with the default TLS store and no ACME resolver, and HTTP redirect routers are dropped. Cloudflare presents the public certificate; Let's Encrypt HTTP-01 could not work with port 80 closed.
4. **Cloudflare Access protects the panel hostname.** The panel creates a self-hosted Access application with an email allow policy. When enforcement is on, the panel also rejects requests for its own hostname that do not carry a valid `Cf-Access-Jwt-Assertion` (RS256 signature from the team's published keys, issuer, audience, expiry). Rudder Access for internal apps stays as it is.
5. **SSH uses Access for Infrastructure.** The panel gets the account's SSH CA, creates a tunnel route for the server address, an infrastructure target and application with the allowed Unix users, and shows a script the operator runs on the host to trust the CA. The panel does not edit the host's `sshd_config`. A browser-rendered terminal application can be added as a fallback.
6. **Closing the ports is a separate, explicit step.** The panel reports a preflight (tunnel healthy, panel domain published, Access enforced, SSH route present) and the installer has `--close-ports` and `--open-ports`. The panel never closes its own listener, so a bad configuration cannot lock the operator out from inside.
7. **Secrets** (API token, tunnel token) are encrypted with the master key like other credentials and are never returned by the API.

## Consequences

- The API token needs Account: Cloudflare Tunnel Edit, Access: Apps and Policies Edit, Access: SSH Auditing Edit, Cloudflare One Networks Edit, and Zone: DNS Edit. The panel says which call failed when a permission is missing.
- The zone must already be on Cloudflare. Hostnames outside the zone are listed and skipped.
- AI agents and API clients reach the panel through Access. Their paths can be exempted with a bypass application, which is off by default.
- Client-side steps for Access for Infrastructure (Cloudflare One Client, device enrollment, Gateway TCP proxy, split tunnels) stay with the operator; the guide lists them.
- Tests use a fake Cloudflare API server. The real API is not called in CI.
