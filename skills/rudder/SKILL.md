---
name: rudder
description: Operate a self-hosted Rudder panel — deploy and manage services, build and push images with the `rudder` CLI, read logs, purge the cache, and look up Rudder's own documentation. Use when the user mentions Rudder, rudder.toml, `rudder` commands, their Rudder panel, or deploying to a server managed by Rudder.
---

<!-- rudder-skill: managed by `rudder skills install` (rudder {{VERSION}}). Re-run it to update; edits are overwritten. -->

# Rudder

Rudder is a self-hosted deployment panel. A panel (web UI + API) manages **servers** (each runs a Rudder agent), grouped into **projects → environments → services**. A service is an application, a compose stack, or a managed database. The panel also handles domains and certificates, HTTP cache, backups, logs and metrics, and image deploys.

You can drive it two ways. Use whichever is available; both do the same things.

- **`rudder` CLI** — for work in a repository: build a Docker image, push it, deploy, tail logs, purge cache.
- **Rudder MCP server** — the panel's own API as tools (`<panel URL>/mcp`). Every panel feature is a tool. What you may call depends on what the user approved when connecting you.

## Look it up before you guess

Rudder's documentation is built into each panel, so it always matches the installed version. Search it instead of relying on memory:

- MCP: `get_v1_docs_search` with `query: {"q": "deploy an image"}`, then `get_v1_docs__name_` with `path: {"name": "cli"}` to read a whole guide. `get_v1_docs` lists the guides. These need the `system:read` permission.
- CLI: `rudder docs search "deploy an image"`, `rudder docs read cli`, `rudder docs list`.

Guides worth knowing: `cli` (build/push/deploy), `mcp` (connecting agents and permissions), `updates`, `cache`, `backups`, `stacks`, `cloudflare`, `security`, `logs`, `monitoring`.

## First steps in any session

1. Check you are signed in and see which panel and role you have: `rudder whoami` (CLI) or the `rudder_whoami` tool (MCP).
2. List what exists: `rudder services` or `get_v1_services`.
3. Refer to a service by id, name, `project/name`, or `project/environment/name`. If `rudder.toml` exists in the repo, its `service` is the default.

## Sign in (CLI)

```sh
rudder login --url https://panel.example.com --device    # prints an 8-character code to approve in the panel
rudder login --url https://panel.example.com --browser   # opens the browser
```

Do not put a token on the command line. An API token goes in `RUDDER_TOKEN` or on stdin. `RUDDER_URL` sets the panel address. Never print or commit tokens.

## Ship an image from a repository

`rudder.toml` at the repository root:

```toml
service = "shop/prod/web"        # project/environment/name
dockerfile = "Dockerfile"
context = "."
platform = "linux/amd64"         # the server's architecture
checks = ["cargo test --workspace"]
```

```sh
rudder build                 # runs `checks`, builds with BuildKit, saves the image locally
rudder push --deploy         # uploads to the panel's bucket, verifies the digest, deploys
rudder images list           # versions; also: rollback TAG, prune --keep N, remove TAG
```

`rudder init` creates `rudder.toml`. `GIT_SHA` and `BUILD_TIME` are passed to the build automatically. Make sure `.dockerignore` excludes `target/`, `node_modules/` and `.git/`: the build warns above 200 MiB of context.

## Everyday commands

```sh
rudder logs shop/web -n 200 --since 1h --level error --grep timeout   # add -f to follow
rudder cache purge shop/web --path /index.html                        # or --prefix /blog/ or --all
rudder status --check        # Rudder Image setup, with a live bucket test
rudder deploy shop/web --image TAG
```

Run any command with `--help` for its options.

## Working through MCP

- Tool names follow the HTTP route: `get_v1_services`, `post_v1_services__id__deploy`, `delete_v1_services__id_`. Pass path parameters as `path`, query parameters as `query`, and the request body as `body`.
- A tool the user did not grant returns a permission error. Say which permission is missing; do not try to work around it.
- Large results are truncated. Narrow with query parameters (`limit`, `since`, `level`).

## Rules of engagement

- **Look before you change.** Read the service (status, domains, env) before editing it.
- **Ask before anything destructive or hard to undo**: deleting a project, environment, service, database, domain or image; restoring a backup; stopping a production service; rolling back; panel updates. Deleting a project or environment removes every service in it and their containers.
- Deploy and rollback affect live traffic. Confirm the target environment (`prod` vs `staging`) first.
- Secrets: environment variables can hold secrets. Do not echo their values into chat or logs, and do not write them to files in the repository.
- When a command fails, read the error and the relevant guide (search the docs) before retrying. Do not loop on the same failing call.
- If something is not covered here or in the docs, say so rather than inventing a flag or endpoint.
