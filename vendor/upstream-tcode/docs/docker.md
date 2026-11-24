# Tcode on Docker, NAS and homelab hosts

Releases publish `ghcr.io/tryanks/tcode-headless` for Linux `amd64` and
`arm64`. The image contains the release's headless binary with its browser app
embedded, Node.js and the system tools agents use; no agent CLI is in it. The
CLIs `TCODE_AGENTS` selects install into the data volume when the container
starts (see [Choose agents](#choose-agents)). Tags are `<version>`, without
the leading `v`, and `latest`; a prerelease has its version tag only and does
not move `latest`.

Each published image carries a build provenance attestation, and each
architecture an SPDX SBOM, signed by this repository's release workflow:

```sh
gh attestation verify oci://ghcr.io/tryanks/tcode-headless:latest --repo Tryanks/tcode
```

## Start a machine

Download [docker-compose.yml](../docker-compose.yml) into a directory on the
NAS. Create a private `.env` beside it:

```dotenv
TCODE_PASSWORD=replace-with-a-long-unique-password
PUID=1000
PGID=1000
TCODE_PROJECTS=/srv/projects
TZ=Europe/Berlin
TCODE_NAME=NAS
# TCODE_VERSION=0.3.0
# TCODE_AGENTS=codex,pi
```

Use a non-root project owner's `id -u` and `id -g` as `PUID` and `PGID`. Prepare the
host project directory with write access for those IDs; startup fixes ownership
of its own data volume, never `/projects`. Then run:

```sh
chmod 600 .env
docker compose up -d
docker compose logs -f tcode
```

`TCODE_NAME` is the name devices show for this machine; `TZ` sets the
time zone of agents and terminal panes. Startup prints the machine id and invitation link/QR. Scan or paste the
invitation in a phone or desktop Tcode client. Open `http://<nas-ip>:47420/`
for the browser app and sign in with `TCODE_PASSWORD`. The invitation expires;
use the browser's **Settings → Remote → New invitation** for another device,
or `docker compose exec --user tcode tcode tcode-headless pair` to print the
current invitation. See [remote access](remote.md) for the pairing lifecycle.

The template publishes TCP 47420 for the password-protected browser listener
and UDP 47420 for native clients. Linux `network_mode: host`, replacing the
`ports` section, also carries LAN multicast/mDNS and is preferable for local
discovery. Under bridge networking, Traverse works normally; if a device cannot
find the direct LAN path, enter the NAS's IP address on the device. Allow inbound
UDP 47420 in the host firewall. HTTP is for trusted networks; use an HTTPS
reverse proxy for browser access over an untrusted network.

The image's health check requests `/auth/state` on the browser listener at
`127.0.0.1:47420`, so NAS dashboards and `docker compose ps` show whether the
machine is serving. It allows ten minutes for a first start's installs. If `command`
moves the listener to another port, override `healthcheck` in Compose to match.
Stopping waits up to 30 seconds (`stop_grace_period`) while running agent
sessions are flushed.

To pair a phone once and reach both the NAS and your desktop, create a group
on the NAS in the browser's **Settings → Remote → Machine group** and join it
from the desktop with an invitation from the NAS; see
[Machine groups](remote.md#machine-groups).

To use a self-hosted [Traverse](../crates/traverse-server/README.md) instance,
add `--traverse https://traverse.example.org` to `command`; `--traverse off`
keeps the machine to the LAN and addresses typed on a device.

## Data and projects

| Container path | Purpose |
| --- | --- |
| `/data/tcode` | Tcode settings, threads, `traverse.json`, browser password and tokens |
| `/data/home` | The runtime user's `HOME`: all agent credential and configuration directories |
| `/data/agents` | The global npm prefix per CPU architecture (`<arch>/npm`: npm agent CLIs and every other global npm install), links to Cursor's and Grok's current builds in `bin`, the npm cache |
| `/data/bin` | User-supplied executables, including ACP agent commands, on `PATH` |
| `/projects` | Mounted project directories; use these paths when adding projects in Tcode |

Keep and back up the **whole `/data` volume**. Losing `traverse.json` loses the
machine key and invalidates every native pairing. Losing the home directory
loses CLI logins. `docker compose down` retains the named volume;
`docker compose down -v` deletes it. Stop Tcode before taking a consistent
backup. Never run two hosts against the same volume.

Nothing an agent needs across restarts is outside `/data`: every CLI keeps its
login, configuration and history under `/data/home`, and none uses a keyring
on Linux, so the container needs no D-Bus. `/tmp` is not persisted; Claude
Code keeps its scratchpad and copies of pasted images in `/tmp/claude-<uid>`
and OpenCode keeps temporary files in `/tmp/opencode`, both lost on restart.
The [research notes](research/agent-persistence.md) cite each CLI's paths.

The entrypoint initializes the volume, then runs both CLI installation and
Tcode as `PUID:PGID` (1000:1000 by default). Agent processes and terminal panes
inherit those IDs. Changing the IDs on an existing volume migrates its
ownership. Do not set `user:` in Compose: it bypasses that initialization.
Use `docker compose exec --user tcode tcode bash` for a maintenance shell.

## Choose agents

`TCODE_AGENTS` lists the native agent CLIs the machine has, comma separated
from `claude,codex,pi,opencode,cursor,grok`. Unset means all six; an
explicitly empty value means none; to leave one or two out, list the others.
Unknown names stop startup. The selection is reapplied to the native provider
switches at every start: omitted native profiles are disabled, and their
configuration and credentials are retained. ACP configuration is retained.

At start, each selected CLI that is not yet installed is installed in its
current release: the npm packages globally, into the image's npm prefix
`/data/agents/<cpu architecture>/npm`, Cursor with its own installer under
`/data/home`. Later starts reuse them offline. A CLI that cannot be installed
(no network on its first start, say) is reported in the log, the machine
starts without it, and the next start tries again. Only selected CLIs are on
`PATH`: an npm CLI left out of the selection is uninstalled (its login and
configuration stay in `/data/home`, and selecting it again reinstalls it), and
Cursor's commands are linked from `/data/agents/bin` only while it is
selected. The [agent manifest](../crates/headless/container/agents.json) names
each CLI's package or installer and its commands.

Updates follow each CLI. Claude Code, Cursor and Grok update themselves in the
background, into the copy the machine runs. Codex, pi and OpenCode update when
asked: from Tcode's provider settings, which update an npm install in place,
or with `codex`, `pi update` or `opencode upgrade` in a terminal pane. Keep
the image's npm: the Claude Code, OpenCode and Grok updates rely on install
scripts, which npm 12 skips by default, so an `npm install --global
npm@latest` would leave those updates incomplete.

For ACP, install the agent's command into `/data/bin`, mount an executable
there, or run `npm install --global <package>` as `tcode`; global npm
installs, including those an agent makes itself, persist in the npm prefix and
are on `PATH`. Node, npm, npx and the system `PATH` remain available even with
an empty native selection. Install any additional language runtime in a
derived image. Add the command through Tcode's existing ACP configuration.

## Login from a Tcode terminal pane

Open a terminal pane in a project after connecting to the machine. Its shell
runs inside the container with the same home and selected commands as the
agents. Open displayed URLs on your own computer or phone. Browser callbacks
to `localhost` refer to that browser's computer, not the container; publishing
47420 does not forward an agent's OAuth callback port.

| Agent | In-container method | API-key alternative | Persistent location under `/data/home` |
| --- | --- | --- | --- |
| Claude Code | `claude auth login`: open the printed URL externally and paste the authorization code when offered. | `ANTHROPIC_API_KEY` | `.claude` (login in `.claude/.credentials.json`), `.claude.json`, `.config/anthropic` (Console sign-in) |
| Codex | `codex login --device-auth`: open the verification URL and enter the code. Enable device-code login in account/workspace security settings first. Default `codex login` uses a localhost callback and needs forwarding or a local login copied into the container. | Set `OPENAI_API_KEY`, then `printenv OPENAI_API_KEY \| codex login --with-api-key` | `.codex/auth.json`, `.codex/.credentials.json` (MCP sign-ins) |
| pi | Run `pi`, then `/login`. Use a device code or paste the final redirect URL/authorization code when prompted. A provider without a paste fallback needs callback forwarding or an API key. | The selected provider's key, e.g. `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` | `.pi/agent/auth.json` |
| OpenCode | `opencode auth login`: paste a provider API key, or use GitHub Copilot's displayed device code. OAuth methods requiring a localhost callback need forwarding or a provider key. | The selected provider's key, e.g. `OPENAI_API_KEY` | `.local/share/opencode/auth.json`, `.local/share/opencode/mcp-auth.json` (MCP sign-ins) |
| Cursor | `NO_OPEN_BROWSER=1 cursor-agent login`: open the printed Cursor link externally while the CLI waits for browser authentication. | `CURSOR_API_KEY` for unattended operation | `.config/cursor/auth.json` (login), `.cursor` (configuration, chats, project data) |
| Grok | `grok login --device-auth`: follow the printed device-code flow. Default browser OAuth may need a localhost callback. | `XAI_API_KEY` | `.grok/auth.json`, `.grok/mcp_credentials.json` (MCP sign-ins) |

Sources: [Claude authentication](https://code.claude.com/docs/en/authentication),
[Codex headless authentication](https://developers.openai.com/codex/auth),
[pi providers](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/providers.md),
[OpenCode providers](https://opencode.ai/docs/providers/),
[Cursor authentication](https://cursor.com/docs/cli/reference/authentication).
Provider login flows can change; the image pins CLI versions, not account policies.

Enable the relevant environment rows in Compose for API-key authentication,
then recreate the service. An `export` in one terminal affects that shell and
its children; it does not update the already-running Tcode host. Alternatively
configure the key in Tcode's provider settings.

The automated smoke check verifies CLI execution, password login, native LAN
pairing/reconnection, UID/GID ownership and persistence of the shared agent
home and browser login token. It does **not** complete third-party account logins. Those flows
still need an account-backed manual check on a Linux host; do not treat file
persistence checks as evidence of successful OAuth or API authentication.

## Optional Docker socket

Uncomment the socket mount and `DOCKER_GID` environment row in Compose to let agents create
**sibling containers**. This grants **host-root-equivalent access**. Enable it
only for agents and users you trust. It is not Docker-in-Docker.

The base image does not include a Docker CLI. For example, build a derived
image using the official static CLI (pin the image tags for your deployment):

```dockerfile
FROM docker:cli AS docker-cli
FROM ghcr.io/tryanks/tcode-headless:latest
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker
```

Set `DOCKER_GID` to `stat -c %g /var/run/docker.sock` on the host. The entrypoint
adds that supplementary group, including group 0 when explicitly selected,
and also preserves other non-root groups supplied with Docker's `group_add`.
Bind mounts requested by sibling containers refer to **host paths**, so use
the host's `/srv/projects/...` path when constructing their mounts.

## NAS platforms

Every platform below runs the same image; only where the Compose file, `.env`
and the data live differs. Use a host path that the platform backs up for
`/data`, or keep the named volume and back it up with Docker.

### Synology DSM (Container Manager)

1. In **File Station**, create `docker/tcode` for the project and a shared
   folder for projects, e.g. `/volume1/projects`.
2. Put `docker-compose.yml` and `.env` in `docker/tcode`. Set
   `TCODE_PROJECTS=/volume1/projects`, and `PUID`/`PGID` to the DSM user
   that owns that folder: `id <user>` over SSH (DSM users usually start at
   uid 1026 in group `users`, gid 100).
3. **Container Manager → Project → Create**, choose that folder as the path;
   it uses the existing `docker-compose.yml`. Build and start the project.
4. The invitation is in the container's **Log** tab. Allow TCP and UDP 47420
   in **Control Panel → Security → Firewall** if the firewall is enabled.

### Unraid

The [Unraid template](../crates/headless/container/unraid.xml) maps `/data` to
`/mnt/user/appdata/tcode`, `/projects` to `/mnt/user/projects`, and runs as
Unraid's usual `99:100` (`nobody:users`). Save it as
`/boot/config/plugins/dockerMan/templates-user/my-tcode.xml`, then **Docker →
Add Container → Template: tcode**, set the password and apply. The container's
log shows the invitation; **WebUI** opens the browser app. With the Compose
Manager plugin, the Compose file above works as it is.

### TrueNAS SCALE

TrueNAS SCALE 24.10 and later run Compose files under **Apps → Discover Apps
→ ⋮ → Install via YAML**. That dialog reads no `.env`, so replace each
`${...}` in `docker-compose.yml` with its value, mount datasets for `/data`
and `/projects` instead of the named volume, and set `PUID`/`PGID` to the
dataset owner (the built-in `apps` user is 568:568).

### Other hosts

Any Linux host with Docker Engine 24 or later and Compose v2 runs the template
as it is. Without Compose, the equivalent command is:

```sh
docker run -d --name tcode --restart unless-stopped --stop-timeout 30 \
  -e PUID=1000 -e PGID=1000 -e TZ=UTC -e TCODE_PASSWORD=... \
  -p 47420:47420/tcp -p 47420:47420/udp \
  -v tcode-data:/data -v /srv/projects:/projects \
  ghcr.io/tryanks/tcode-headless:latest serve --name NAS --browser-listen 0.0.0.0:47420
```

## Self-host Traverse

[`crates/traverse-server/docker-compose.yml`](../crates/traverse-server/docker-compose.yml)
runs the published `tcode-traverse` image on a public host with a domain:
write `traverse.toml` beside it as its header describes, then
`docker compose up -d`. Point machines at it with `--traverse`, as above. The
[Traverse server documentation](../crates/traverse-server/README.md) covers TLS
modes, ports and limits.

## Upgrade or build locally

Back up `/data`, change `TCODE_VERSION` if pinned, then run `docker compose
pull && docker compose up -d`. Upgrading the image does not reinstall the
agent CLIs: they live in the volume (see [Choose agents](#choose-agents) for
how each updates). To reinstall an npm CLI, leave it out of `TCODE_AGENTS` for
one start and add it back. To reinstall Cursor, stop the service, delete
`/data/home/.local/share/cursor-agent` and the `cursor`, `cursor-agent` and
`agent` links in `/data/home/.local/bin`, and start again.

To build the image from a checkout instead, run from the repository root:

```sh
docker build -f crates/headless/Dockerfile -t tcode-headless:local .
```

This compiles the browser app and the headless binary inside Docker, so the
host needs nothing but Docker; Cargo's registry and target directory persist
in BuildKit cache mounts between builds. Then set `TCODE_IMAGE=tcode-headless`
and `TCODE_VERSION=local` in `.env` and run `docker compose up -d`.
`docker build -f crates/traverse-server/Dockerfile -t tcode-traverse .` builds
Traverse the same way.

The release workflow builds each architecture natively from the same
`--features web` binaries as the archives (`--build-arg BINARY_SOURCE=release`,
with the binary staged as `dist/<amd64|arm64>/tcode-headless`), tests the
images, and publishes multi-architecture manifests to GHCR with their
attestations. It also publishes `ghcr.io/tryanks/tcode-traverse`.

For maintainers enabling publishing for the first time: make both GHCR packages
public in their package settings after their first upload, and retain this
repository's Actions access. GHCR creates new packages as private; repository
linking grants workflow access but does not make anonymous pulls available.
See [GitHub's registry setup](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry#pushing-container-images).

CI builds the browser bundle on every push, runs Clippy on the
`tcode-headless --features web` binary the image runs, and checks the container
scripts and Compose files. Release publishing runs the
[smoke check](../crates/headless/container/smoke.mjs) on native amd64 and arm64
runners against images built from the release binaries before pushing either
architecture.
