# Command line client (`rudder`) & Rudder Image

`rudder` is a native, dependency-free Rust CLI distributed as single-file binaries for **macOS** (`x86_64` and Apple Silicon `aarch64`), **Windows** (`x86_64`), and **Linux** (`x86_64` and `aarch64` static `musl`).

In addition to managing services, logs, and cache purges, `rudder` provides the **Rudder Image** local CI/CD workflow: run tests and build Docker images directly on a developer's machine (or AI agent workstation), upload compressed image archives to **Cloudflare R2** through Rudder, automatically prune old versions from R2 to save storage cost while keeping local archives for instant rollback, and deploy on demand or automatically.

---

## 1. Install `rudder` natively

### macOS & Linux

```sh
curl -fsSL https://github.com/vantanminh/rudder/releases/latest/download/install-cli.sh | sh
```

Options for `install-cli.sh`:
- `--version v0.6.0` — install a specific release tag instead of `latest`.
- `--bin-dir ~/.local/bin` — customize the target directory (defaults to `/usr/local/bin` when writable or `~/.local/bin`).

### Windows (PowerShell)

```powershell
irm https://github.com/vantanminh/rudder/releases/latest/download/install-cli.ps1 | iex
```

The installer verifies `SHA256SUMS`, places `rudder.exe` in `$HOME\.rudder\bin`, and adds it to your user `PATH`.

### Native self-update

Update `rudder` in place to the latest GitHub release at any time:

```sh
rudder update --check   # check if a newer version is available on GitHub
rudder update           # download, verify SHA-256 (and minisign when set), and replace binary in place
```

---

## 2. Sign in (`rudder login`) — 3 authentication methods

`rudder login` supports three ways to authenticate against your Rudder panel. Every CLI login appears on **Account → API tokens** with its method badge (`API key`, `Browser login`, or `Device code`) and can be revoked with **Revoke** at any time.

### Method 1: API key (`RUDDER_TOKEN` or stdin)

Create a token on **Account → API tokens** and pass it via `RUDDER_TOKEN` or stdin (never as a command-line flag, so it stays out of shell history):

```sh
rudder login --url https://panel.example.com
# API token: rdr_...
```

### Method 2: Browser authorization (`--browser`)

Starts a temporary loopback listener on `127.0.0.1`, opens your browser to the Rudder panel, and receives the scoped token automatically once you click **Approve CLI login**:

```sh
rudder login --url https://panel.example.com --browser
```

### Method 3: 8-character Device Code (`--device`)

Ideal for remote SSH sessions, containers, or headless environments. `rudder` prints a verification URL and an **8-character alphanumeric code** (containing both letters and digits):

```sh
rudder login --url https://panel.example.com --device
# Open this URL in your browser to authorize rudder CLI:
#   https://panel.example.com/tokens?device=1
#
# Enter 8-character device code: K7M4P9X2
# Waiting for approval...
```

Open **Account → API tokens** in the Rudder dashboard, enter the 8-character code under **Authorize CLI with device code**, and click **Authorize CLI**. The CLI receives its token automatically and saves it to `~/.config/rudder/config.json` (mode `0600`).

`rudder logout` removes the local configuration file.

---

## 3. Configure Cloudflare R2 for Rudder Image

1. In the Rudder web panel, open **S3 → S3 destinations**.
2. Under **New destination**, pick the **Cloudflare R2** preset, fill in your R2 endpoint (`https://<account-id>.r2.cloudflarestorage.com`), bucket name, Access Key ID, and Secret Access Key, and click **Add destination**.
3. Open **S3 → Rudder Image → Setup** and, in the **Image store** card:
   - Select your Cloudflare R2 bucket as the **Bucket**.
   - Set the **Object key prefix** (default `rudder-images`).
   - Optionally set **Keep on R2 (versions per service)** and **Keep on R2 (days)** so older images are automatically deleted from Cloudflare R2 to save storage cost while remaining on the developer's machine.
4. On each application or Compose service (**Services → `<app>` → General → Rudder Image**), configure the per-service policy:
   - **Auto-deploy immediately when a new image is pushed**:
     - **On (default)**: `rudder push` immediately loads the image from R2 into Docker on the target server and deploys it.
     - **Off**: `rudder push` stores the image in R2 and emits an `image.pushed` notification without deploying until you run `rudder deploy` (or click **Deploy** in the panel).
   - **Keep on Cloudflare R2 (versions)**: optional per-service override for how many versions stay in R2.

### The Rudder Image page

**S3 → Rudder Image** is the place to watch it all:

