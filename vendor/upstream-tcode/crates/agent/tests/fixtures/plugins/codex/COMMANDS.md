# Codex plugin probe: commands and state

Recorded 2026-10-03 against `codex-cli 0.159.3` (`codex --version`; npm
package `@openai/codex` installed through mise at
`~/.local/share/mise/installs/node/26.8.1/lib/node_modules/@openai/codex`),
macOS arm64. Findings that cite these files: `tmp/probe-codex-plugins.md`.

## Isolation

- `CODEX_HOME=/tmp/tcode-probe/codex-home` for every recorded run. It started
  empty. No `auth.json` was copied: no request needed authentication (see the
  findings, question 6).
- `HOME=/tmp/tcode-probe/codex-fakehome` (empty) for every recorded run.
  `CODEX_HOME` does not isolate `$HOME/.agents/skills`: an exploratory run
  without this override listed the user's own skills
  (`/Users/<user>/.agents/skills/*`, scope `user`) in `skills/list`.
- Project directory: `/tmp/tcode-probe/codex-proj` (a git repo with one
  commit), not `/tmp/tcode-probe/proj`. A concurrent Claude-plugin probe uses
  `/tmp/tcode-probe/proj` and wiped it in the middle of an exploratory run, so
  this probe moved to `codex-`-prefixed directories:
  `codex-proj`, `codex-elsewhere` (empty), `codex-nongit` (copy of
  `codex-proj` without `.git`, marketplace renamed `tcode-probe-nongit`),
  `codex-fakehome`.
- The no-network run used a separate fresh home
  `/tmp/tcode-probe/codex-home-nonet` so the startup sync started from
  nothing.
- `~/.codex` was never read or written.

## app-server invocation

Every server is `codex app-server` (argv exactly that, nothing else) with the
environment above plus `RUST_LOG=info` (stderr only; stdout frames are not
affected), stdin/stdout piped, and the process cwd stated per run. Framing is
the one in `crates/agent/src/codex.rs`: one JSON object per line, no
`"jsonrpc"` member, `initialize` with id 1 and

```json
{"clientInfo":{"name":"tcode","title":"Tcode","version":"0.1.0"},"capabilities":{"experimentalApi":true}}
```

then the `{"method":"initialized"}` notification. Requests are sent one at a
time; each waits for its response. Teardown is closing stdin; every server
exited 0.

The driver and scenario scripts stay outside the repository in
`/tmp/tcode-probe/` (`driver.py`, `scenario_main.py`, `scenario_cwd.py`,
`scenario_forms.py`, `scenario_nonet.py`, `scenario_reconcile.py`,
`race2.py`, `race3.py`, `package.py`); stderr logs are in
`/tmp/tcode-probe/logs/`.

## File formats

- Per-method files (`marketplace-add.jsonl` … `marketplace-remove.jsonl`):
  every line is a literal frame of server A in the main run, request then
  response. Only the frames the Codex adapter's tests read are kept; the
  id → step mapping below marks the others "(not kept)". The observer,
  notification, cwd-discovery, manifest-form, no-network and
  external-reconcile runs are described for provenance; their recordings,
  the CLI outputs, the generated schemas and the authored marketplaces are
  not kept.
- `config-read.jsonl`: recorded later (2026-10-03, same binary) with the same
  driver and framing against a fresh `CODEX_HOME=/tmp/tcode-plugins-live/rec-home`,
  process cwd `/tmp`: `marketplace/add {"source":"/tmp/tcode-probe/codex-mkt"}`
  (id 2, not kept), then `config/read {}` (id 3). Its `marketplaces` table is
  the one the main run's `config.toml` snapshot shows after id 2.

## Marketplace layout

`/tmp/tcode-probe/codex-mkt` (marketplace `tcode-probe-mkt`, added by local
path):

```
.agents/plugins/marketplace.json      name tcode-probe-mkt; alpha AVAILABLE/ON_INSTALL, beta AVAILABLE/ON_USE
plugins/alpha/.codex-plugin/plugin.json   name, version 0.1.0, description, keywords, skills "./skills/", mcpServers "./mcp.json", interface
plugins/alpha/mcp.json                 mcpServers.alpha-docs: streamable-http http://127.0.0.1:9/mcp (nothing listens)
plugins/alpha/hooks/hooks.json         SessionStart command "echo tcode-probe-alpha-hook" (found by default discovery)
plugins/alpha/skills/alpha-hello/SKILL.md
plugins/beta/.codex-plugin/plugin.json    name, version 0.2.0, description, skills, interface
plugins/beta/skills/beta-hello/SKILL.md
```

