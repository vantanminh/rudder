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

1. In the Rudder web panel, open **Backups → S3 destinations**.
2. Under **New destination**, pick the **Cloudflare R2** preset, fill in your R2 endpoint (`https://<account-id>.r2.cloudflarestorage.com`), bucket name, Access Key ID, and Secret Access Key, and click **Add destination**.
3. At the top of **Backups → S3 destinations**, in the **Rudder Image store (Cloudflare R2)** card:
   - Select your Cloudflare R2 bucket as the **Image store bucket**.
   - Set the **Object key prefix** (default `rudder-images`).
   - Optionally set **Keep on R2 (versions per service)** (for example `10`) so older images are automatically deleted from Cloudflare R2 to save storage cost while remaining on the developer's machine.
4. On each application service (**Services → `<app>` → General → Rudder Image**), configure the per-service policy:
   - **Auto-deploy immediately when a new image is pushed**:
     - **On (default)**: `rudder push` immediately loads the image from R2 into Docker on the target server and deploys it.
     - **Off**: `rudder push` stores the image in R2 and emits an `image.pushed` notification without deploying until you run `rudder deploy` (or click **Deploy** in the panel).
   - **Keep on Cloudflare R2 (versions)**: optional per-service override for how many versions stay in R2.

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

# Commands executed locally before building the Docker image (`rudder build`).
checks = [
  "cargo test",
  "pnpm build",
]

# How many recent image versions to keep in Cloudflare R2 (older ones are pruned from R2).
r2_keep_count = 10

# How many recent image versions to keep on this developer machine for local rollback/re-push.
local_keep_count = 20

# When true, `rudder deploy` runs `build -> push -> deploy` automatically.
deploy_build = false
```

### Step 1: Build and check locally (`rudder build`)

```sh
rudder build                        # runs `checks` from rudder.toml, then builds & saves image locally
rudder build --tag v1.4.0           # use an explicit version tag (defaults to timestamp + git SHA)
rudder build --skip-checks          # skip local CI check commands
```

`rudder build` executes every command in `checks` on your machine. If all checks pass, it runs `docker build`, exports and compresses the image into `~/.config/rudder/images/<service>/<tag>.tar.gz` with its SHA-256 checksum, and prunes local archives beyond `local_keep_count`.

### Step 2: Push to Cloudflare R2 (`rudder push`)

```sh
rudder push                         # push the most recently built local image to R2
rudder push v1.4.0                  # push a specific local tag to R2
rudder push --deploy                # force immediate deploy even if service auto-deploy is off
rudder push --no-deploy             # upload to R2 only, even if service auto-deploy is on
rudder push --r2-keep 5             # keep only the 5 newest versions on R2
```

When `rudder push` completes:
- Rudder verifies the SHA-256 digest and stores the archive in Cloudflare R2.
- Rudder prunes older objects in R2 beyond the retention limit (`r2_keep_count`), while keeping the local copy on your computer.
- If the service has **Auto-deploy** enabled (or `--deploy` was passed), Rudder immediately pulls the archive from R2 onto the target server, runs `docker load`, and replaces the container. Otherwise, Rudder records the version and emits a notification.

### Step 3: Deploy (`rudder deploy`)

```sh
rudder deploy                       # deploy the latest R2 image for the service in rudder.toml
rudder deploy shop/prod/web --image v1.4.0
rudder deploy --build               # run full local CI check -> build -> push to R2 -> deploy in one command
```

If you deploy or roll back to an older `<tag>` that was already pruned from Cloudflare R2 to save storage space, `rudder` automatically detects the local archive on your machine, re-uploads it to R2, and deploys it.

### Step 4: Manage local & R2 image versions (`rudder images`)

```sh
rudder images list                  # list versions across local storage and Cloudflare R2
rudder images prune --keep 10       # prune local archives on your machine, keeping the newest 10
rudder images rollback v1.3.9       # roll back to v1.3.9 (from R2, or re-uploading from local if pruned from R2)
```

Example output of `rudder images list`:

```text
TAG                       SHA256        SIZE        STORAGE
v1.4.0                    9f86d081884c  18452910    local + R2
v1.3.9                    4b227777d4dd  18440112    local + R2
v1.3.8                    e3b0c44298fc  18399004    local only
```

---

## 5. Other CLI commands

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
