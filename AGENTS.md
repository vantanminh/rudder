# Agent Instructions

If `.agents/skills/harness/SKILL.md` exists, use it for the work loop,
mutation rules, and tool-only durable writes. `harness init` installs that
skill.

## Optional Harness Cloud sync

If this project opts into cloud sync, use `harness login` (default
https://5harness.knotree.com) followed by one `harness sync push` so later
durable edits auto-sync. Use `harness sync pull|status` and
`harness plan get <token>` for web-AI briefs. The Cloudflare Worker is the
canonical OAuth/MCP endpoint; an optional Pages proxy may serve the dashboard.
The CLI encrypts a restore snapshot before upload; a user-scoped catalog is
stored so hosted MCP can read harness entities. Do not sync `.5harness/`,
credentials, traces, indexes, or arbitrary files. Never commit `.env`, Firebase
service-account keys, Pages secrets, or a sync passphrase. Review the project's
cloud sync runbook before enabling it.

<!-- HARNESS:BEGIN -->
<!-- harness-version: 0.30.1 -->
<!-- harness-project-id: 2fa11256f8caaa0bb46f400cdfd97cfa -->
## Harness

**5harness** (bin `harness`) is this repo's operating system for **coding
agents**. Durable work — stories, decisions, intakes, backlog, reports — is
Git-backed markdown. **You** (the agent) read and write that history through
the CLI. You do not invent commands. You do not hand-edit those files.

### First commands

```bash
npm i -g 5harness          # if `harness` is missing
harness link               # after cloning a harnessed repo
harness doctor --json
harness status --json
harness next --json
```

Do **not** start by dumping `docs/HARNESS.md`, `ARCHITECTURE.md`, or
`CONTEXT_RULES.md`. Open those only when the task changes harness or product
rules. Prefer `.agents/skills/harness/SKILL.md` when present.

### Work loop

```bash
harness intake --type <type> --summary "…" --lane tiny|normal|high-risk
harness story add --id US-… --title "…" --lane normal
harness story start US-…                 # also: --id US-…
# implement the slice
git add <slice files> && git commit -m "feat: …"
harness story done US-…                  # or: story update --id … --status implemented
harness next --json
```

`story add` / `update` use `--id`. `story start` / `done` / `block` take a
positional id (`--id` also works).

### Mutation rule (mandatory)

**Do not** create or edit operational durable markdown by hand
(stories / decisions / intakes / backlog / reports).

```bash
harness intake --type … --summary "…" --lane normal
harness story add --id US-… --title "…" --lane normal
harness story update --id US-… --status implemented --unit 1 --integration 1 --e2e 0 --platform 0
harness decision add --id … --title "…" --doc docs/decisions/….md
harness query matrix --json
```

All mutation commands auto-reindex after writing. You do NOT need to call
`harness reindex` manually after mutations.

### Commit after each completed slice (mandatory)

When a small task is actually done (code + tests/docs for that slice, or a
durable write that should travel with the repo):

1. `git add` only the files for that slice.
2. `git commit` with a conventional message (`feat:`, `fix:`, `docs:`, `chore:`).
3. Do **not** wait for the whole epic.
4. Do **not** `git push` unless the user asked.
5. Never commit `.5harness/`, secrets, `node_modules`, or unrelated dirty files.

Skip if there is nothing to commit, or the user forbade commits.

### HARD STOP — harness failure contract (decision 0017)

If the harness **CLI** or **MCP** fails, is missing, or returns a non-zero /
error result for a step you need:

1. **HARD STOP** that durable-write path. Do **not** continue as if it succeeded.
2. **Never** fall back to hand-editing story / decision / intake / backlog
   markdown to “fix” or bypass the failure.
3. **Recover**, then retry the harness command:

| Order | Command | Why |
| --- | --- | --- |
| 1 | `harness --version` | Confirm install / PATH |
| 2 | `harness doctor` or `harness doctor --json` | Workspace health |
| 3 | `harness link` | Register clone / registry pointer |
| 4 | `harness reindex` | Rebuild derived index from markdown |
| 5 | `harness status` / `harness next` | Confirm the project is usable |

**Exit codes:** `0` = success; `1` = usage / validation / operational error
(**stop**, fix, retry); `2` = reserved — treat as non-success unless that
command’s docs say otherwise. Non-zero exit is never success.

### Read with tools (prefer `--json`)

```bash
harness search "…" --json
harness get <id> --json
harness links <id> --json
harness context <id> --json
harness query matrix --json
harness query stats --json
harness query reports --json
```

Classify work with feature intake before large edits. Record durable decisions
when architecture or product rules change.

### MCP (optional)

Prefer the CLI. If MCP is connected, use JSON tools (`harness_next`,
`harness_get`, `harness_query_*`). Discover the project id with
`harness project id` or the `<!-- harness-project-id: … -->` comment.
For an all-projects grant, send `X-Harness-Project: <id>` (preferred) or
`?project=<id>` on every call. Never rely on cwd to select a project.

### Upgrade

When a newer harness CLI version is installed (`npm i -g 5harness`),
run `harness upgrade` to update the harness block in this AGENTS.md.
Only the harness-managed section (markers HARNESS:BEGIN through HARNESS:END)
is modified — all other content is preserved.
<!-- HARNESS:END -->

## Changelog (mandatory)

Always write the changelog. `CHANGELOG.md` is what people read in the panel's
**System → Updates** before they press Update: it lists every release the update
brings in, so they can see what is about to be installed. Treat it as part of the
feature, not as paperwork.

- In the **same pull request** as the change, add a line under `## [Unreleased]`
  in the right section (**Added**, **Changed**, **Fixed**, **Security**,
  **Removed**). One line per thing a person can notice or that changes how
  Rudder behaves: new screens and actions, new or changed settings and
  environment variables, behavior that differs from before, fixes for bugs users
  could hit, security changes, and anything that needs action on upgrade
  (migrations, new required variables, removed options).
- Write for the person running Rudder, in plain words: what they can now do or
  what changed for them, in bold lead-in plus a sentence. Not file names, not
  commit titles, not "refactor". Mention a breaking or manual step explicitly.
- Commit the changelog line with the slice it describes (small commits), so
  `git log -p CHANGELOG.md` follows the feature.
- Pure internal work (tests, CI, refactors without visible effect) needs no line;
  docs-only changes need none either. When unsure, add the line.
- Cutting a release: rename `[Unreleased]` to `[x.y.z] - YYYY-MM-DD`, add a new
  empty `[Unreleased]` above it, bump the workspace version, merge, then tag
  `vx.y.z`. The Release workflow publishes that section as the release notes
  (`deploy/release/changelog.sh`) and **fails when it is empty**; CI fails a
  pull request that changes `crates/`, `web/src/`, `deploy/` or `templates/`
  without touching `CHANGELOG.md`.
- Also record user-visible behavior in the matching guide under `docs/`.

## MCP parity (mandatory)

Every user-facing Rudder feature must be controllable by an authorized AI agent
through the panel's MCP server (`docs/mcp.md`).

When you add or change a panel feature:

1. Expose it on the HTTP API and add the route to `ROUTES` in
   `crates/server/src/openapi.rs`. The route coverage test already requires this.
2. Use an OpenAPI tag that already has an MCP scope (`auth`, `catalog`,
   `agent`, `edge`, `source`, `databases`, `backups`, `stacks`, `cache`,
   `logs`, `monitoring`, `system`), or add the new scope to
   `rudder_core::MCP_SCOPES` and to the permission templates in the same change.
   **Full control** must include the new read and write scopes.
3. Leave the route off the MCP exclusion list unless it cannot be a normal
   request/response tool (browser sign-in, inbound webhooks, WebSockets, public
   downloads, the agent login endpoints themselves).
   `mcp_covers_every_documented_route` fails otherwise.
4. The MCP server publishes one tool per documented operation. A full-control
   grant can call all of them. Narrower grants only call tools in their scopes.

## Cursor Cloud specific instructions

Rust (stable, from `rust-toolchain.toml`), Node 22, and pnpm 10 are already on the image. Each boot starts PostgreSQL 16 and Docker. It does not start the panel.

- The database is `postgres://rudder:rudder@127.0.0.1:5432/rudder`, the same URL as `.env.example`. The `rudder` role has `CREATEDB`, which `sqlx::test` needs. Login shells export `SQLX_OFFLINE=true` so `cargo test` and `cargo clippy` use the committed `.sqlx` cache. Unset it before `cargo sqlx prepare`.
- Docker uses the `fuse-overlayfs` storage driver (`/etc/docker/daemon.json`). This VM has no overlay kernel module. The `ubuntu` user is in the `docker` group. Tests that start containers stay skipped unless `RUDDER_DOCKER_TESTS=1`.
- `/var/lib/rudder` is created on boot and owned by `ubuntu`, so the variables in `.env.example` work as written. Load them, then run `cargo run -p rudder-server` (port 3000, agent listener 7443). `pnpm --dir web build` fills `web/dist`, which the binary embeds. For UI work, `pnpm --dir web dev` serves Vite on port 5173 and proxies `/api` to port 3000.
- A fresh database has no owner. Open `/setup` and create one. The password must be at least 12 characters. The panel then shows the local Docker server as online.
- Checks match CI: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `DATABASE_URL=postgres://rudder:rudder@127.0.0.1:5432/rudder cargo test --workspace`, `pnpm --dir web install --frozen-lockfile`, `pnpm --dir web build`, and `pnpm --dir web check:i18n`.