alpha uses the `.codex-plugin/plugin.json` form because 0.159.3 loads plugin
hooks only from that form (manifest-form run). The first draft of alpha
used a root portable `plugin.json`; its hook never appeared.

`/tmp/tcode-probe/codex-proj` (repo marketplace `tcode-probe-repo`):
`.agents/plugins/marketplace.json` with one
plugin `gamma` (`.codex-plugin/plugin.json`, one skill).

Manifest-form matrix (`/tmp/tcode-probe/codex-formtest`, `codex-hooktest`,
`codex-mcptest`; the recording is not kept):

| plugin | layout |
|---|---|
| rootonly | root `plugin.json` without `$schema` |
| compatonly | `.codex-plugin/plugin.json` only |
| both | root `plugin.json` without `$schema` + `.codex-plugin/plugin.json` |
| none | no manifest, `skills/` only |
| rootschema | root `plugin.json` with Agent Plugins `$schema` |
| hcompatfield | compat manifest, `"hooks": "./hooks/hooks.json"` |
| hcompatdefault | compat manifest, no `hooks` field, `hooks/hooks.json` present |
| hcompatinline | compat manifest, inline `hooks` object |
| hrootdefault | root `$schema` manifest, `hooks/hooks.json` present |
| hrootext | root `$schema` manifest, `extensions."com.openai".hooks` path |
| mcompatmcpjson | compat manifest, portable `mcp.json`, no `mcpServers` field |
| mcompatdotmcp | compat manifest, `"mcpServers": "./.mcp.json"` |
| mcompatptr | compat manifest, `"mcpServers": "./mcp.json"` (portable file) |
| aboth | root `$schema` manifest + compat overlay, `hooks/hooks.json`, `mcp.json` |
| aboth2 | as aboth, overlay with explicit `"hooks": "./hooks/hooks.json"` |
| aroothookext | root `$schema` manifest with `extensions."com.openai"` hooks + interface, no overlay |

## Main run (`scenario_main.py`)

Process cwd `/tmp/tcode-probe/codex-proj` for all five servers.

- **A**: the management server; all per-method files.
- **B1-passive**: starts after A initialized; `initialize` only.
- **B2-reader**: after every A mutation, sends `plugin/installed {}`,
  `skills/list {cwds:[proj]}` and `hooks/list {cwds:[proj]}` itself.
- **B3-session**: `thread/start {"cwd":proj,"approvalPolicy":"untrusted","sandbox":"read-only"}`
  (the shape `codex.rs` sends), then idle.
- **B4-session-after-install**: same `thread/start`, started after A's
  install, then idle.

The observer frames were recorded but are not kept. B1, B3 and B4 start only
after A has initialized the home: starting several servers at the same moment
on a fresh `CODEX_HOME` makes some exit before replying with
`Error: failed to initialize sqlite state runtime under <home>` (12 of 36
starts in `race2.py`, 3 servers × 12 fresh homes; 0 of 36 on an already
initialized home in `race3.py`).

Server A, in order (`t` = seconds since start):

