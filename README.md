# Rudder

Rudder is a lightweight, self-hosted deployment platform, an alternative to Dokploy or Coolify for people who want one VPS to run everything. One server runs the control panel. Extra servers run a small agent that dials out to the panel and are managed from the same dashboard. There is no cluster scheduler: each server is independent.

What you get:

- **Apps** from container images (including private registries such as GHCR) or from Git with Dockerfile, Nixpacks, or static builds, push webhooks (one-click GitHub App setup), health-checked deploys, and rollback.
- **Domains and TLS** through Traefik v3 (Let's Encrypt or uploaded certificates), plus an optional Pingora-based HTTP cache per server.
- **Databases**: Postgres, MySQL, MariaDB, MongoDB, and Redis, with streaming encrypted backups to any S3-compatible storage.
- **Docker Compose stacks** and 11 one-click templates (n8n, Uptime Kuma, Gitea, Vaultwarden, Ghost, WordPress, and more).
- **Operations**: central logs with live tail, a web terminal, metrics and alerts (Telegram, Discord, Slack, webhooks, email), a load guard that keeps the panel reachable when builds or traffic overload the server, panel backup and disaster recovery, and signed self-updates with rollback.
- **Access**: users with admin and viewer roles, TOTP, scoped API tokens, an audit log, an OpenAPI document at `/api/docs`, and the `rudder` CLI.

## Quick start

On a fresh Linux VPS with ports 80, 443, 3000, and 7443 open:

```sh
curl -fsSL https://github.com/vantanminh/rudder/releases/latest/download/install.sh | sudo sh
```

The installer sets up Docker if it is missing. It starts Postgres, Traefik, `rudder-cache`, and the panel as containers, and prints the panel URL. Open it and create the owner account. Pass `--domain panel.example.com --email you@example.com` to serve the panel over HTTPS. See `install.sh --help` for backup and restore options, and [disaster recovery](docs/disaster-recovery.md) for moving to a new host.

To add a second server, go to **Servers → Add server** and run the join command it shows on that machine:

```sh
curl -fsSL http://<panel>:3000/install-agent.sh | sudo sh -s -- --server http://<panel>:3000 --token join_...
```

The agent enrolls with a one-time token, then keeps an outbound gRPC session (mutual TLS) to the panel on port 7443. The server needs no inbound port except 80/443 for its own apps. Every page in the panel can then target that server. See [the agent](docs/agent.md).

From your laptop or CI — install the native `rudder` CLI on macOS/Linux (`curl -fsSL https://github.com/vantanminh/rudder/releases/latest/download/install-cli.sh | sh`) or Windows PowerShell (`irm https://github.com/vantanminh/rudder/releases/latest/download/install-cli.ps1 | iex`), sign in via API key, browser, or 8-character device code, and run local CI/CD builds pushed to Cloudflare R2 (see [docs/cli.md](docs/cli.md)):

```sh
rudder login --url https://panel.example.com --device  # or --browser, or paste an API token
rudder init shop/prod/web
rudder build && rudder push                            # local CI checks -> build -> push to Cloudflare R2
rudder deploy                                          # deploy from R2 (or `rudder deploy --build`)
rudder images list                                     # manage local and R2 image versions
rudder update                                          # native self-update from GitHub Releases
rudder deploy shop/web --image ghcr.io/acme/web:1.4.2
rudder logs shop/web -f
rudder cache purge shop/web --all
```

## Resource use

These are measured values, not estimates. They come from the x86_64 musl release binaries on a 4-vCPU, 16 GB Ubuntu 24.04 VM with Postgres 16 on the same host. The steps are at the end of this section.

| Process | Idle RSS | Target | Startup |
| --- | --- | --- | --- |
| `rudder-server` (control plane, web UI embedded) | 27.3 MiB (peak 42.6 MiB) | < 60 MB | 38–44 ms to `/api/health` (warm); 402 ms on an empty database including all migrations |
| `rudder-agent` | 9.1 MiB | < 15 MB | 85 ms to a connected session |
| `rudder-cache` | 5.0 MiB | < 30 MB | 7 ms to `/health` |

- **Idle:** 60 seconds after start, with one agent connected, the panel sampling local Docker every 10 s, and the cache running with an empty rule set. Values are `VmRSS` and `VmHWM` from `/proc/<pid>/status`.
- **Long-running check:** in the containerized install used for the update drill, after about 35 minutes with an agent connected and one app deployed, the panel container's `rudder-server` used 23.2 MiB, `rudder-cache` 5.2 MiB, and the remote `rudder-agent` 8.6 MiB.
- **Fresh install after the walkthrough:** `docker stats` (cgroup memory) showed the `rudder` panel container at 22.7 MiB and each `rudder-cache` container at 1.5 MiB, with Traefik at 26.8 MiB and Postgres at 69.0 MiB. The second server's agent had a `VmRSS` of 10.1 MiB.
- **Binary sizes:** `rudder-server` 38 MB, `rudder-agent` 12 MB, `rudder-cache` 8.4 MB, `rudder` 6.6 MB.

The table does not include Postgres, Traefik, or your apps.

<details>
<summary>How the numbers were taken</summary>

1. Built the binaries with `SQLX_OFFLINE=true cargo build --release --target x86_64-unknown-linux-musl -p rudder-server -p rudder-agent -p rudder-cache`.
2. Started the panel against a fresh database and timed it until `/api/health` answered. Restarted it five times and timed each warm start.
3. Created the owner account and a remote server, then enrolled and started the agent.
4. Started `rudder-cache --rules rules.json` and slept 60 seconds before reading `/proc/<pid>/status`.

</details>

## Documentation

| Topic | Document |
| --- | --- |
| Remote servers and the agent protocol | [docs/agent.md](docs/agent.md) |
| Databases and backups | [docs/backups.md](docs/backups.md) |
| Compose stacks and templates | [docs/stacks.md](docs/stacks.md) |
| GitHub App and push-to-deploy | [docs/github-app.md](docs/github-app.md) |
| HTTP cache | [docs/cache.md](docs/cache.md) |
| Central logs and terminal | [docs/logs.md](docs/logs.md) |
| Monitoring and alerts | [docs/monitoring.md](docs/monitoring.md) |
| Load guard and overload protection | [docs/load-guard.md](docs/load-guard.md) |
| Panel backup and disaster recovery | [docs/disaster-recovery.md](docs/disaster-recovery.md) |
| Self-update and releases | [docs/updates.md](docs/updates.md) |
| Command line client | [docs/cli.md](docs/cli.md) |
| Architecture | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) |
| Design decisions | [docs/decisions/](docs/decisions/) |

