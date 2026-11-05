# Changelog

What changes in each Rudder release, written for the people who run it. **System → Updates** in the panel shows the entries of every release it is about to install, so each line should say what a person will notice, not which files moved.

Every change that someone can see or that changes how Rudder behaves gets a line here in the same pull request. When a release is cut, `[Unreleased]` is renamed to the version and date and a new empty `[Unreleased]` is added on top. The release workflow publishes the section of the tag as the release notes and refuses to run when it is empty.

Sections: **Added** (new things), **Changed** (existing behavior), **Fixed**, **Security**, **Removed**. Earlier releases are listed on [GitHub Releases](https://github.com/vantanminh/rudder/releases).

## [Unreleased]

## [0.7.0] - 2026-11-05

### Added
- **Local CI/CD build, R2 push, dual retention, and native self-update in the `rudder` CLI.** Run `rudder init` to create a `rudder.toml` project file, `rudder build` to run your local CI checks and build a compressed Docker image locally, `rudder push` to upload it to Cloudflare R2, `rudder deploy` (or `rudder deploy --build`) to deploy or re-upload local versions, `rudder images` (`list`, `prune`, `rollback`) to manage versions across your machine and R2, and `rudder update` to update the native CLI binary on macOS, Windows, and Linux.
- **Rudder Image store on Cloudflare R2 with automatic retention pruning and deploy policies.** Connect a Cloudflare R2 or S3 destination in **Backups → S3 destinations** as the Rudder Image store, choose per-service whether a pushed image deploys immediately or only sends a notification, and set how many image versions stay on R2 to save storage cost while older versions remain on the developer's machine for rollback or re-upload.
- **Three ways to sign in from the `rudder` CLI, with revocation on the panel.** Sign in with an API key, authorize in your browser (`rudder login --browser`), or enter an 8-character device code (`rudder login --device`) on **Account → API tokens**. Each token shows how it signed in and can be revoked at any time.

## [0.6.1] - 2026-11-04

### Fixed
- **SSH through Cloudflare works on an account with no SSH certificate authority yet.** Setting it up failed with "gateway_ca_not_found (code 12112)"; Rudder now creates the authority when Cloudflare says there is none.

## [0.6.0] - 2026-11-03

### Added
- **Cloudflare Zero Trust, as an optional mode.** **Security → Cloudflare** can create a Cloudflare Tunnel with one to five replicas, publish the panel and every app domain through it (DNS records included), and put the panel behind Cloudflare Access with an email allow list. Nothing changes until you set it up, and every step can be undone. Full guide: `docs/cloudflare.md`.
- **SSH through Cloudflare.** Optional short-lived SSH certificates through Cloudflare Access for Infrastructure, so no SSH keys are kept on the server. The panel shows a script to run as root that makes `sshd` trust Cloudflare; it never edits `sshd_config` itself.
- **Close the public ports from the installer.** `install.sh --close-ports` stops Traefik from listening on 80 and 443 and keeps the panel on `127.0.0.1:3000`, once a tunnel replica is running; the choice survives upgrades. `install.sh --open-ports` undoes it. The panel shows a preflight checklist before you do.
- **AI agents can manage it.** A new `cloudflare` permission scope covers the same operations through the API and MCP. Full control can change them; the Operate template can only read.

### Changed
- **Behind a tunnel, Rudder reads the visitor address from Cloudflare.** Rate limits, Rudder Access, and the audit log use the real visitor instead of Cloudflare's address. This only applies when the tunnel is on and the request comes from a private address.
- **Domains published through the tunnel use Cloudflare's certificate.** Traefik serves them on HTTPS without Let's Encrypt and without an HTTP redirect, because port 80 can be closed.

### Fixed
- **Re-running the installer protects the database and Traefik.** Containers created before they were protected kept a normal OOM score, so the panel showed "partly protected" and the kernel could stop them first. `install.sh` now recreates them (their data is kept). It also checks for the container itself, not a volume of the same name, so a missing database or Traefik container is created again.
- **Re-running the installer no longer starts an old panel.** `install.sh` now pulls the newest `latest` panel image every time. Before, a copy pulled earlier was reused, and a panel updated since then stopped with "migration was previously applied but is missing".

## [0.5.1] - 2026-10-24

### Fixed
- **Release 0.5.0 is superseded.** 0.5.1 has the same changes as 0.5.0 and fixes a flaky test that failed the 0.5.0 build; update to it directly.

## [0.5.0] - 2026-10-24

### Added
- **Rudder Access for internal apps.** Add people by email, group them into departments and ranks, and require sign-in before an app receives a request. Sign-in is Google or a 6-digit email code, and both require Cloudflare Turnstile. The portal is at `/access`, or on a hostname you choose. An app can ask Rudder who the current session belongs to. Other Access administrators cannot remove the original Access administrator.
- **Email for Access codes.** Send codes with Resend, SMTP, or Cloudflare Email Sending. Send a test message from **Rudder Access**.
- **A security page.** **Security** shows certificate expiry and SSL events, checks deployed domains for common exposed files and for plain HTTP or raw-IP access, and applies shared rate limits with counts of allowed and blocked requests.
- **Recovery codes for the panel authenticator.** After you turn on the authenticator, Rudder shows eight single-use codes. Each one signs you in once if you lose the authenticator.

### Changed
- **Apps no longer publish the same host port by default.** Network isolation is on. Two apps can both listen on port 80; each has its own address on the internal network, and a domain routes to the right one. Turn it off on that app only when it must bind a host port. The next deploy applies the change.
- **You can edit a domain after adding it.** Change the hostname, port, SSL mode, or stack service. Routing updates immediately.
- **Extra panel administrators cannot remove the owner.** The first Rudder account stays an administrator. Other administrators can manage the panel, but they cannot delete that account or change its role.

## [0.4.1] - 2026-10-24

### Added
- **Check the panel's SSL certificate.** **Certificates → Panel domain** has a **Check SSL** button. It connects to the saved panel domain over HTTPS, as a browser would, and shows whether the certificate is trusted, whether it covers the domain, who issued it, and how many days are left. AI agents can run the same check.

### Changed
- **The panel domain is also the AI agent sign-in address.** The domain you save under **Certificates → Panel domain** is what Rudder gives to AI agents such as Claude, so you no longer need to set `RUDDER_PUBLIC_URL` to match it.

### Fixed
- **AI agents can connect again behind an HTTPS panel domain.** If Rudder was installed by IP address and the panel later moved to an HTTPS domain, agents such as Claude were told to sign in at the old `http://` address and failed with "Couldn't register". Rudder now advertises the panel domain, or the HTTPS address the agent actually used.

## [0.4.0] - 2026-10-24

### Added
- **Cache effectiveness and a memory ceiling.** **Operate → Cache** shows the hit ratio and memory use over the last 14 days, and lets you limit the cache to a share of the machine (15% by default). Unless you set a higher ceiling, the cache stays at or below 384 MiB. A response larger than that budget is not stored.
- **AI agents can control Rudder.** Add the panel address ending in `/mcp` to an agent. You sign in, pick **Full control** (read and change every setting and all data), **Read everything**, **Operate**, **Watch**, or a custom set of permissions. The agent gets a short-lived access token that refreshes on its own for 90 days. Using a refresh token twice revokes that approval. Revoke an agent from **Admin → AI agents**.
- **Git stacks run on extra servers.** A stack whose compose file is in Git can deploy on a server that runs the agent. Rudder copies the repository's files to that server and leaves files you created there, such as a database directory mounted into the stack, in place. The agent has to be connected. A repository larger than 512 MiB is refused.
- **Cached pages can be kept on disk.** A cache policy has a disk budget. Leave it blank and caching stays in memory, as before. Set it and responses are kept on disk across a cache restart, while the busiest ones stay in memory. The Cache page shows how much disk is in use.
- **A pull request can get its own preview.** Turn on **Preview pull requests** on a service's Git source. Opening, updating, or reopening a pull request against that branch creates a copy of the service with a preview address; closing the pull request removes it. Pull requests from another repository are ignored. A GitHub App connected before this release needs the **Pull request** event turned on in the app's settings on GitHub, or previews will not start.

### Changed
- **Update checks and the install scripts use `vantanminh/rudder`.** This update switches a panel that still has the old default. Rename the GitHub repository to that name before you check for updates or install from the release URL. If you have not renamed it yet, set the repository back to the previous name under **System → Updates** or with `RUDDER_GITHUB_REPO`.
- **The cache container is limited to 512 MiB.** New installs, and the next run of `install.sh` on an existing machine, stop `rudder-cache` at `RUDDER_CACHE_MEMORY` (default `512m`) before it can fill the host. If you raise the ceiling in the panel above that, raise `RUDDER_CACHE_MEMORY` and run the installer again.

### Fixed
- **A cancelled stack is free as soon as its deployment ends.** Cancelling, failing, or finishing a stack action updates the stack and the deployment together, so the stack does not stay on Deploying after the deployment is already over and the next action is accepted.
- **A panel backup restores on PostgreSQL 16.** PostgreSQL 17's restore tool sends `transaction_timeout`, which PostgreSQL 16 rejects. Restoring onto 16 now skips that setting instead of stopping.

### Security
- **The panel sends stricter browser headers, and the cache admin port slows repeated purge and reload calls.** Pages cannot be framed by another site, and a stolen cache admin token cannot flush the cache hundreds of times a second.

## [0.3.1] - 2026-10-16

### Fixed
- **Agent updates reliably start the downloaded binary.** The agent waits until the staged file is fully closed before checking its version, avoiding intermittent Linux "Text file busy" errors.
- **Complete installation and update downloads.** This release includes the installers, signed checksums, binaries and container images missing from the incomplete 0.3.0 release. It also includes the changes listed below; no manual migration is required.

### Added
- **The panel stays reachable when the server is overloaded.** A load guard reads the panel server's memory and CPU every few seconds. While the server is busy, new builds (and push deploys of stacks that build) wait in the queue; when free memory is about to run out, running builds are stopped and the reason is written to their log, so the panel, its database and Traefik keep enough room to answer. The guard is on by default and its thresholds can be changed (`busy` below 15% free memory or above 90% CPU, builds stopped below half the memory reserve). A build held only because of CPU starts anyway after 20 minutes.
- **Load page.** **Operate → Load** shows the panel server's memory, CPU, swap, load and memory pressure with the guard's thresholds drawn on the bars, why builds wait, the build queue, how the panel, its database and Traefik are protected, the guard settings, and what to do if the panel stops answering. A banner on every page says when the server is out of memory, builds are paused, or builds wait for room.
- **Search and jump from anywhere.** Press **Ctrl K** (**⌘K** on a Mac) or **/**, or use the search box in the header, to open any page or service by typing part of its name (Vietnamese works without accents) and to run common actions.
- **Connection status in the header.** A dot shows whether the panel answers normally, slowly, is busy, or is not reachable; pages retry with growing pauses instead of failing at once.
- **Pause, cancel and limit builds.** Admins can pause every new build, cancel all queued and running builds at once, and set how many builds run at the same time on one server (the API is under `/api/v1/load`).

### Changed
- **Redesigned overview.** The overview opens with a health summary that lists each problem with the button that fixes it, shows builds running and waiting next to servers, services and alerts, adds the services using the most CPU or memory with a Stop button, and shows the panel server's load and build controls beside the service list.
- **Apps are killed before the panel when memory runs out.** App, database and stack containers now start with a higher OOM score, and the panel, its database and Traefik get more CPU weight than apps (applied on every panel start). Redeploy an app to give its container the new score. New installs start the panel, Postgres and Traefik with a low OOM score; on an existing install the panel gets it with the next panel update, and Postgres and Traefik when their containers are recreated by `install.sh`.
- **A flood of anonymous requests no longer locks you out.** Requests from signed-in people and API tokens always get through; anonymous requests (webhooks, sign-in pages, scanners) are answered "busy, try again in a few seconds" when 64 are already in progress.
- **The agent on extra servers is protected the same way.** New agent installs run the agent and Traefik with a low OOM score and a higher CPU weight, so the panel can still stop apps on an overloaded server. For an agent installed earlier, add `OOMScoreAdjust=-900` and `CPUWeight=1000` with `systemctl edit rudder-agent`, then restart it.

### Fixed
- **Readable errors when a proxy answers instead of the panel.** When Traefik answers "Bad Gateway" because the panel is restarting or overloaded, pages show that error and keep retrying, instead of a JSON parse error.

## [0.2.0] - 2026-10-12

### Added
- **Watch a stack deploy while it happens.** The Deployments tab shows the output of `docker compose` (image pulls, build steps, container start-up) as it is produced, opens the running deployment's log by itself, follows the newest line, and shows the current step on the row. Before, the log appeared only when the action had finished, so a long build looked frozen.
- **Cancel a running stack action.** A Cancel button stops a deploy, build or pull that is taking too long or went wrong; the deployment ends as *cancelled* and the stack accepts new actions again.
- **Time limit for stack actions.** An action that runs longer than `RUDDER_STACK_TIMEOUT_SECS` (default one hour, `0` for none) is stopped and recorded as failed, instead of blocking the stack forever.
- **Rebuild and deploy, Pull and deploy.** Two new actions: rebuild stacks that build from source (`up --build`), or fetch newer versions of every image first (`up --pull always`). *Build images on every deploy* makes the normal Deploy rebuild too. A plain Deploy still reuses images built earlier; the stack page now says which services build from source.
- **Deploy a Git stack when you push.** *Deploy when I push to the branch* (stack source settings) lets the GitHub App webhook deploy the stack on every push to its branch.
- **Saved registry logins work for stacks.** Logins from the Registries page, such as GitHub's `ghcr.io`, are now used when a stack pulls its images, so private images no longer need `docker login` on the server by hand. A refused pull tells you which registries the stack uses. The Registries page now says so.
- **Connect GitHub with one button.** Git providers has a "Connect with GitHub" card that creates and installs a private GitHub App for you; there is no app id, installation id or private key to copy any more. Pushes to the repositories you pick build the matching services when the panel can be reached from the internet.
- **Import many environment variables at once.** *Import .env* on the Environment tab takes a pasted `.env` block or a file, previews every key as new, changed or unchanged, and saves them all in one step; if any line is wrong nothing is saved.
- **See that a variable was saved correctly.** The Environment list shows when each key was last changed and how many characters its value has, and *Check value* tells you whether a value you type matches the saved one. The saved value is still never shown.
- **Warning when the panel is opened over plain HTTP.** A banner stays on every page (and on the sign-in screen) while the address is `http://` and not this computer, saying that passwords and secrets can be read on the network, with a link to the Certificates page where the panel can get its own domain and certificate. Saving an environment variable, registry, S3 or Git credential over HTTP now asks you to confirm first (once per browser tab).
- **See what a Git stack needs before its first deploy.** *Refresh from Git* reads the compose file from the repository without deploying, so the stack page shows its services, the generated file and any warnings, and the Environment tab lists the variables the file reads that still have no value (picking one fills in its key). Before, you found out only after a failed deploy.
- **Pick the repository, branch and compose file from GitHub.** With a GitHub App provider the Source forms show the repositories the app can read, the branches of the one you chose (default first), and the compose files found on that branch, instead of free text. Saving a source now asks GitHub whether the app can read the repository and the branch, so a typo or a repository outside the installation is refused immediately rather than at the first deploy.
- **Choose a stack's compose project name.** When creating a stack, or on its source form until the first deploy, you can set the compose project name. Using the name of a project already running on the server lets the stack take over its containers and named volumes (databases keep their data) instead of getting a new random name. The guide now also explains how to dump and restore a database or copy a volume.
- **Release notes for everything an update brings in.** System → Updates now lists the notes of every release between the installed version and the newest one, and shows them before you press Update.

### Changed
- **Variables with a fallback are no longer reported as missing.** A stack that reads `${NAME:-default}` only lists `NAME` under *Not set* when the file has no fallback.
- **Long deployment logs are cut in the list.** Each deployment keeps the last 1 MiB of output; the Deployments tab shows the last 200 000 characters with a link to the whole log.

## [0.1.5] - 2026-10-12

### Changed
- **Projects page redesigned** as a workspace: projects on the left, the selected project's environments and services on the right, with search and a phone layout.
- **Service page redesigned** with clearer tabs and details.
