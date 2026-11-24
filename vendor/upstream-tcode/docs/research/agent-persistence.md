# Where the six native agent CLIs persist data on Linux

Date: 2026-10-24

Question: for each native CLI the Tcode container runs, where do credentials,
config, history, caches and self-updates land on Linux? Which variables move
them? Does everything survive restarts and image upgrades when only `/data`
is a volume (`HOME=/data/home`, no D-Bus, no keyring, non-root `PUID:PGID`)?

## Versions inspected

| Agent | Artifact inspected | Source revision |
| --- | --- | --- |
| Claude Code | `@anthropic-ai/claude-code@2.1.310` and `@anthropic-ai/claude-code-linux-x64@2.1.310` (Bun-compiled binary, `GIT_SHA 0d2056ec…`, built 2026-10-23); docs at code.claude.com fetched 2026-10-24 | closed source; binary strings only |
| Codex | `@openai/codex@0.164.0` and `@openai/codex@0.164.0-linux-x64` | `openai/codex` tag `rust-v0.164.0` |
| pi | `@earendil-works/pi-coding-agent@1.4.0` (unbundled `dist/` and `docs/` ship in the tarball) | `earendil-works/pi` (npm metadata `repository`) |
| OpenCode | `opencode-ai@1.19.4` and `opencode-linux-x64@1.19.4` | `anomalyco/opencode` tag `v1.19.4` (`sst/opencode` redirects there) |
| Cursor | `https://cursor.com/install` as served on 2026-10-24 (pins `2026.10.22-27dcf5f`) and its `linux/x64/agent-cli-package.tar.gz` | closed source; bundled JS only |
| Grok | `@xai-official/grok@1.0.70` and `@xai-official/grok-linux-x64@1.0.70` (binary shipped brotli-compressed; decompressed for reading) | closed source (`xai-org-shared/grok-build` is not public); binary strings only |
| npm | `npm@11.19.1`, the npm bundled with Node 24.22.1 (latest `node:24` as of 2026-10-20) | `npm@11.19.1:docs/content/…` |

Downloaded packages were read and grepped only; nothing from them was
executed or installed.

Citation forms used below: a URL; `pkg@version:path` for a file in an npm
tarball; for a compiled binary, `pkg@version:path` plus the literal string
quoted.

## The deployment being checked

From [`crates/headless/Dockerfile`](../../crates/headless/Dockerfile),
[`entrypoint.sh`](../../crates/headless/container/entrypoint.sh),
[`install-agents.mjs`](../../crates/headless/container/install-agents.mjs) and
[`profile.sh`](../../crates/headless/container/profile.sh):

- `HOME=/data/home`, `TCODE_DATA_DIR=/data/tcode`,
  `NPM_CONFIG_PREFIX=/data/agents/custom`, `NPM_CONFIG_CACHE=/data/agents/npm-cache`;
  `PATH=/data/agents/bin:/data/bin:/data/agents/custom/bin:$PATH`. `TMPDIR`
  and all `XDG_*` are unset, so `os.tmpdir()` is `/tmp` and the XDG defaults
  are `~/.config`, `~/.local/share`, `~/.local/state` and `~/.cache`, all
  under `/data/home`.
- Each npm CLI is installed with `npm install --global --prefix
  /data/agents/<arch>/<agent> --allow-scripts=<pkg> -- <pkg>@latest` and run
  through `/data/agents/bin/<cmd>`, a `sh` wrapper that exports
  `NPM_CONFIG_PREFIX=/data/agents/<arch>/<agent>` and `exec`s
  `<prefix>/bin/<cmd>`.
- Cursor is installed by its own installer under `$HOME`;
  `/data/agents/bin/{cursor-agent,agent}` are symlinks to
  `$HOME/.local/bin/{cursor-agent,agent}`. `$HOME/.local/bin` is not on `PATH`.
- Tcode drives the CLIs non-interactively: `codex app-server`
  (`crates/agent/src/codex.rs:1219`), `pi --mode rpc` (`crates/agent/src/pi.rs:53`),
  `opencode serve` (`crates/agent/src/opencode.rs:1493`), `cursor-agent acp`
  (`crates/agent/src/cursor.rs:227`), `grok agent stdio`
  (`crates/agent/src/grok.rs:416-429`). Terminal panes run the interactive
  TUIs through the same wrappers.

---

## Claude Code (`@anthropic-ai/claude-code@2.1.310`)

### 1. Credentials

