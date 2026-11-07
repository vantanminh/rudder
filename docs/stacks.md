# Compose stacks and templates

A compose stack is a service of kind `compose` that runs one `docker compose` project on a server.

## Creating a stack

- **Paste YAML**: Projects → choose an environment → Service → kind *compose stack*, then paste the file. API: `POST /api/v1/environments/{id}/stacks` with `source: "inline"` and `compose_yaml`.
- **Git**: choose *Git repository*, a Git provider, the repository, branch, and the compose file path inside it. The repository is cloned on the panel into `<data dir>/stacks/<project>/src` and updated with `git fetch` + `git reset --hard` afterwards, so untracked files such as `./data` bind mounts survive. On an extra server the panel then copies the tracked files (not the `.git` directory) to that server. Files already on the server that Git does not track, such as a bind-mounted data directory, are left in place. A checkout larger than 512 MiB is refused. The agent must be connected.
- **Template**: Templates → pick an app → fill in the domain and any options → Deploy.

Set the *main service* to the compose service that serves HTTP. New domains target it unless you choose another compose service in the domain form.

## What Rudder changes in your file

The original file is stored unchanged. The generated file, shown on the stack's General tab and written to `stacks/<project>/rudder-compose.yml`, differs as follows:

- the top-level `name` becomes the project name (`<service name>-<6 hex>`) and `version` is dropped;
- `container_name` values gain the `<project>-` prefix;
- every service gets `rudder.managed`, `rudder.project`, and `rudder.service` labels;
- the main service and every service with a domain join the external `rudder-proxy` network next to their own networks.

Traefik routes are written by the panel's file provider (see decision 0011), not by Docker labels, so you do not need to add Traefik labels. Routed services must not use `network_mode`. Published host ports and `replicas` produce warnings.

### Reading a Git stack before the first deploy

A Git stack's compose file is read from the repository at deploy time, so until then the panel does not know which variables it needs. **Refresh from Git** on the stack page (`POST /api/v1/services/{id}/stack/refresh`) fetches the repository on the panel host, reads the compose file and plans it again, without writing anything to the server or starting a container. Afterwards the General tab shows the services, the generated file and the warnings, and the Environment tab lists the variables the file reads that have no value yet (marked *optional* when the file gives a fallback, *required* for `${NAME:?error}`); picking one fills in its key. Saving or deleting a variable updates that list. The same refresh runs again on every deploy. It is refused while an action is running and for stacks with a pasted-YAML source.

### Project name, and taking over an existing project

Compose prefixes the containers, networks and named volumes of a stack with its **project name** (`<project>-web-1`, `<project>_pgdata`). By default Rudder generates one: the stack name plus six random characters, so two environments can hold stacks with the same name. Set **Compose project name** when creating the stack (`"project"` in `POST /api/v1/environments/{id}/stacks`) or on the stack's source form to choose it yourself: lowercase letters, digits and hyphens, unique among the stacks of that server (`409 project_taken` otherwise). It can be changed only until the first deploy (`409 project_locked` afterwards), because the containers and volumes then carry the name.

To take over something that already runs under another compose project on the same server, find its name and use it:

```bash
docker compose ls                                   # project names
docker volume ls --filter label=com.docker.compose.project=<project>
```

The first deploy then finds the volumes called `<project>_<volume>` and keeps their data. Containers are recreated from the file Rudder generates (compose replaces a container whose configuration changed), and Rudder adds its labels and the proxy network. Keep the same service and volume names as in the old file; a volume that was created by hand or by an old compose version is used as it is, with a warning in the deploy log that compose did not create it.

### Moving a database between projects or servers

When the project name cannot be reused, copy the data instead, with the database stopped or through its own dump tool:

```bash
# logical dump from the old project and restore into the new one (PostgreSQL)
docker compose -p <old-project> exec -T db pg_dump -U <user> -Fc <database> > app.dump
docker compose -p <new-project> exec -T db pg_restore -U <user> -d <database> --clean --if-exists < app.dump

# or copy a whole volume while nothing writes to it
docker run --rm -v <old-project>_pgdata:/from -v <new-project>_pgdata:/to alpine sh -c 'cp -a /from/. /to/'
```

For a database created from the Databases page use its backups (see [backups](backups.md)) and restore into the new service.

## Environment

Reference values as `${NAME}` (or `${NAME:-default}`) in the compose file and set them on the Environment tab. They are stored encrypted and written to a mode 0600 `.env` next to the generated file each time the stack is deployed; values are quoted so compose does not expand `$` inside them. The stack page lists referenced variables that have no value yet; a variable with a fallback (`${NAME:-x}`) is not reported. Use `$$` for a literal dollar sign in the compose file. Container and compose output is masked with these values before it is stored or shown.

### Importing many variables

**Import .env** on the Environment tab takes the text of a `.env` file (pasted or chosen from disk) and saves every key in one request. Lines are `KEY=value`, optionally `export KEY=value`; values may be unquoted, `'single'` or `"double"` quoted, and a quoted value can span several lines; `#` starts a comment. **Preview** shows each key as *new*, *changed* or *unchanged* with its length, and any line that cannot be read; **Apply** saves all of them or, when a line is wrong, none. The *Build secret* box applies to the whole import. API: `POST /api/v1/services/{id}/env/import` with `{ "text", "is_build_secret", "dry_run" }`.

