# Grok Build plugin probe — commands

- CLI: `grok 1.0.46 (2765805b9442)`, darwin-arm64, binary
  `/tmp/grok-inspect.D1FK/bin/grok`. Date 2026-10-03, macOS (Darwin 27.0.0,
  arm64).
- Every command ran with stdin = `/dev/null` (with an inherited non-terminal
  stdin, `grok plugin` fails with `Device not configured (os error 6)`), from
  the cwd `/tmp/tcode-probe-grok/outside`, stdout and stderr captured
  separately.
- Env: `PATH USER LOGNAME SHELL TMPDIR LANG` plus `HOME=/tmp/tcode-probe-grok/home`
  and `GROK_HOME=/tmp/tcode-probe-grok/home/.grok`. No `XAI_API_KEY`; plugin
  management needs none. With the real `HOME`, Grok also reads the user's
  Claude Code settings and lists their marketplaces, so `HOME` is isolated too.
- Paths: local marketplace `/tmp/tcode-probe-grok/mkt` (plugins `alpha`:
  skill, command, SessionStart hook and an HTTP MCP server; `beta`: skill),
  git marketplace `/tmp/tcode-probe-grok/tools` added as a `file://` URL
  (plugin `zeta`: skill and an HTTP MCP server), and the two-plugin repository
  `/tmp/tcode-probe-grok/multi` (`delta`, `epsilon`, one directory each, each
  with `.grok-plugin/plugin.json`). All three are git repositories.
  Marketplace indexes are `.grok-plugin/marketplace.json`; Grok names a
  source after its directory or repository (`mkt`, `tools`), not after the
  index's `name`.

## Recorded commands

| # | Command | Exit | Fixture |
|---|---------|------|---------|
| 01 | `grok plugin list --json --available` | 0 | (`[]`) |
| 02 | `grok plugin marketplace add /tmp/tcode-probe-grok/mkt` | 0 | `Added marketplace source: mkt (…)` |
| 03 | `grok plugin marketplace add file:///tmp/tcode-probe-grok/tools` | 0 | `Added marketplace source: tools (…)` |
| 04 | `grok plugin marketplace list --json` | 0 | `marketplace-list.json` |
| 05 | `grok plugin list --json --available` | 0 | three available plugins |
| 06 | `grok plugin install alpha@mkt` | 1 | `install-untrusted.stderr.txt` |
| 07 | `grok plugin install alpha@mkt --trust` | 0 | `install-trusted.txt` |
| 08 | `grok plugin install zeta@tools --trust` | 0 | `Installed 1 plugin(s) from tools: zeta` |
| 09 | `grok plugin install /tmp/tcode-probe-grok/multi --trust` | 0 | `Installed 2 plugin(s) from …: epsilon, delta` |
| 10 | `grok plugin list --json --available` | 0 | `plugin-list-available.json` |
| 11 | `grok plugin details alpha` | 0 | `details-alpha.txt` |
| 13 | `grok plugin details delta` | 0 | `details-delta.txt` |
| 14 | `grok plugin update alpha` (after alpha → 1.1.0 in `mkt`) | 0 | `update.txt` |
| 16 | `grok plugin uninstall delta` | 1 | `Plugin "delta" belongs to repo "multi-0b1623cc" which also contains: - epsilon … To proceed: grok plugin uninstall delta --confirm` |
| 17 | `grok plugin uninstall zeta` | 0 | `Uninstalled 1 plugin(s): zeta` |
| 19 | `grok plugin marketplace remove mkt` | 0 | `Removed marketplace source and uninstalled 1 plugin(s): alpha-e84495d9` |
| 22 | `grok plugin install nosuch@tools --trust` | 1 | `install-unknown.stderr.txt` |
| 23 | `grok plugin marketplace add file:///tmp/tcode-probe-grok/tools` | 1 | `Error: Marketplace source already configured: …` |
| 24 | `grok plugin disable delta` | 0 | `Disabled plugin: delta`; `config.toml` `[plugins] disabled = ["delta"]` |
| 25 | `grok inspect --json` | 0 | `plugins: [{"name": "delta", "enabled": true, …}, …]` |
| 26 | `grok plugin enable delta` | 0 | `Enabled plugin: delta` |
| 27 | `grok plugin update delta` (after delta → 1.0.1 in `multi`) | 0 | `update-path-plugin.txt`; the list still reports 1.0.0 |
| 29 | `grok plugin install file:///tmp/tcode-probe-grok/multi --trust` (second home) | 0 | `Installed 2 plugin(s) from file:///tmp/tcode-probe-grok/multi: epsilon, delta` |
| 30 | `grok plugin list --json --available` (second home) | 0 | `plugin-list-git-install.json` |
| 31 | `grok plugin details delta` (second home) | 0 | `details-git-install.txt` |

Each mutation was followed by a `plugin list --json --available` or
`plugin marketplace list --json` showing its effect (05, 10, 15, 18, 20, 21,
28 in the run).

29–31 ran in a second, empty home (`HOME=/tmp/tcode-probe-grok-git/home`,
`GROK_HOME` under it), otherwise as above. In a third throwaway home,
`grok plugin install ../multi --trust` from the cwd `outside` listed both
plugins with `source` `/private/tmp/tcode-probe-grok/outside/../multi`; no
fixture was kept.

## Findings the implementation relies on

- Installing without `--trust` exits 1 and prints, on stderr, what installing
  activates and the exact command that proceeds (06). That command and text
  are the challenge a person accepts; `--trust` is passed only on a re-run
  whose challenge text hashes to the accepted SHA-256.
- A plugin installed from a path or a URL has no marketplace, and its listed
  `source` is the path made absolute against the cwd, or the URL as given
  (10, 30); the listing carries no kind for it. `plugin details`, which
  prints one block per repository (the same for `delta` and `epsilon`),
  says `kind: local: <path>` (13) or `kind: git: <url>` (31), and the kind
  is taken from there rather than from the path's syntax, which is the
  host's.
- `plugin update` refreshes a git marketplace's checkout itself (verified
  1.1.0 → 1.2.0 without `marketplace update`). For a path install it reports
  `local symlink, already live` and changes nothing (27), so Update is offered
  for marketplace installs only. A plugin added to a git marketplace after it
  was added is not installable until `grok plugin marketplace update`, which
  the plugin contract has no action for.
- Uninstalling one plugin of a multi-plugin repository needs `--confirm` and
  removes them all (16). The contract's uninstall carries no consent, so
  Uninstall is not offered for those plugins.
- `marketplace remove` uninstalls the marketplace's plugins without asking
  (19); the listing reports them so the host confirms first.
- Enablement: `install --trust` adds the plugin to `[plugins] enabled` in
  `config.toml`; `disable`/`enable` move it between `enabled` and `disabled`.
  No `grok plugin` read reports it, and `grok inspect` (text and `--json`)
  reports a disabled plugin as enabled (25). A session started while `alpha`
  was disabled listed none of its commands; after `enable`, a new session
  listed `alpha-cmd` and `alpha-note`. Enable and Disable are therefore not
  offered.

## When a change applies

Checked against a running `grok agent stdio` session (scripted model backend):
after `grok plugin install zeta@tools --trust` ran outside the session,
nothing changed until the prompt `/reload-plugins`, which answered `Plugin
registry rebuilt: 4 plugin(s), 1 hook(s) reloaded, MCP refreshed, 6 skill(s)
refreshed.`, listed `zeta-note`, and connected zeta's MCP server (`zeta-http`
ready, with its bearer header). After `grok plugin update beta` added a skill,
`/reload-plugins` listed `beta-extra`. A session started afterwards had both.