| id | request | file |
|---|---|---|
| 1 | `initialize`, then `initialized` | (not kept) |
| 2 | `marketplace/add {"source":"/tmp/tcode-probe/codex-mkt"}` | marketplace-add.jsonl |
| 3 | `plugin/list {}` | (not kept) |
| 4 | `plugin/list {"cwds":[proj]}` | plugin-list-with-cwds.jsonl |
| 5 | `plugin/read {"pluginName":"alpha","marketplacePath":<path from id 4>}` | plugin-read.jsonl |
| 6 | `skills/list {"cwds":[proj]}` before install | (not kept) |
| 7 | `hooks/list {"cwds":[proj]}` before install | (not kept) |
| 8 | `plugin/install {"pluginName":"alpha","marketplacePath":…}` | plugin-install.jsonl |
| 9 | `plugin/installed {}` | (not kept) |
| 10 | `plugin/installed {"cwds":[proj]}` | plugin-installed.jsonl |
| 11 | `skills/list` (enabled) | skills-list.jsonl |
| 12 | `hooks/list` (enabled) | hooks-list.jsonl |
| — | CLI `codex plugin list --json`, `… --json --available`, `codex plugin marketplace list --json` | (not kept) |
| 13 | `config/value/write {"keyPath":"plugins.alpha@tcode-probe-mkt.enabled","value":false,"mergeStrategy":"upsert"}` | config-value-write-enabled.jsonl |
| 14 | `plugin/installed {}` (disabled) | plugin-installed-after-disable.jsonl |
| 15 | `plugin/reconcile {"reason":…}` | (not kept) |
| 16 | `skills/list` (disabled) | skills-list.jsonl |
| 17 | `skills/list {"cwds":[proj],"forceReload":true}` (disabled) | (not kept) |
| 18 | `hooks/list` (disabled) | hooks-list.jsonl |
| — | CLI `codex plugin list --json` (disabled) | (not kept) |
| 19 | `config/value/write … "value":true …` | (not kept) |
| 20 | `plugin/installed {}` (re-enabled) | (not kept) |
| 21 | `skills/list` (re-enabled) | (not kept) |
| 22 | `hooks/list` (re-enabled) | (not kept) |
| 23 | `plugin/uninstall {"pluginId":"alpha@tcode-probe-mkt"}` | plugin-uninstall.jsonl |
| 24 | `plugin/reconcile` (after uninstall) | (not kept) |
| 25 | `plugin/installed {}` (after uninstall) | plugin-installed.jsonl |
| 26 | `skills/list` (after uninstall) | (not kept) |
| 27 | `hooks/list` (after uninstall) | (not kept) |
| 28 | `marketplace/remove {"marketplaceName":"tcode-probe-mkt"}` | marketplace-remove.jsonl |
| 29 | `plugin/list {"cwds":[proj]}` (after remove; curated catalog has synced by now) | (not kept) |

After the lifecycle, the watcher control wrote
`$HOME/.agents/skills/probe-control/SKILL.md`, then
`<proj>/.agents/skills/probe-repo-control/SKILL.md`, then removed both, with a
4 s quiet window after each step (recording not kept).

CLI commands (cwd `codex-proj`, same environment, no `RUST_LOG`):

```
codex plugin list --json
codex plugin list --json --available
codex plugin marketplace list --json
codex plugin list --json              # after id 13
```

## Other runs

- `scenario_cwd.py` (cwd discovery). Server `D-cwd-proj` (cwd
  `codex-proj`): `plugin/list` with `{}`, `cwds:[proj]`, `cwds:[proj/plugins]`
  and `cwds:[codex-nongit]`; `skills/list {}` and `hooks/list {}`;
  `plugin/install gamma` from the repo marketplace; `plugin/installed` with
  `{}` and with `cwds:[proj]`; `skills/list` with `cwds:[codex-elsewhere]` and
  with `cwds:[proj]`. Server `D-cwd-elsewhere` (cwd `codex-elsewhere`):
  `plugin/list` and `plugin/installed`, each with `{}` and with `cwds:[proj]`;
  `skills/list` with `{}` and with `cwds:[proj]`;
  `plugin/uninstall gamma@tcode-probe-repo`; `plugin/installed cwds:[proj]`.
- `scenario_forms.py` (manifest forms). cwd `codex-proj`; for each of
  the three test marketplaces: `marketplace/add`, `plugin/read` for every
  plugin, then `marketplace/remove`.
- `scenario_nonet.py` (no network). `sandbox-exec -f nonet.sb codex
  app-server`, where `nonet.sb` is
  `(version 1) (allow default) (deny network-outbound (remote ip))`, with
  `CODEX_HOME=/tmp/tcode-probe/codex-home-nonet` (fresh), cwd `codex-proj`:
  the whole local lifecycle, plus `marketplace/add {"source":"openai/plugins"}`
  and `plugin/list {"forceRefetch":true}`.
- `scenario_reconcile.py` (external reconcile). Server
  `C-reconciler`: `marketplace/add`, then `plugin/reconcile` as a baseline,
  then again after each change made outside it: CLI
  `codex plugin add alpha@tcode-probe-mkt` (stdout
  ``Added plugin `alpha` from marketplace `tcode-probe-mkt`.`` and
  `Installed plugin root: /private/tmp/tcode-probe/codex-home/plugins/cache/tcode-probe-mkt/alpha/0.1.0`),
  a second server `D-writer` sending `config/value/write enabled=false`, and
  CLI `codex plugin remove alpha@tcode-probe-mkt` (stdout
  ``Removed plugin `alpha` from marketplace `tcode-probe-mkt`.``). Ends with
  `marketplace/remove`.

## `CODEX_HOME` state after each mutating step (main run)