- **Overview**: a status checklist (bucket, retention, signed-in CLI clients, first push, deploy-on-push), a live **Check bucket** test that writes, reads, and deletes a probe object, push, build, and image-deploy counts for the last 30 days with a daily chart, usage per service, the latest pushes, and every command-line client signed in with `--browser` or `--device`.
- **Images**: every version of a service, whether it is still on the bucket, with **Deploy**, **Free bucket space** (delete from the bucket, keep the history), and **Delete**.
- **Setup**: the bucket, prefix, and retention, plus the commands to connect the CLI.

The **Overview** page of the panel also shows a small Rudder Image card with the same status. The same data is available to AI agents through `GET /api/v1/image-store/overview` and `POST /api/v1/image-store/test` (MCP scope `source`).

---

## 4. Local CI/CD workflow (`rudder init`, `build`, `push`, `deploy`, `images`)

Instead of spending GitHub Actions minutes or overloading your production VPS with `docker build`, you or your AI coding agent can run CI checks and build the image locally, then push the artifact to Cloudflare R2.

### Initialize `rudder.toml`

Run `rudder init` in your repository root:

```sh
rudder init shop/prod/web
```

This creates a `rudder.toml` configuration file:

```toml
# Rudder project configuration (`rudder build`, `rudder push`, `rudder deploy`)
service = "shop/prod/web"
dockerfile = "Dockerfile"
context = "."
platform = "linux/amd64"

# Commands executed locally before building the Docker image (`rudder build`).
checks = [
  "cargo test",
  "pnpm build",
]

# Extra Docker --build-arg values (`GIT_SHA` and `BUILD_TIME` are injected automatically).
build_args = { APP_ENV = "production" }

# How many recent image versions to keep in Cloudflare R2 (older ones are pruned from R2).
r2_keep_count = 10

# How many recent image versions to keep on this developer machine for local rollback/re-push.
local_keep_count = 20

# When true, `rudder deploy` runs `build -> push -> deploy` automatically.
deploy_build = false
```

### Step 1: Build and check locally (`rudder build`)

```sh
rudder build                                  # runs `checks` from rudder.toml, then builds & saves image locally
rudder build --tag v1.4.0                     # use an explicit version tag (defaults to YYYYMMDD-HHMMSS-<short-sha>)
rudder build --platform linux/amd64           # target your server's architecture (e.g. from Apple Silicon or Windows)
rudder build --no-cache --progress=plain      # force a clean BuildKit build with plain log output
rudder build --build-arg FEATURE_FLAG=1       # pass additional --build-arg KEY=VAL flags (repeatable)
rudder build --skip-checks                    # skip local CI check commands
```

What `rudder build` does:
- Runs every command in `checks` on your machine (unless `--skip-checks` is passed).
- Measures the Docker build context (respecting `<dockerfile>.dockerignore` or `.dockerignore`) and prints a warning if the context exceeds **200 MiB**.
- Enables **BuildKit** (`DOCKER_BUILDKIT=1`) so `# syntax=docker/dockerfile:1.7` and `RUN --mount=type=cache` always work.
- Automatically passes `--build-arg GIT_SHA=<commit-or-nogit>` and `--build-arg BUILD_TIME=<RFC3339-UTC>` (plus any `build_args` from `rudder.toml` or `--build-arg`). Outside a Git repository, it prints a notice and falls back to `GIT_SHA=nogit` and a timestamp tag.
- Exports and compresses the image into `~/.config/rudder/images/<service>/<tag>.tar.gz` with its SHA-256 checksum, prints a summary (Image ID, Platform, Git SHA, Size, Digest, Duration, and Next step), and prunes local archives beyond `local_keep_count` (never pruning the currently deployed image).

### Step 2: Push to Cloudflare R2 (`rudder push`)

```sh
rudder push                         # push the most recently built local image to R2
rudder push v1.4.0                  # push a specific local tag to R2
rudder push --deploy                # force immediate deploy even if service auto-deploy is off
rudder push --no-deploy             # upload to R2 only, even if service auto-deploy is on
rudder push --r2-keep 5             # keep only the 5 newest versions on R2
```

When `rudder push` runs:
- If the exact `tag` + `sha256` is already present in Cloudflare R2, `rudder push` skips re-uploading the archive (and proceeds directly to deployment when `--deploy` is passed).
- Otherwise, `rudder push` streams the archive with upload progress and automatic retry on transient network or `502`/`503`/`504` errors.
- Rudder verifies the `expected_sha256` digest on the panel before registering the version in the database (deleting the R2 object if the digest mismatches, so a failed push never leaves a half-registered image version).
- Rudder prunes older objects in R2 beyond `r2_keep_count` while protecting both the newest image and the image currently deployed on the service.
- If your panel is protected by **Cloudflare Access**, either add a Bypass policy for `/api/v1/*` (Rudder still requires `Authorization: Bearer rdr_...` on every API endpoint) or export `CF_ACCESS_CLIENT_ID` and `CF_ACCESS_CLIENT_SECRET` from a Cloudflare Access Service Token.