- On Linux the `/login` credential is the file `~/.claude/.credentials.json`,
  mode `0600`. Only macOS uses a keychain.
  [Authentication → credential management](https://code.claude.com/docs/en/authentication#credential-management)
- In the binary, the storage path is `join(configDir, ".credentials.json")`. The
  only Linux keyring reference is `secret-tool`, and it appears in a list of
  credential-helper commands that the permission/sandbox code blocks. It is
  not a storage backend: `case"linux":case"wsl":return["secret-tool"]`
  sits next to the `security`/`cmdkey` deny rules.
  `@anthropic-ai/claude-code-linux-x64@2.1.310:claude`
- Headless with no keyring: the file store needs no D-Bus, so nothing
  changes. Verified from the docs and the binary. Not run.
- A Console sign-in that creates no API key is stored as an Anthropic profile
  in the Anthropic configuration directory, `$ANTHROPIC_CONFIG_DIR` or else
  `~/.config/anthropic`. Non-secret `configs/<profile>.json` and secret
  `credentials/<profile>.json` (mode `0600`) live there.
  [Authentication → Anthropic profiles](https://code.claude.com/docs/en/authentication#anthropic-profiles-and-federation-credentials),
  [WIF reference → configuration directory](https://platform.claude.com/docs/en/manage-claude/wif-reference#configuration-directory)
- `claude setup-token` saves nothing; the token goes in `CLAUDE_CODE_OAUTH_TOKEN`.
  [Authentication → long-lived token](https://code.claude.com/docs/en/authentication#generate-a-long-lived-token)

### 2. Config

- User settings: `~/.claude/settings.json`. Global config, account details,
  personal MCP servers and per-project trust: `~/.claude.json`, which sits
  outside `~/.claude` unless `CLAUDE_CONFIG_DIR` is set.
  [Settings](https://code.claude.com/docs/en/settings),
  [Dev containers → persist authentication](https://code.claude.com/docs/en/devcontainer)
  ("Without `CLAUDE_CONFIG_DIR`, `~/.claude.json` stays in the home directory").
- Managed settings are read from a system path such as
  `/etc/claude-code/managed-settings.json`. They are read-only, and the image
  ships none. [Managed settings](https://code.claude.com/docs/en/managed-settings)

### 3. History, transcripts, logs

All of these are under `~/.claude/`. Transcripts are
`projects/<project>/<session>.jsonl`, with subagents, tool results and MCP tasks
beside them. The rest are `file-history/`, `plans/`, `debug/`, `paste-cache/`,
`session-env/`, `tasks/`, `shell-snapshots/` and `backups/` (copies of
`~/.claude.json`). Prompt history is `history.jsonl`, and `sessions/` holds
running-session markers. `jobs/` and `daemon/` hold background-session state.
[.claude directory → application data](https://code.claude.com/docs/en/claude-directory#application-data)

### 4. Caches and other state

- `~/.claude/stats-cache.json`, `remote-settings.json`, `policy-limits.json`,
  `cache/changelog.md`, `plugins/` and `usage-data/`. `todos/`, `statsig/`
  and `logs/` are legacy directories that current versions no longer write.
  [.claude directory → application data](https://code.claude.com/docs/en/claude-directory#application-data)
- Telemetry `user.id` is kept in `~/.claude.json`.
  [Monitoring](https://code.claude.com/docs/en/monitoring-usage)
- **Outside HOME:** Claude Code's temp tree is `$CLAUDE_CODE_TMPDIR`, else
  `os.tmpdir()`, plus `/claude-<uid>/`, so it is `/tmp/claude-<uid>/` here. It holds
  the per-session scratchpad
  (`/tmp/claude-<uid>/<project>/<session>/scratchpad/`). Since v2.1.275 it
  also holds pasted and attached images (`images/` per session), which older
  versions kept in `~/.claude/image-cache/`. These are lost on every
  container restart.
  [.claude directory → scratchpad](https://code.claude.com/docs/en/claude-directory#session-scratchpad-directory),
  [env vars → `CLAUDE_CODE_TMPDIR`](https://code.claude.com/docs/en/env-vars)

### 5. Relocation variables

- `CLAUDE_CONFIG_DIR` replaces `~/.claude` for settings, history, plugins and
  `.credentials.json`. It also moves `.claude.json` inside it.
  [env vars](https://code.claude.com/docs/en/env-vars),
  [Authentication](https://code.claude.com/docs/en/authentication#credential-management),
  [Troubleshoot install → directory permissions](https://code.claude.com/docs/en/troubleshoot-install#check-directory-permissions)
- `CLAUDE_CODE_TMPDIR` moves the temp tree. `CLAUDE_CODE_PLUGIN_CACHE_DIR`
  moves `plugins/`. `CLAUDE_CODE_DEBUG_LOGS_DIR` is a file path.
  `ANTHROPIC_CONFIG_DIR` moves Anthropic profiles. [env vars](https://code.claude.com/docs/en/env-vars)
- `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME` matter only to the
  native installer (`~/.local/share/claude`, `~/.local/state/claude`,
  `~/.cache/claude`).
  [Troubleshoot install](https://code.claude.com/docs/en/troubleshoot-install#check-directory-permissions)
- Paths that ignore these variables: none found that matter. Tcode's own
  per-provider home override sets `HOME` for Claude
  (`crates/agent/src/lib.rs:806-813`), which moves both `~/.claude` and
  `~/.claude.json`.

### 6. Install and self-update

- The npm package's `postinstall` (`install.cjs`) hard-links or copies the
  platform binary over `bin/claude.exe`.
  `@anthropic-ai/claude-code@2.1.310:install.cjs`.
- Install-method detection: a binary path containing
  `/node_modules/@anthropic-ai/` is `npm-global`. Here that path is
  `/data/agents/<arch>/claude/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe`.
  Quoted from the binary: `if(n.includes("/node_modules/@anthropic-ai/"))return"npm-global"`.
- The npm updater spawns `npm` from `PATH` with
  `["install","-g","@anthropic-ai/claude-code@<version>"]` and inherits the
  environment: `let w=await ct(s,["install","-g",u],{cwd:L(),…})`. It checks
  writability against `npm -g config get prefix`. Under the wrapper's
  `NPM_CONFIG_PREFIX` that resolves to the agent's own prefix, so **the update
  lands where it runs from**. `@anthropic-ai/claude-code-linux-x64@2.1.310:claude`
- Updates run in the background "on startup and periodically while running"
  and take effect at the next start. They cover npm installs too (an npm
  auto-update "runs the same in-place upgrade").
  [Setup → auto-updates](https://code.claude.com/docs/en/setup#auto-updates),
  [Setup → shared storage](https://code.claude.com/docs/en/setup)
- No automatic npm-to-native migration was found. `claude doctor` only
  suggests ``Consider using native installation: `claude install` ``. If a
  user runs `claude install`, it writes `~/.local/bin/claude` and
  `~/.local/share/claude/versions/`. Both persist, but `~/.local/bin` is not
  on `PATH` and the `/data/agents/bin/claude` wrapper keeps running the npm
  copy. `@anthropic-ai/claude-code-linux-x64@2.1.310:claude`,
  [Setup](https://code.claude.com/docs/en/setup#auto-updates)
- Disable with `DISABLE_AUTOUPDATER=1`, or `DISABLE_UPDATES=1` to also block
  `claude update` and `claude install`.
  [env vars](https://code.claude.com/docs/en/env-vars)
- The update relies on npm running `postinstall`. npm 11.19.1 (Node 24) still
  runs unreviewed install scripts and prints a notice
  (`npm@11.19.1:docs/content/commands/npm-approve-scripts.md`: "this field is
  advisory: install scripts still run by default"). npm 12 blocks them by
  default (`npm@12.2.1:docs/content/commands/npm-approve-scripts.md`:
  "Dependency install scripts are blocked by default"). Under npm 12 the
  self-update would leave the placeholder stub, which prints
  `claude native binary not installed`.
  [Troubleshoot install](https://code.claude.com/docs/en/troubleshoot-install#native-binary-not-found-after-npm-install)
- Unverified: whether the background updater also runs in `-p`/SDK sessions
  like the ones Tcode starts. Settled by running an old version under Tcode
  and checking `claude doctor`'s last update result.

### 7. API-key variables

`ANTHROPIC_API_KEY` (an interactive session asks once to approve it; `-p`
always uses it), `ANTHROPIC_AUTH_TOKEN` (Bearer), `CLAUDE_CODE_OAUTH_TOKEN`,
`apiKeyHelper`, `ANTHROPIC_PROFILE`, and the `CLAUDE_CODE_USE_BEDROCK`,
`_VERTEX` and `_FOUNDRY` cloud selectors.
[Authentication → precedence](https://code.claude.com/docs/en/authentication#authentication-precedence)

---

## Codex (`@openai/codex@0.164.0`, `rust-v0.164.0`)

The `developers.openai.com/codex/*` docs now redirect to `learn.chatgpt.com/docs/*`.

### 1. Credentials

- `cli_auth_credentials_store` defaults to `File`, which persists
  `$CODEX_HOME/auth.json`. `Keyring` fails when no keyring is available.
  `Auto` falls back to the file. `Ephemeral` keeps the credential in memory.
  [types.rs#L133-L146](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/config/src/types.rs#L133-L146),
  [storage.rs#L162-L163](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/login/src/auth/storage.rs#L162-L163),
  [Auth → credential storage](https://learn.chatgpt.com/docs/auth#credential-storage)
- MCP OAuth tokens (`mcp_oauth_credentials_store`) default to `Auto`: keyring
  first, falling back to `$CODEX_HOME/.credentials.json`.
  [types.rs#L148-L163](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/config/src/types.rs#L148-L163),
  [resolved_store.rs#L343-L392](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/rmcp-client/src/oauth/resolved_store.rs#L343-L392)
- The keyring backend on Linux is `keyring` 3.6.3 with
  `linux-native-async-persistent`
  ([keyring-store/Cargo.toml](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/keyring-store/Cargo.toml)).
  That store writes the kernel keyutils cache and then the Secret Service over
  D-Bus. If the Secret Service write fails, it reverts the keyutils entry and
  returns the error
  ([keyring-rs v3.6.3 keyutils_persistent.rs#L59-L79](https://github.com/hwchen/keyring-rs/blob/v3.6.3/src/keyutils_persistent.rs#L59-L79)).
  With no D-Bus, every keyring write fails, `Auto` falls back to the file, and
  nothing ends up only in the non-persistent kernel keyring. This is read from
  the source, not run.

### 2. Config

User config is `$CODEX_HOME/config.toml`, plus `$CODEX_HOME/<profile>.config.toml`
for profiles. The system layer `/etc/codex/config.toml` is read-only.
Variables in `$CODEX_HOME/.env` are loaded at startup.
[Config basics](https://learn.chatgpt.com/docs/config-file/config-basic),
[arg0/src/lib.rs#L272-L280](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/arg0/src/lib.rs#L272-L280)

### 3. History, transcripts, logs

Session rollouts are `$CODEX_HOME/sessions/` and `archived_sessions/`
([rollout/src/lib.rs#L86-L87](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/rollout/src/lib.rs#L86-L87)),
with `session_index.jsonl` and `history.jsonl` beside them. SQLite state lives
under `CODEX_SQLITE_HOME`, which defaults to `CODEX_HOME`. Logs go to
`$CODEX_HOME/log`, and the plaintext `codex-tui.log` is opt-in.
[Environment variables](https://learn.chatgpt.com/docs/config-file/environment-variables),
[Config reference → `log_dir`](https://learn.chatgpt.com/docs/config-file/config-reference)

### 4. Caches and other state

`$CODEX_HOME/shell_snapshots/`
([core/src/shell_snapshot.rs#L108](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/core/src/shell_snapshot.rs#L108)),
the update-check cache `$CODEX_HOME/version.json`
([tui/src/updates_cache.rs#L18-L22](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/tui/src/updates_cache.rs#L18-L22)),
and helper links in `$CODEX_HOME/tmp/arg0-linux/`. Codex refuses to put that
directory under the system temp dir
([arg0/src/lib.rs#L314-L344](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/arg0/src/lib.rs#L314-L344)).
Nothing persistent goes to `/tmp`.

### 5. Relocation variables

`CODEX_HOME` covers config, auth, logs, sessions and skills; it must already
exist. `CODEX_SQLITE_HOME` (or the `sqlite_home` setting) moves SQLite state.
Without `CODEX_HOME` the default is `dirs::home_dir()/.codex`. XDG variables
are not consulted.
[home-dir/src/lib.rs#L13-L60](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/utils/home-dir/src/lib.rs#L13-L60),
[Environment variables](https://learn.chatgpt.com/docs/config-file/environment-variables)

### 6. Install and self-update

- The npm launcher `bin/codex.js` sets `CODEX_MANAGED_BY_NPM=1` and
  `CODEX_MANAGED_PACKAGE_ROOT`. `@openai/codex@0.164.0:bin/codex.js` lines 223-239.
  The binary maps that marker to `InstallMethod::Npm`
  ([install-context/src/lib.rs#L127](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/install-context/src/lib.rs#L127)),
  and the update action is `npm install -g @openai/codex`
  ([tui/src/update_action.rs#L50](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/tui/src/update_action.rs#L50)).
- The update is offered only by the interactive TUI's "Update now" prompt
  (`tui/src/update_prompt.rs`), and only after the user confirms. It is gated
  by `check_for_update_on_startup`
  ([tui/src/updates.rs#L27-L58](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/tui/src/updates.rs#L27-L58)).
  `codex app-server`, which Tcode uses, never self-updates.
- When a user accepts it in a terminal pane, `npm install -g` runs with the
  wrapper's `NPM_CONFIG_PREFIX` and replaces the copy it runs from. The npm
  package has no install scripts (`@openai/codex@0.164.0:package.json`), so
  the npm script policy does not affect it.

### 7. API-key variables

- `CODEX_API_KEY` is honoured only where the docs list it: exec, review, the
  TypeScript SDK and remote exec-server. In code it takes effect only where
  `enable_codex_api_key_env` is true
  ([login/src/auth/manager.rs#L1537-L1543](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/login/src/auth/manager.rs#L1537-L1543)).
- `CODEX_ACCESS_TOKEN` is accepted for trusted automation.
- `OPENAI_API_KEY` only pre-fills the TUI's API-key login field
  ([tui/src/onboarding/auth.rs#L852](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/tui/src/onboarding/auth.rs#L852)).
  The persistent route is `printenv OPENAI_API_KEY | codex login --with-api-key`,
  which writes `auth.json`.
  [Auth](https://learn.chatgpt.com/docs/auth),
  [Environment variables](https://learn.chatgpt.com/docs/config-file/environment-variables)
- Custom providers read whichever variable their `env_key` names.

---

## pi (`@earendil-works/pi-coding-agent@1.4.0`)

### 1. Credentials

API keys and OAuth tokens go to `<agent-dir>/auth.json`, where `<agent-dir>`
is `~/.pi/agent`. A key may be written as `!command` so that it is resolved
from a secret manager instead of stored.
`@earendil-works/pi-coding-agent@1.4.0:docs/configuration.md`,
`…:docs/providers.md`, `…:dist/config.js` (`getAuthPath`). No keyring code was
found: there are no `keytar`, `keychain`, `keyring` or `libsecret` references
in `dist/` of `pi-coding-agent`, `pi-ai` or `pi-agent-core`. Headless use
needs nothing extra.

### 2. Config

`<agent-dir>/settings.json`, `keybindings.json`, `mcp.json`, `models.json`,
`SYSTEM.md`/`APPEND_SYSTEM.md`, and the `extensions/`, `skills/`, `prompts/`
and `themes/` directories. Project trust is `~/.pi/agent/trust.json`.
`…:docs/configuration.md`, `…:docs/security.md`

### 3. History, transcripts, logs

Sessions are `<agent-dir>/sessions/`, grouped by working directory. The debug
log is `<agent-dir>/pi-debug.log`. `…:docs/sessions.md`,
`…:dist/config.js` (`getSessionsDir`, `getDebugLogPath`)

### 4. Caches and other state

- Managed `fd` and `rg` binaries go to `<agent-dir>/bin`. Pi packages are
  installed under `<agent-dir>/npm/` and `<agent-dir>/git/`.
  `…:dist/config.js` (`getBinDir`), `…:dist/core/package-manager.js`
- `os.tmpdir()` holds only transient files: the external editor buffer,
  `/share` exports, tool output spill files and WSL clipboard files.
  `…:dist/modes/interactive/external-editor.js`, `…:dist/utils/output-files.js`

### 5. Relocation variables

`PI_CODING_AGENT_DIR` moves the agent dir. `PI_CODING_AGENT_SESSION_DIR`,
`--session-dir` or the `sessionDir` setting move sessions. `PI_PACKAGE_DIR`
moves the package dir. Without `PI_CODING_AGENT_DIR` the default is
`homedir()/.pi/agent`. XDG variables are not used.
`…:docs/environment-variables.md`, `…:dist/config.js` (`getAgentDir`)

### 6. Install and self-update

- There is no automatic update. A version check against `pi.dev` only
  notifies; `PI_SKIP_VERSION_CHECK` or `PI_OFFLINE` turn it off. `pi update`
  is manual. `…:docs/cli.md`, `…:dist/utils/version-check.js`
- For an npm install (a path containing `/node_modules/`), `pi update` infers
  the global prefix from its own package location
  (`<prefix>/lib/node_modules/@earendil-works/pi-coding-agent`). It runs
  `npm --prefix <that prefix> install -g --ignore-scripts --min-release-age=0
  @earendil-works/pi-coding-agent`. The update lands in place whatever
  `NPM_CONFIG_PREFIX` says, and it needs no install scripts.
  `…:dist/config.js` lines 44-62 (`detectInstallMethod`), 63-83
  (`getInferredNpmInstall`), 121-141 (`npm` case)

### 7. API-key variables

Each provider has its own variable, for example `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, `GEMINI_API_KEY`, `XAI_API_KEY`, `OPENROUTER_API_KEY`,
`COPILOT_GITHUB_TOKEN` and `OPENCODE_API_KEY`; the full table is in
`…:docs/providers.md` ("Use an API key from the environment").

---

## OpenCode (`opencode-ai@1.19.4`, `v1.19.4`)

### 1. Credentials

- `Global.Path.data/auth.json`, written with mode `0600`. Data is
  `$XDG_DATA_HOME/opencode`, else `~/.local/share/opencode`.
  `OPENCODE_AUTH_CONTENT` replaces the file with inline JSON.
  [auth/index.ts#L10](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/auth/index.ts#L10),
  [#L59-L61](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/auth/index.ts#L59-L61),
  [#L79](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/auth/index.ts#L79)
- MCP OAuth tokens go to `<data>/mcp-auth.json`.
  `opencode-linux-x64@1.19.4:bin/opencode`: `si=Wh.join(de.data,"mcp-auth.json")`
- No keyring use. `libsecret-1.so.0` appears in the binary only inside the Bun
  runtime's string table, and no `Bun.secrets` or `keytar` use was found.
  This is inferred from binary strings. A code search was run, but it covers
  the default branch, not the tag.

### 2. Config

The global config is `~/.config/opencode/opencode.json` or `opencode.jsonc`
(`$XDG_CONFIG_HOME/opencode`), with `tui.json`, `agents/`, `commands/` and
`plugins/` beside it. Older installs may still have
`~/.local/share/opencode/opencode.jsonc`.
[global.ts#L9-L29](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/core/src/global.ts#L9-L29),
[config.mdx](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/web/src/content/docs/config.mdx),
[troubleshooting.mdx](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/web/src/content/docs/troubleshooting.mdx)

### 3. History, transcripts, logs

Sessions are in the SQLite database `<data>/opencode.db`; channels other than
`latest`, `beta` and `prod` use `opencode-<channel>.db`. Some data is under
`<data>/storage/`, and logs are `<data>/log/` (the ten newest are kept).
[database.ts#L43-L55](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/core/src/database/database.ts#L43-L55),
[troubleshooting.mdx](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/web/src/content/docs/troubleshooting.mdx)

### 4. Caches and other state

- State is `$XDG_STATE_HOME/opencode` (`~/.local/state/opencode`). The cache
  is `$XDG_CACHE_HOME/opencode` (`~/.cache/opencode`), including `bin/` for
  downloaded tools. `repos/` is under data.
  [global.ts#L9-L29](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/core/src/global.ts#L9-L29)
- **Outside HOME:** `tmp` is `os.tmpdir()/opencode`, which is `/tmp/opencode`,
  created at startup. Its use was not traced; it is assumed to be transient.
  [global.ts#L15](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/core/src/global.ts#L15)

### 5. Relocation variables

- `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME`,
  through `xdg-basedir`. `OPENCODE_DB` takes an absolute path or one relative
  to data. `OPENCODE_CONFIG` names the config file and `OPENCODE_CONFIG_DIR`
  names a config directory. `OPENCODE_TUI_CONFIG` names the TUI config.
  [flag.ts](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/core/src/flag/flag.ts),
  [cli.mdx → environment variables](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/web/src/content/docs/cli.mdx)
- Ignored by these: `tmp` always follows `os.tmpdir()`. `OPENCODE_CONFIG_DIR`
  does not move data, state, cache or `auth.json`, which is why Tcode has no
  home override for OpenCode (`crates/agent/src/lib.rs:796-798`).

### 6. Install and self-update

- `postinstall.mjs` hard-links the platform binary over `bin/opencode.exe`.
  If the script never runs, the stub prints "opencode-ai's postinstall script
  was not run". `opencode-ai@1.19.4:bin/opencode.exe`, `…:postinstall.mjs`
- Method detection checks `execPath` for `.opencode/bin` or `.local/bin` and
  reports `curl` on a match. Otherwise it runs `npm list -g --depth=0` and
  matches `opencode-ai`. Under the wrapper's `NPM_CONFIG_PREFIX` that lists
  the agent's own prefix, so the method is `npm`.
  [installation/index.ts#L276-L308](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/installation/index.ts#L276-L308)
- The upgrade runs `npm install -g opencode-ai@<target>`, inheriting the
  environment, so it lands in place.
  [installation/index.ts#L333-L334](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/installation/index.ts#L333-L334)
- There is no automatic install: "Checking never installs anything. Updates
  only happen when the user runs `/update` or `opencode upgrade`." The check
  is off with `autoupdate: false` or `OPENCODE_DISABLE_AUTOUPDATE`.
  [cli/upgrade.ts#L7-L25](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/cli/upgrade.ts#L7-L25)
- `opencode upgrade --major` switches to OpenCode 2 with
  `npm uninstall -g opencode-ai && npm install -g @opencode/cli`
  ([#L95-L100](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/installation/index.ts#L95-L100)).
  `@opencode/cli@2.1.4` also ships an `opencode` bin (`npm view`), so the
  wrapper keeps working. OpenCode 2's storage paths were not inspected.
- This update also relies on `postinstall`, so the npm 12 risk described for
  Claude applies.

### 7. API-key variables

Provider variables come from models.dev, for example `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, `NVIDIA_API_KEY`, `CLOUDFLARE_API_KEY`, `GITLAB_TOKEN` and
`AWS_*`/`AWS_BEARER_TOKEN_BEDROCK`. Config can reference any variable with
`{env:NAME}`.
[providers.mdx](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/web/src/content/docs/providers.mdx)

---

## Cursor CLI (`cursor-agent` `2026.10.22-27dcf5f`)

### 1. Credentials

- The credential store comes from `AGENT_CLI_CREDENTIAL_STORE` (`file`,
  `memory`, else `default`). `default` selects the macOS keychain on `darwin`
  and the file store on every other platform:
  `"darwin"===(0,n.platform)()?new p(t.domain):new l(t.domain)`.
  The keychain module throws "Keychain operations are only supported on
  macOS" anywhere else. `agent-cli-package.tar.gz:index.js`
  (`./src/utils/credential-store.ts`, `../cli-credentials/dist/index.js`,
  `../keychain/dist/index.js`)
- On Linux the file is `$XDG_CONFIG_HOME/cursor/auth.json`, else
  `~/.config/cursor/auth.json`. The directory is set to `0700` and the file
  to `0600`.
  `getAuthFilePath`: `default:{const e=process.env.XDG_CONFIG_HOME||join(homedir(),".config");return join(e,t,"auth.json")}`
  with domain `"cursor"`. Headless use needs no keyring.
- **This path ignores `CURSOR_CONFIG_DIR`.**

### 2. Config

`cli-config.json` sits in the config dir: `CURSOR_CONFIG_DIR`, else
`$XDG_CONFIG_HOME/cursor`, else `~/.cursor`. Project config is
`<project>/.cursor/cli.json`.
`…:index.js` (`../cursor-config/dist/paths.js`, function `a()` and `m()`),
[Configuration](https://cursor.com/docs/cli/reference/configuration)

### 3. History, transcripts, logs

- Chats are `<config dir>/chats/<md5(cwd)>/`.
  `…:index.js`: `join((0,t.WI)(),"chats")`
- Per-project data is `<data dir>/projects/<slug>/`, including MCP OAuth tokens in
  `mcp-auth.json`. The data dir is `CURSOR_DATA_DIR`, else `~/.cursor`. Worktrees
  are `~/.cursor/worktrees/`.
  `…:index.js` (`paths.js`, function `c()` and `u()`),
  [Parameters](https://cursor.com/docs/cli/reference/parameters)

### 4. Caches and other state

- The Node compile cache is `$XDG_CACHE_HOME/cursor-compile-cache`
  (`~/.cache/cursor-compile-cache`). `…:cursor-agent` (launcher script)
- **Outside HOME:** socket and IPC names fall back to `/tmp/.cursor` when the
  projects path is too long, and pipe names use `$XDG_RUNTIME_DIR` when it is
  set. Both are transient.
  `…:index.js` (`paths.js`, function `d()`)

### 5. Relocation variables

`CURSOR_CONFIG_DIR` (config and chats), `CURSOR_DATA_DIR` (projects),
`XDG_CONFIG_HOME` (auth, and config when `CURSOR_CONFIG_DIR` is unset) and
`XDG_CACHE_HOME`. Ignored: `auth.json` ignores `CURSOR_CONFIG_DIR`. The
install tree, `~/.local/share/cursor-agent` and `~/.local/bin`, ignores
every variable and always uses `homedir()`.

### 6. Install and self-update

- The installer extracts to `~/.local/share/cursor-agent/versions/<version>/`
  and symlinks `~/.local/bin/agent` and `~/.local/bin/cursor-agent` to
  `…/versions/<version>/cursor-agent`. `https://cursor.com/install`
  lines 78-131
- The updater (`./src/commands/install-core-posix.ts` in the bundle) downloads
  into `join(homedir(),".local","share","cursor-agent","versions",<v>)` and
  takes `~/.local/share/cursor-agent/.install.lock`. It re-creates the
  `~/.local/bin/{agent,cursor-agent}` symlinks and writes a `~/.local/bin/cursor`
  shim. It keeps the two newest versions plus any version a `~/.local/bin`
  link resolves to. `…:2761.index.js`
- A background update runs 2 s after start unless `--disable-auto-update` is
  passed or the channel is `static`:
  `s.disableAutoUpdate||"static"===es.channel||setTimeout((()=>{(0,R.NB)({…isAutoUpdate:!0…})}),2e3)`.
  `…:index.js`;
  [Installation → updates](https://cursor.com/docs/cli/installation)
  ("will try to auto-update by default").
- Because `/data/agents/bin/{agent,cursor-agent}` link to the `~/.local/bin`
  links, **the next start runs the updated version**. Everything stays under
  `/data/home`.

### 7. API-key variables

`CURSOR_API_KEY` (or `--api-key`), and `CURSOR_AUTH_TOKEN` (found in the
bundle; not documented). `NO_OPEN_BROWSER=1` prints the login URL instead of
opening a browser.
[Authentication](https://cursor.com/docs/cli/reference/authentication)

---

## Grok CLI (`@xai-official/grok@1.0.70`)

### 1. Credentials

- Credentials are `$GROK_HOME/auth.json` (default `~/.grok`), guarded by
  `auth.json.lock`. MCP OAuth tokens are `~/.grok/mcp_credentials.json`.
  [Settings reference](https://docs.x.ai/build/settings/reference) ("`GROK_HOME`
  … Home for config, auth, sessions, skills, plugins, and logs"),
  [MCP servers](https://docs.x.ai/build/features/mcp-servers),
  [Enterprise](https://docs.x.ai/build/enterprise) ("nothing is written to
  `auth.json`"), and the binary strings
  `could not open or lock auth.json.lock` and
  `failed to persist API key to auth.json` in
  `@xai-official/grok-linux-x64@1.0.70:bin/grok.br`.
- No keyring crate was found in the binary: no `keyring`, `secret-service`,
  `libsecret` or keyutils strings. `zbus-5.14.0` is linked, and the D-Bus
  names it references are `org.freedesktop.portal.Settings` and appearance,
  which is theme detection, not storage. Headless use needs nothing extra.
  This is inferred from binary strings because the source is not public.

### 2. Config

`$GROK_HOME/config.toml`, `managed_config.toml` and `requirements.toml`, plus
the system layers `/etc/grok/managed_config.toml` and
`/etc/grok/requirements.toml`, which are read-only. The npm `postinstall`
writes `[cli] installer = "npm"` into `$GROK_HOME/config.toml`.
[Settings](https://docs.x.ai/build/settings), [Enterprise](https://docs.x.ai/build/enterprise),
`@xai-official/grok@1.0.70:bin/postinstall.js` lines 222-246

### 3. History, transcripts, logs

Sessions are `~/.grok/sessions/`, keyed by working directory, and include file
snapshots. MCP stderr logs are `~/.grok/logs/mcp/`. Crash reports go to
`$GROK_HOME/crash/`.
[Sessions](https://docs.x.ai/build/features/sessions),
[Headless scripting](https://docs.x.ai/build/cli/headless-scripting),
[MCP servers](https://docs.x.ai/build/features/mcp-servers),
[Settings reference](https://docs.x.ai/build/settings/reference)

### 4. Caches and other state

Versioned binaries live in `$GROK_HOME/bin/` (see item 6), along with
`models_cache.json` and the update `version.json` (binary strings). Temp use
was not traced (unverified).

### 5. Relocation variables

`GROK_HOME` covers everything, and the npm scripts honour it too
(`bin/grok-bootstrap.js`, `bin/postinstall.js`). `GROK_CONFIG` and
`GROK_CONFIG_PATH` exist in the binary but are undocumented (unverified).
XDG variables are not used.

### 6. Install and self-update

- The npm `postinstall` decompresses the platform binary to
  `$GROK_HOME/bin/grok-<version>` and points the symlink
  `$GROK_HOME/bin/grok` at it. On Unix installs whose user agent is `npm/`, it
  also replaces the package's own `bin/grok` with a symlink to a package-local
  `bin/grok-native`. `…:bin/postinstall.js` lines 1-16, 189-216
- The Tcode wrapper therefore execs the package-local `grok-native`. If
  `postinstall` did not run, `bin/grok` stays the Node bootstrap, which
  **prefers `$GROK_HOME/bin/grok` whatever its version** (`resolveBinary`).
  `…:bin/grok-bootstrap.js`
- The updater (`crates/codegen/xai-grok-update/src/auto_update.rs` in the
  binary) has an npm path: `Installing via npm...`, `npm`, `i`, `-g`, then
  `@xai-official/grok@<version>`, falling back to the dist-tag. Run that way,
  it updates the wrapper's prefix in place and `postinstall` re-links both
  `bin/grok` and `$GROK_HOME/bin/grok`.
- The same module also downloads builds itself: `Auto-update running in
  background.`, `Background update: target already on disk, skipping
  download`, `~/.grok/bin/grok is not a link to an installed build`.
  **Unverified:** for an npm install, does the background updater use the npm
  path, or download into `~/.grok/bin` and repoint only `~/.grok/bin/grok`?
  In the second case the wrapper would keep running the stale package-local
  `grok-native`. To settle it, install `@xai-official/grok@<older>` into the
  prefix, run `grok update` and a background-update start, and see which file
  changes.
- Disable with `[cli] auto_update = false`, `GROK_DISABLE_AUTOUPDATER`, or
  `--no-auto-update`, which the docs recommend for ACP/`-p` automation.
  [Headless scripting](https://docs.x.ai/build/cli/headless-scripting),
  [Settings reference](https://docs.x.ai/build/settings/reference)
- The update relies on `postinstall`, so the npm 12 risk applies. Here it
  would fail silently: the Node bootstrap would keep launching the old
  `$GROK_HOME/bin/grok`.

### 7. API-key variables

`XAI_API_KEY`, `model.api_key` or `model.env_key` in config, and
`GROK_CODE_XAI_API_KEY` (binary string; not documented). Precedence is
`model.api_key` > `model.env_key` > session token > `XAI_API_KEY`.
[Enterprise → authentication](https://docs.x.ai/build/enterprise)

---

## Checking the docs/docker.md credential table

| Row in `docs/docker.md` | Verdict | Correction or addition |
| --- | --- | --- |
| Claude: `.claude`, `.claude.json` | Correct | `.credentials.json` is inside `.claude`. A Console sign-in without an API key also writes `.config/anthropic/` (`configs/`, `credentials/`). |
| Codex: `.codex/auth.json` | Correct (default `file` store) | MCP OAuth tokens fall back to `.codex/.credentials.json` because no keyring is reachable. Everything else is under `.codex/`. |
| pi: `.pi/agent/auth.json` | Correct | — |
| OpenCode: `.local/share/opencode/auth.json` | Correct | MCP OAuth is `.local/share/opencode/mcp-auth.json`. Config is `.config/opencode/`; sessions are in `.local/share/opencode/opencode.db`. |
| Cursor: `.config/cursor`, `.cursor` | Correct, but the two roles are not spelled out | The login is `.config/cursor/auth.json`. `.cursor` holds `cli-config.json`, `chats/`, `projects/` (MCP tokens) and `worktrees/`. The install is `.local/share/cursor-agent` plus `.local/bin`. |
| Grok: `.grok/auth.json` | Correct | MCP OAuth is `.grok/mcp_credentials.json`. `.grok/bin/` holds the binary copies. |

Other statements in `docs/docker.md` that need correcting:

- "it updates itself the way that CLI does outside a container; a CLI that
  installs its updates with npm updates this same copy". That is true for the
  update *target*, but only Claude Code, Cursor and (probably) Grok update on
  their own. Codex (TUI prompt), pi (`pi update`) and OpenCode (`/update`,
  `opencode upgrade`) only notify, and only when used interactively. Under
  Tcode's `app-server`, `rpc` and `serve` modes they stay on the version
  installed at first start until someone updates them in a terminal pane.
- "Upgrading the image does not reinstall the agent CLIs: they live in the
  volume and update themselves" has the same caveat.
- Reinstalling Cursor should also remove the `~/.local/bin/cursor` shim the
  updater writes.

## Summary table

Paths are relative to `HOME` (`/data/home`).

| Agent | Credentials | Config | History | Caches and other state | Relocation vars (and what ignores them) | Keyring when headless | Self-update target | API-key env | Verdict |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Claude Code | `.claude/.credentials.json`; `.config/anthropic/` (Console profile) | `.claude/settings.json`, `.claude.json` | `.claude/projects/`, `.claude/history.jsonl` | `.claude/*`; **`/tmp/claude-<uid>/`** (scratchpad, pasted images) | `CLAUDE_CONFIG_DIR` (moves `.claude.json` too), `CLAUDE_CODE_TMPDIR`, `ANTHROPIC_CONFIG_DIR`, `CLAUDE_CODE_PLUGIN_CACHE_DIR` | Not used on Linux (file) | Background `npm install -g` into the wrapper's prefix (in place) | `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN` | Persists with only `/data`, except the scratchpad and pasted-image temp files |
| Codex | `.codex/auth.json`; MCP: `.codex/.credentials.json` | `.codex/config.toml`, `.codex/.env` | `.codex/sessions/`, `history.jsonl`, SQLite | `.codex/shell_snapshots/`, `version.json`, `tmp/arg0-linux/`, `log/` | `CODEX_HOME`, `CODEX_SQLITE_HOME`; XDG ignored | `file` default; MCP `auto` falls back to file | TUI prompt only: `npm install -g @openai/codex` (in place) | `CODEX_API_KEY` (exec/SDK only), `CODEX_ACCESS_TOKEN`; `OPENAI_API_KEY` via `codex login --with-api-key` | Persists with only `/data` |
| pi | `.pi/agent/auth.json` | `.pi/agent/settings.json`, `models.json`, `mcp.json`, `trust.json` | `.pi/agent/sessions/` | `.pi/agent/bin/`, `npm/`, `git/`; `/tmp` transient only | `PI_CODING_AGENT_DIR`, `PI_CODING_AGENT_SESSION_DIR`, `PI_PACKAGE_DIR`; XDG ignored | Not used | Manual `pi update`: `npm --prefix <own prefix> install -g --ignore-scripts` (in place) | Per provider: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `XAI_API_KEY`, … | Persists with only `/data` |
| OpenCode | `.local/share/opencode/auth.json`, `mcp-auth.json` | `.config/opencode/opencode.json(c)` | `.local/share/opencode/opencode.db`, `log/` | `.local/state/opencode/`, `.cache/opencode/`; **`/tmp/opencode`** | `XDG_*_HOME`, `OPENCODE_DB`, `OPENCODE_CONFIG(_DIR)`; tmp ignores them | Not used | Manual `/update` or `opencode upgrade`: `npm install -g opencode-ai@X` (in place) | models.dev provider vars (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …) | Persists with only `/data` (`/tmp/opencode` assumed transient) |
| Cursor | `.config/cursor/auth.json` | `.cursor/cli-config.json` | `.cursor/chats/`, `.cursor/projects/` | `.cache/cursor-compile-cache`; install in `.local/share/cursor-agent/versions/`, `.local/bin/` | `CURSOR_CONFIG_DIR`, `CURSOR_DATA_DIR`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`; **`auth.json` ignores `CURSOR_CONFIG_DIR`**; install tree ignores all | Linux uses the file; keychain is macOS-only | Background download to `.local/share/cursor-agent/versions/` and repointed `.local/bin` links (followed by `/data/agents/bin`) | `CURSOR_API_KEY` | Persists with only `/data` |
| Grok | `.grok/auth.json`, `.grok/mcp_credentials.json` | `.grok/config.toml` (+ `/etc/grok/*` read-only) | `.grok/sessions/`, `.grok/logs/` | `.grok/bin/`, `models_cache.json` | `GROK_HOME`; XDG ignored | Not used (binary strings) | npm path `npm i -g` (in place) **or** download into `.grok/bin` (unverified which; the second would leave the wrapper on the old build) | `XAI_API_KEY`, `model.api_key`/`env_key` | Persists with only `/data`; whether the update target matches the run path is unverified |

Writes outside `/data` that matter: Claude's `/tmp/claude-<uid>/` and
OpenCode's `/tmp/opencode`. Nothing writes to `/root`, `/etc`, `/opt` or
`/usr/lib/node_modules`. The `/etc` paths above are read only, and
`NPM_CONFIG_PREFIX` keeps every npm global write in `/data/agents`. No CLI
depends on a keyring on Linux.

## Further findings for the container

1. **Every agent's subprocesses inherit `NPM_CONFIG_PREFIX=/data/agents/<arch>/<agent>`.**
   The wrapper exports it for the agent process, so a `npm install -g`,
   `npx` or `npm prefix -g` that the agent's own shell tool runs targets the
   agent's prefix, not `/data/agents/custom`. The installed command lands in
   `/data/agents/<arch>/<agent>/bin`, which is not on `PATH`. Reinstalling the
   agent, which deletes the prefix, removes it. This follows from process
   environment inheritance; it was not run. Terminal panes are unaffected:
   they get Tcode's own environment, where the prefix is `/data/agents/custom`.
2. **A newer npm on `PATH` would break three self-updaters.** Claude Code,
   OpenCode and Grok rely on `postinstall`, and their updaters do not pass
   `--allow-scripts`. npm 11.19.1 in the image runs those scripts with a
   notice. npm 12 skips them by default. The likeliest way npm 12 reaches
   `PATH` is a user's `npm install -g npm@latest`, which lands in
   `/data/agents/custom/bin`, ahead of the image's npm.
3. **Tcode's Cursor home override does not move the login.** Tcode maps a
   provider home to `CURSOR_CONFIG_DIR` (`crates/agent/src/lib.rs:813`), but
   Cursor reads `auth.json` from `$XDG_CONFIG_HOME/cursor` or
   `~/.config/cursor`, and project data from `CURSOR_DATA_DIR` or `~/.cursor`.
   Two Cursor profiles with different homes would share one login. This is not
   specific to the container.
4. Tcode's own provider-update detection probably reports these installs as
   `Unknown`. It canonicalizes the command on `PATH`, and the sh wrapper in
   `/data/agents/bin` is not a symlink. The resolver recognizes only Windows
   `.cmd` shims as wrappers (`crates/services/src/version_check/provider_updates/installation.rs`
   around lines 1290-1330), and it binds npm updates to `npm root --global`,
   which here is `/data/agents/custom`. If so, Tcode offers no in-app updates
   for the five npm CLIs. Unverified; settled by checking what Settings →
   Providers shows in a running container.
5. `docker compose exec` without `--user tcode` runs as root with
   `HOME=/data/home`. Running a CLI that way leaves root-owned files in the
   shared home (`.claude.json`, `.codex/`), which the `PUID` user then cannot
   rewrite. The entrypoint only fixes ownership at the next start, and only
   when `/data` itself has the wrong owner. `docs/docker.md` already says to
   use `--user tcode`.

## Recommendations

Each item is tied to the finding it addresses. What the container does now:

- Item 1 is not applied. An exported `NPM_CONFIG_ALLOW_SCRIPTS` reaches every
  process an agent starts, so under the image's npm 11 it would skip the
  install scripts of a project's own dependencies whenever an agent runs
  `npm install`. `docs/docker.md` says to keep the image's npm instead.
  (npm 11.19.1 and 12.2.1 both read `allow-scripts` from the environment,
  checked with `NPM_CONFIG_ALLOW_SCRIPTS=x npm config get allow-scripts`.)
- Item 2 is superseded: npm CLIs install into the image's own global prefix,
  `/data/agents/<arch>/npm`, which is on `PATH`, with no wrapper. An agent's
  global installs land there, run and persist, and the CLIs resolve as npm
  global installs, so Tcode's provider update recognises them (finding 4).
  An npm CLI left out of `TCODE_AGENTS` is uninstalled to keep it off `PATH`.
- Item 3: docs only, as recommended.
- Item 4: the Grok command links to `~/.grok/bin/grok` once it exists. The
  package's `postinstall` installs the build there (observed: `~/.grok/bin/grok
  -> grok-1.0.70`), so both update paths move the binary that runs.
- Items 5 and 6: applied to `docs/docker.md`, with Tcode's provider update
  as the way to update Codex, pi and OpenCode.
- Item 7: `CURSOR_DATA_DIR` joins `CURSOR_CONFIG_DIR`; the login stays shared,
  because moving `XDG_CONFIG_HOME` or `HOME` would also move the configuration
  of every tool the agent runs. The `LaunchEnv` documentation says so.

1. **Pin the install-script policy for self-updates** (finding 2). In
   `install-agents.mjs`, add `NPM_CONFIG_ALLOW_SCRIPTS='<package>'` to the
   wrapper, next to `NPM_CONFIG_PREFIX`. Then the CLIs' own `npm install -g`
   keeps running `postinstall` under any npm. npm reads `allow-scripts` like
   any other config key (`npm@11.19.1:docs/content/using-npm/config.md`:
   "persist the setting with `npm config set allow-scripts=…`"). That npm
   accepts it from the environment is unverified; settled by
   `NPM_CONFIG_ALLOW_SCRIPTS=x npm config get allow-scripts`.
2. **Document, or contain, the inherited prefix** (finding 1). The minimal
   step is a sentence in `docs/docker.md` → *Choose agents*: global npm
   installs made by an agent's own tools go into that agent's prefix. To
   contain it, the wrapper can keep `NPM_CONFIG_PREFIX` and also prepend
   `<prefix>/bin` to `PATH`, so such installs at least run. Moving them to
   `/data/agents/custom` would need each updater to target a prefix some
   other way. pi already does this (`--prefix` inferred from its own path);
   the other four would need a different launch mechanism.
3. **Decide about the temp tree** (Claude scratchpad and pasted images; OpenCode
   tmp). If those should survive restarts, set `CLAUDE_CODE_TMPDIR`
   (Claude-only), or `TMPDIR` (all CLIs), to a directory on the volume such as
   `/data/tmp`, in the Dockerfile `ENV` and created by `entrypoint.sh`.
   Otherwise state in `docs/docker.md` → *Data and projects* that `/tmp` is
   not persisted and that Claude's scratchpad and pasted-image copies live
   there. The docs-only option is recommended: neither CLI treats these as
   durable state.
4. **Grok update target** (Grok item 6). Until the npm-versus-download
   question is settled, either set `GROK_DISABLE_AUTOUPDATER=1` in the Grok
   wrapper and rely on `grok update` run in a pane, or have the wrapper exec
   `$GROK_HOME/bin/grok` when it exists. That symlink is the one both the npm
   `postinstall` and the native updater maintain. The second option is
   preferred if the experiment shows the background updater downloads into
   `~/.grok/bin`.
5. **`docs/docker.md` credential table** (verification above). Keep the rows
   and add the missing state:
   - Claude: `.config/anthropic` for a Console sign-in.
   - Codex: `.codex/.credentials.json` for MCP OAuth.
   - OpenCode: `.local/share/opencode/mcp-auth.json`.
   - Cursor: say that `.config/cursor/auth.json` is the login and `.cursor`
     holds config and chats.
   - Grok: `.grok/mcp_credentials.json`.

   One footnote should say that no CLI uses a keyring on Linux, so nothing
   depends on D-Bus.
6. **`docs/docker.md` → *Choose agents* and *Upgrade*** (summary table, self-update
   column). Replace "updates itself the way that CLI does outside a
   container" with the actual split. Claude Code and Cursor update in the
   background. Grok has a background updater, target unverified. Codex, pi and
   OpenCode update only when you run their update command (`codex` TUI
   prompt, `pi update`, `opencode upgrade`) in a terminal pane. In *Upgrade*,
   add `~/.local/bin/cursor` to the Cursor reinstall list.
7. **Cursor home override** (finding 3; outside `crates/headless`). When
   Tcode applies a Cursor home, it should set `XDG_CONFIG_HOME` and
   `CURSOR_DATA_DIR` as well as `CURSOR_CONFIG_DIR`, or document that the
   override does not isolate the login.

## Not confirmed

- Whether Claude Code's background updater runs in `-p`/SDK sessions (Claude item 6).
- Grok's update path for npm installs (Grok item 6), and Grok's temp-file
  use. The source is not public; only binary strings were read.
- What OpenCode keeps in `/tmp/opencode`.
- Tcode's own update detection for wrapper scripts (finding 4).
- No CLI was run in a container. Every behaviour above comes from docs,
  source at the cited tags, or strings in the published artifacts.