Besides `config.toml` and `plugins/`, the first server start created the
state databases (`state_5.sqlite`, `logs_2.sqlite`, …), `installation_id`,
`skills/.system/` (five bundled system skills) and `.tmp/plugins/`. That last
directory is a checkout of the curated marketplace (`plugins.sha`
`5fd93af4cd0c623e020d0cc7e9ce178b4ac1f70f`); it shows up as marketplace
`openai-api-curated` once the background sync finishes, as in id 29 but not
yet at id 3.

#### before any request

`config.toml`:

```toml
<absent>
```

`plugins/`:

```
<absent>
```

#### after marketplace/add

`config.toml`:

```toml
[marketplaces.tcode-probe-mkt]
source_type = "local"
source = "/private/tmp/tcode-probe/codex-mkt"
```

`plugins/`:

```
<absent>
```

#### after plugin/install alpha

`config.toml`:

```toml
[marketplaces.tcode-probe-mkt]
source_type = "local"
source = "/private/tmp/tcode-probe/codex-mkt"

[plugins."alpha@tcode-probe-mkt"]
enabled = true
```

`plugins/`:

```
$CODEX_HOME/plugins
$CODEX_HOME/plugins/cache
$CODEX_HOME/plugins/cache/tcode-probe-mkt
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/hooks
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/hooks/hooks.json
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/skills
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/skills/alpha-hello
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/skills/alpha-hello/SKILL.md
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/.codex-plugin
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/.codex-plugin/plugin.json
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/mcp.json
```

#### after config/value/write enabled=false

`config.toml`:

```toml
[marketplaces.tcode-probe-mkt]
source_type = "local"
source = "/private/tmp/tcode-probe/codex-mkt"

[plugins."alpha@tcode-probe-mkt"]
enabled = false
```

`plugins/`:

```
$CODEX_HOME/plugins
$CODEX_HOME/plugins/cache
$CODEX_HOME/plugins/cache/tcode-probe-mkt
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/hooks
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/hooks/hooks.json
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/skills
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/skills/alpha-hello
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/skills/alpha-hello/SKILL.md
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/.codex-plugin
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/.codex-plugin/plugin.json
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/mcp.json
```

#### after plugin/reconcile (after disable)

`config.toml`:

```toml
[marketplaces.tcode-probe-mkt]
source_type = "local"
source = "/private/tmp/tcode-probe/codex-mkt"

[plugins."alpha@tcode-probe-mkt"]
enabled = false
```

`plugins/`:

```
$CODEX_HOME/plugins
$CODEX_HOME/plugins/cache
$CODEX_HOME/plugins/cache/tcode-probe-mkt
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/hooks
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/hooks/hooks.json
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/skills
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/skills/alpha-hello
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/skills/alpha-hello/SKILL.md
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/.codex-plugin
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/.codex-plugin/plugin.json
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/mcp.json
```

#### after config/value/write enabled=true

`config.toml`:

```toml
[marketplaces.tcode-probe-mkt]
source_type = "local"
source = "/private/tmp/tcode-probe/codex-mkt"

[plugins."alpha@tcode-probe-mkt"]
enabled = true
```

`plugins/`:

```
$CODEX_HOME/plugins
$CODEX_HOME/plugins/cache
$CODEX_HOME/plugins/cache/tcode-probe-mkt
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/hooks
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/hooks/hooks.json
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/skills
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/skills/alpha-hello
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/skills/alpha-hello/SKILL.md
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/.codex-plugin
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/.codex-plugin/plugin.json
$CODEX_HOME/plugins/cache/tcode-probe-mkt/alpha/0.1.0/mcp.json
```

#### after plugin/uninstall alpha

`config.toml`:

```toml
[marketplaces.tcode-probe-mkt]
source_type = "local"
source = "/private/tmp/tcode-probe/codex-mkt"
```

`plugins/`:

```
$CODEX_HOME/plugins
$CODEX_HOME/plugins/cache
$CODEX_HOME/plugins/cache/tcode-probe-mkt
```

#### after plugin/reconcile (after uninstall)

`config.toml`:

```toml
[marketplaces.tcode-probe-mkt]
source_type = "local"
source = "/private/tmp/tcode-probe/codex-mkt"
```

`plugins/`:

```
$CODEX_HOME/plugins
$CODEX_HOME/plugins/cache
$CODEX_HOME/plugins/cache/tcode-probe-mkt
```

#### after marketplace/remove

`config.toml`:

```toml
(empty file)
```

`plugins/`:

```
$CODEX_HOME/plugins
$CODEX_HOME/plugins/cache
$CODEX_HOME/plugins/cache/tcode-probe-mkt
```