---

## 5. Migrate from GitHub Actions / GHCR to local build

Moving a service from "build on GitHub Actions → push to `ghcr.io` → server pulls" to "build locally with `rudder build` → push to Cloudflare R2 with `rudder push` → deploy from Rudder" takes four steps:

1. **Connect Cloudflare R2 once in the panel**:
   - Add your R2 bucket under **S3 → S3 destinations**, then select it under **S3 → Rudder Image → Setup**.
2. **Sign in from your workstation**:
   ```sh
   rudder login --url https://panel.example.com --device
   rudder status --check
   ```
3. **Create `rudder.toml` in your repository root**:
   ```toml
   service = "my-project/staging/api"
   dockerfile = "Dockerfile"
   context = "."
   platform = "linux/amd64"
   checks = ["cargo test --workspace"]
   deploy_build = false
   r2_keep_count = 10
   local_keep_count = 20
   ```
   - Make sure your `.dockerignore` (or `<Dockerfile>.dockerignore`) excludes large local directories such as `target/`, `node_modules/`, and `.git/` so `rudder build` sends a small context to BuildKit.
   - If your `Dockerfile` declares `ARG GIT_SHA` and `ARG BUILD_TIME`, `rudder build` populates both automatically on every build.
4. **Build, push, and deploy**:
   ```sh
   rudder build                  # builds locally with BuildKit + layer cache
   rudder push --deploy          # uploads to R2, verifies SHA-256, and deploys to staging
   ```
   Once verified on staging, you can disable or remove the `.github/workflows/*.yml` image build job and GHCR registry credentials on the server.

---

## 6. Other CLI commands

### Choosing a service

Commands take a service as its id, its name, `project/name`, or `project/environment/name` (or read `service` from `rudder.toml`). `rudder services` prints every service:

```text
ID                                    NAME            KIND         STATUS
6c1f…                                 shop/prod/web   application  running
91aa…                                 shop/prod/blog  compose      running
```

### Logs

```sh
rudder logs shop/web                       # last 100 lines, oldest first
rudder logs shop/web -n 500 --since 2h
rudder logs shop/web --level error,warn --source app --grep timeout
rudder logs shop/web -f                    # keep following
```

### Purge the cache

```sh
rudder cache purge shop/web --all
rudder cache purge shop/web --path /index.html --path /app.css
rudder cache purge shop/web --prefix /blog/ [--domain <domain id>]
```

### Decrypt a backup

```sh
RUDDER_BACKUP_PASSPHRASE=... rudder backup decrypt --in db.dump.zst.enc --out db.dump.zst
```

---

## 7. Skills for AI agents

`rudder skills` installs a skill, a short guide that AI coding agents load on demand, so they know what Rudder is and how to use it: the `rudder` CLI, the MCP server, the permission model, and when to ask you before doing something destructive. The text is built into the CLI, so it always matches the version you installed.

```sh
rudder skills install                       # every agent found in your home directory
rudder skills install --agent claude        # one agent
rudder skills install --project             # into this repository, so the whole team gets it
rudder skills list                          # what is installed and whether it is current
rudder skills remove
rudder skills print                         # the skill text, to put anywhere else
```

| `--agent` | Your home directory | With `--project` |
| --- | --- | --- |
| `claude` | `~/.claude/skills/rudder/SKILL.md` | `.claude/skills/rudder/SKILL.md` |
| `codex` | `$CODEX_HOME` or `~/.codex`, then `skills/rudder/SKILL.md` | `.agents/skills/rudder/SKILL.md` |
| `agents` | `~/.agents/skills/rudder/SKILL.md` | `.agents/skills/rudder/SKILL.md` |
| `cursor` | not available | `.cursor/rules/rudder.mdc` |

`--dir DIR` writes `DIR/rudder/SKILL.md` for any other agent that reads skill folders. Without `--agent`, the agents whose folder exists are used, and the command says so when it finds none.

Files written by `rudder skills` carry a marker. Running `install` again updates them after you upgrade the CLI. A file with the same name that `rudder` did not write is left alone unless you pass `--force`, and `remove` only deletes files it wrote. `--dry-run` shows what would change. Restart the agent afterwards so it loads the skill.

### Documentation from your panel

The skill sends agents to your panel for documentation, so they read the version you run:

```sh
rudder docs search deploy an image          # ranked sections with snippets
rudder docs read cli                        # a whole guide
rudder docs list
```

Agents connected over MCP use the same content through `get_v1_docs_search` and `get_v1_docs__name_` (see [Search the documentation](mcp.md#search-the-documentation)).