The API is described by the OpenAPI document served at `/api/openapi.json`, with a viewer at `/api/docs`. The web UI's types are generated from the same document (`web/openapi.json`).

## Workspace

| Path | Role |
| --- | --- |
| `crates/core` | Domain types, validation, release verification, the `NodeDriver` trait |
| `crates/node` | Docker driver shared by the panel and the agent |
| `crates/proto` | Agent session protobuf |
| `crates/server` | Control plane (Axum, PostgreSQL, embedded web UI) |
| `crates/agent` | Node agent |
| `crates/cache` | Pingora HTTP cache |
| `crates/cli` | `rudder` command line client |
| `web` | React panel (React Router 7, TanStack Query, Tailwind) |
| `deploy` | `install.sh`, `install-agent.sh`, Dockerfiles, release tooling |

## Develop

You need Rust 1.88, pnpm 10, Node 22, PostgreSQL 16, and Docker. Copy `.env.example` and export the variables, then:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
SQLX_OFFLINE=true cargo test --workspace
pnpm --dir web install
pnpm --dir web build
```

Tests that start real containers are skipped unless `RUDDER_DOCKER_TESTS=1` is set.

Compile-time checked SQL uses `sqlx`. CI sets `SQLX_OFFLINE=true` and reads the committed `.sqlx` cache. After a query change, apply the migrations and refresh the cache:

```bash
DATABASE_URL=postgres://rudder:rudder@127.0.0.1:5432/rudder cargo sqlx prepare --workspace -- --all-targets
```

After an API change, regenerate the OpenAPI snapshot and the frontend types. A test fails when either is stale or when a route is missing from the document.

```bash
cargo run -p rudder-server -- openapi > web/openapi.json && pnpm --dir web gen:api
```

The `CI` workflow runs these gates on every push and pull request. Pushing a `v*` tag runs the `Release` workflow, which builds musl binaries for x86_64 and aarch64 and multi-arch images. It then signs `SHA256SUMS` with minisign and publishes the release (see [docs/updates.md](docs/updates.md)).

## Status and limits

Rudder targets a few servers per panel. It has no Kubernetes, Swarm, clustering or high availability, billing, or multi-tenant organizations.

Verified end to end on Docker-in-Docker hosts:

- `install.sh`, the first admin, `rudder login`/`deploy`/`logs`/`cache purge`, a deploy with a domain and HTTP caching, and a second server joined with `install-agent.sh` that serves its own deploy and cache (systemd replaced by a stub that runs the unit's `ExecStart`),
- backup and restore onto a new host,
- signed self-update with rollback.

Not verified yet:

- the GitHub release workflow itself (it runs on the first tag),
- arm64 builds,
- real systemd restarts,
- Let's Encrypt with a public domain,
- notification delivery to real Telegram, Slack, Discord, or SMTP providers.

The PR history lists the details.

The panel is available in English and Vietnamese; each person picks one on the Account page. Interface text is written in English inside `t("…")` calls and translated by `web/src/i18n/vi.ts`, keyed by the English text. After adding or changing interface text, add the Vietnamese entry and run `pnpm --dir web check:i18n`, which fails on a missing entry or a placeholder that differs between the two.