### Checking a saved value

Values stay write-only, but the list shows when each key was last written and how many characters it holds, so a truncated paste is visible. **Check value** takes a typed value and says whether it equals the saved one (`POST /api/v1/services/{id}/env/{key}/verify`, compared in constant time); the saved value is never returned.

## Actions

| Action | Command |
| --- | --- |
| Deploy | `docker compose -p <project> -f rudder-compose.yml up -d --remove-orphans` (with `--build` when *Build images on every deploy* is on) |
| Rebuild and deploy | `… up -d --build --remove-orphans`: rebuilds services that have a `build:` section from the current checkout |
| Pull and deploy | `… up -d --pull always --remove-orphans`: fetches newer versions of every image first |
| Restart | `… restart` |
| Pull images | `… pull` |
| Stop | `… down` |
| Delete | `… down` (or `down --volumes --remove-orphans` when you also delete volumes), then the `.env` is emptied and the service removed |

A plain *Deploy* never builds on its own: compose reuses an image it already has, so a stack built from source keeps running old code until you use *Rebuild and deploy*, switch on *Build images on every deploy*, or let a push do it (below).

### Following and stopping an action

Actions run in the background and appear under Deployments. Only one runs per stack at a time. The output of `docker compose` (image pulls, build steps, container start-up) is written to the deployment while the command runs, so the Deployments tab shows it live, opens the running deployment's log by itself and follows the newest line. Environment values are masked in it. Each deployment keeps the last 1 MiB of its output; the list shows the last 200 000 characters and `GET /api/v1/services/{id}/deployments/{deployment}/logs` returns all of it. Logs for each compose service are on the Logs tab.

**Cancel** (or `POST /api/v1/services/{id}/stack/cancel`) stops the `docker compose` process; the deployment ends as `cancelled`. Containers that had already been replaced stay as they are, and the stack goes back to `running` when it was running before. A stack that was not running before (a first deploy that is cancelled, or one that had been stopped) is `stopped`. The stack status changes in the same moment as the deployment, so it does not stay on Deploying after the action is already over. An action that runs longer than `RUDDER_STACK_TIMEOUT_SECS` (default 3600, `0` for no limit) is stopped the same way and recorded as `failed`. Stacks on remote servers show their output when the agent finishes the command, not line by line, and cancelling them stops waiting rather than killing the remote process. If the panel restarts during an action, the deployment is marked `failed` ("interrupted by process restart").

### Deploy a pushed image (Rudder Image)

`rudder push` also works for a Compose service. The image is stored in the Image Store as for an application, and a deploy loads it onto the stack's server and runs `docker compose up -d`, which recreates the services whose image changed. Name the pushed image in the compose file (by default `rudder-image/<service name>:<tag>`, or the `image_ref` you pass to the push). The service's image policy decides whether a push deploys at once or waits for `rudder deploy`.

### Deploy on push

Turn on *Deploy when I push to the branch* for a Git stack whose provider was connected with **Connect with GitHub**. The provider's webhook then runs a deploy (a rebuild when *Build images on every deploy* is on) for every push to the stack's branch. A push that arrives while another action runs is skipped.

*Preview pull requests* makes a copy of the stack for each pull request opened against that branch in the same repository, deploys the pull request's branch, and gives the copy a preview address. Closing the pull request removes the copy.

### Registry logins

Logins saved under **Registries** apply to the images in a stack too. Before a deploy, pull or rebuild the panel writes a temporary Docker config with the logins of the registries the stack's `image:` entries come from (for example `ghcr.io`), points `DOCKER_CONFIG` at it for that one command and deletes it afterwards. A pull refused for lack of credentials ends with a hint naming the registries the stack uses. Images that a `Dockerfile` pulls with `FROM` are not covered, and agents older than this release ignore the logins: run `docker login` on that server or update the agent.

## Templates

Templates live in [`templates/`](../templates) and are compiled into the binary. Shipped: Ghost, Gitea, Metabase, MinIO (community build `pgsty/minio`), n8n, Plausible, RustFS, Umami, Uptime Kuma, Vaultwarden, WordPress + MariaDB.

A template is YAML:

```yaml
slug: demo              # file must be demo.yml
name: Demo
description: One line.
category: Testing
tags: [example]
website: https://example.com
main_service: web       # receives the domain
port: 80                # container port the domain routes to
notes: Shown after deploy.
variables:
  - name: DOMAIN
    kind: domain        # domain, string, email, secret, number, boolean
  - name: SECRET_KEY
    kind: secret        # generated when empty
    length: 48
compose: |
  services:
    web:
      image: nginx:1.27
      environment:
        URL: https://${DOMAIN}
        KEY: ${SECRET_KEY}
```

Every `${VAR}` must be declared. `cargo test -p rudder-server templates` checks each file, and `RUDDER_DOCKER_TESTS=1 cargo test -p rudder-server --test stacks` also runs `docker compose config` on each. Generated secrets are returned once by the deploy call and shown on screen; afterwards they are only in the encrypted environment.
