# Claude Code plugin probe — commands

- CLI: `2.1.285 (Claude Code)` (`claude --version`), binary `/Users/tryanks/.local/share/mise/installs/claude/latest/claude` → `2.1.285/claude`. Date 2026-10-03, macOS (Darwin 27.0.0, arm64).
- Driver: `/tmp/tcode-claude-run/probe.py` + `probe_lib.py` (not in the repo). Every command runs with stdin = `/dev/null`, stdout and stderr captured separately.
- **Env** (all runs): a clean base of `PATH USER LOGNAME SHELL TMPDIR LANG` only. The probe itself runs inside a Tcode-launched Claude Code session; its `CLAUDECODE`/`CLAUDE_CODE_*` variables are deliberately *not* inherited. Adding them back one at a time to an otherwise accepted `--accept-command` install showed that `CLAUDECODE` and `CLAUDE_CODE_CHILD_SESSION` each make the CLI refuse it ("--accept-command is ignored inside a Claude Code session"); `CLAUDE_CODE_ENTRYPOINT`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_PID`, `CLAUDE_CODE_EXECPATH`, `AI_AGENT`, `CLAUDE_CODE_SESSION_ATTENDED` and `CLAUDE_CODE_MESSAGING_SOCKET` do not. Those recordings are not kept.
  - `iso` = base + `HOME=/tmp/tcode-probe/claude-home` + `CLAUDE_CONFIG_DIR=/tmp/tcode-probe/claude-home/.claude` (CLAUDE_CONFIG_DIR is not in `claude --help`; it is honoured — all state landed under it).
  - `real` = base + `HOME=/Users/tryanks` (read-only commands and the one `--plugin-dir` session only).
- Paths: marketplace `/tmp/tcode-probe/mkt` (the `manifests/mkt` files below), command-source output dir `/tmp/tcode-probe/gamma-src` (the `manifests/gamma-src` files below), project `/tmp/tcode-probe/proj` (macOS realpath `/private/tmp/tcode-probe/proj`), non-project cwd `/tmp/tcode-probe/outside`.

## Authored manifests

### `manifests/gamma-src/.claude-plugin/plugin.json`

```json
{
  "name": "gamma",
  "version": "0.0.1",
  "description": "Plugin directory produced by the gamma command source"
}
```

### `manifests/gamma-src/skills/gamma-note/SKILL.md`

```markdown
---
name: gamma-note
description: Writes a short note from gamma. Use when the user asks for a gamma note.
---

Reply with "gamma note".
```

### `manifests/mkt/.claude-plugin/marketplace.json`

```json
{
  "name": "tcode-probe",
  "description": "Throwaway marketplace for the Tcode plugin probe",
  "owner": { "name": "Tcode probe" },
  "plugins": [
    {
      "name": "alpha",
      "source": "./plugins/alpha",
      "description": "Skill, command, SessionStart hook and an unreachable HTTP MCP server",
      "version": "1.0.0"
    },
    {
      "name": "beta",
      "source": "./plugins/beta",
      "description": "Skill only",
      "version": "0.1.0"
    },
    {
      "name": "gamma",
      "source": {
        "source": "command",
        "command": "echo /tmp/tcode-probe/gamma-src",
        "timeout": 30
      },
      "description": "Command-source plugin"
    }
  ]
}
```

### `manifests/mkt/plugins/alpha/.claude-plugin/plugin.json`

```json
{
  "name": "alpha",
  "version": "1.0.0",
  "description": "Skill, command, SessionStart hook and an unreachable HTTP MCP server"
}
```

### `manifests/mkt/plugins/alpha/.mcp.json`

```json
{
  "mcpServers": {
    "alpha-dead": {
      "type": "http",
      "url": "http://127.0.0.1:9/mcp"
    }
  }
}
```

### `manifests/mkt/plugins/alpha/commands/hello.md`

```markdown
---
description: Print a hello line from the alpha plugin
---

Reply with exactly: hello from alpha command
```

### `manifests/mkt/plugins/alpha/hooks/hooks.json`

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          { "type": "command", "command": "echo alpha-session-start" }
        ]
      }
    ]
  }
}
```

### `manifests/mkt/plugins/alpha/skills/greet/SKILL.md`

```markdown
---
name: greet
description: Greets the user by name. Use when the user asks to be greeted.
---

Say "Hello from alpha" followed by the user's name.
```

### `manifests/mkt/plugins/beta/.claude-plugin/plugin.json`

```json
{
  "name": "beta",
  "version": "0.1.0",
  "description": "Skill only"
}
```

### `manifests/mkt/plugins/beta/skills/beta-note/SKILL.md`

```markdown
---
name: beta-note
description: Writes a short note from beta. Use when the user asks for a beta note.
---

Reply with "beta note".
```

Before step `16-update` the driver rewrote `"version": "1.0.0"` → `"1.0.1"` in `mkt/.claude-plugin/marketplace.json` and `mkt/plugins/alpha/.claude-plugin/plugin.json`.

## Commands, in execution order

Only the runs whose output is kept here are listed; the numbering is the probe's.

| # | step | argv (after the binary) | cwd | env | exit | stdout | stderr |
|---|---|---|---|---|---|---|---|
| 2 | `real-marketplace-list` | `plugin marketplace list --json` | `/tmp/tcode-probe/outside` | real | 0 | `real-home-marketplace-list.json` | (empty) |
| 9 | `04-marketplace-list` | `plugin marketplace list --json` | `/tmp/tcode-probe/proj` | iso | 0 | `marketplace-list.json` | (empty) |
| 11 | `06-install-alpha-user` | `plugin install alpha@tcode-probe --scope user --json` | `/tmp/tcode-probe/proj` | iso | 0 | `install-user.json` | (empty) |
| 17 | `09c-plugin-list-available-after-install` | `plugin list --json --available` | `/tmp/tcode-probe/proj` | iso | 0 | `plugin-list-available-after-install.json` | (empty) |
| 19 | `10-details` | `plugin details alpha` | `/tmp/tcode-probe/proj` | iso | 0 | `details.txt` | (empty) |
| 21 | `11-disable` | `plugin disable alpha --json` | `/tmp/tcode-probe/proj` | iso | 0 | `disable.json` | (empty) |
| 23 | `12-enable` | `plugin enable alpha --json` | `/tmp/tcode-probe/proj` | iso | 0 | `enable.json` | (empty) |
| 24 | `13-disable-user-scope` | `plugin disable alpha --scope user --json` | `/tmp/tcode-probe/proj` | iso | 0 | `disable-scope-user.json` | (empty) |
| 32 | `16-update` | `plugin update alpha --json` | `/tmp/tcode-probe/proj` | iso | 0 | `update.json` | (empty) |
| 35 | `17-uninstall-beta-project` | `plugin uninstall beta --scope project --json` | `/tmp/tcode-probe/proj` | iso | 0 | `uninstall.json` | (empty) |
| 36 | `18-install-gamma-challenge` | `plugin install gamma@tcode-probe --json` | `/tmp/tcode-probe/proj` | iso | 1 | `install-command-source-challenge.txt` | (not kept) |
| 38 | `19-install-gamma-wrong-hash` | `plugin install gamma@tcode-probe --json --accept-command 0000000000000000000000000000000000000000000000000000000000000000` | `/tmp/tcode-probe/proj` | iso | 1 | `install-command-source-wrong-hash.json` | (not kept) |
| 56 | `20-install-gamma-accepted` | `plugin install gamma@tcode-probe --json --accept-command b9c02c85b17261fa1fce010664bb16cbe866f14c7deeaf6cb6c37c2736bbe269` | `/tmp/tcode-probe/proj` | iso | 0 | `install-command-source-accepted.json` | (empty) |

Notes:

- 2 `real-marketplace-list`: REAL home, read-only.
- 21 `11-disable`: No --scope: auto-detect.
- 23 `12-enable`: No --scope: auto-detect.
- 32 `16-update`: Before this run alpha version was bumped 1.0.0 -> 1.0.1 in marketplace.json and plugin.json. No --scope.
- 56 `20-install-gamma-accepted`: --accept-command = shownCommand.sha256 from step 18 (b9c02c85b17261fa1fce010664bb16cbe866f14c7deeaf6cb6c37c2736bbe269); clean env.

## `claude plugin --help` (exit 0)

```text
Usage: claude plugin|plugins [options] [command]

Manage Claude Code plugins

Options:
  -h, --help                           Display help for command

Commands:
  configure [options] <plugin>         Show a plugin's options and which are
                                       unset, or save values from stdin with
                                       --values-stdin
  details [options] <name>             Show a plugin's component inventory and
                                       projected token cost
  disable [options] [plugin]           Disable an enabled plugin
  enable [options] <plugin>            Enable a disabled plugin
  eval [options] [target]              Run eval cases (<eval dir>/**/case.yaml
                                       or prompt.md + graders/*.md; the eval dir
                                       is evals/ unless --eval-dir or the
                                       manifest says otherwise) against a plugin
                                       and report scored results. Target is a
                                       path, a plugin name, or a
                                       `plugin@marketplace` id — installed and
                                       skills-dir plugins both resolve (and add
                                       a no-plugin baseline arm). It loads the
                                       plugin and runs its eval suite (prompts,
                                       graders; scaffold scripts and real MCP
                                       servers only when you opt in) on your
                                       machine, as you: only evaluate plugins
                                       you trust — the run's sandboxing limits
                                       what a malicious plugin can reach but is
                                       not a guarantee, and a bundled suite
                                       passing is not a security vetting. The
                                       first run in an untrusted plugin
                                       directory asks you to confirm
                                       (--trust-plugin answers for CI)
  help [command]                       display help for command
  init|new [options] <name>            Scaffold a new plugin at
                                       ~/.claude/skills/<name>/ (auto-loads next
                                       session as <name>@skills-dir)
  install|i [options] <plugin>         Install a plugin from available
                                       marketplaces (use plugin@marketplace for
                                       specific marketplace)
  list [options]                       List installed plugins
  marketplace                          Manage Claude Code marketplaces
  prune|autoremove [options]           Remove auto-installed dependencies that
                                       are no longer needed
  tag [options] [path]                 Create a {name}--v{version} git tag for a
                                       plugin release, validating that
                                       plugin.json and any enclosing marketplace
                                       entry agree
  uninstall|remove [options] <plugin>  Uninstall an installed plugin
  update [options] <plugin>            Update a plugin to the latest version
                                       (restart required to apply)
  validate [options] <path>            Validate a plugin or marketplace
                                       manifest, or the skills, agents, and
                                       commands in a directory
```

## Every subcommand `--help` (`bash -c 'claude plugin $s --help'`, exit codes inline)

```text
=== claude plugin list --help
Usage: claude plugin list [options]

List installed plugins

Options:
  --available           Include available plugins from marketplaces (requires
                        --json)
  --data-size [plugin]  Measure each installed plugin's saved data directory, or
                        only the named plugin's (requires --json)
  -h, --help            Display help for command
  --json                Output as JSON
[exit 0]

=== claude plugin install --help
Usage: claude plugin install|i [options] <plugin>

Install a plugin from available marketplaces (use plugin@marketplace for
specific marketplace)

Options:
  --accept-command <sha256>  Accept the marketplace-declared command (a
                             command-source install, or the headersHelper that
                             fetches the archive) whose sha256 a previous --json
                             run reported as shownCommand.sha256; counts as -y
                             for exactly that command, for that plugin and
                             marketplace catalog, and nothing else. If either
                             changed (a refresh that moved the catalog counts),
                             the run refuses and reports the command again, to
                             be shown to a person again
  --config <key=value>       Set a userConfig option declared in the plugin's
                             manifest, or a bundled .mcpb server's own
                             user_config field as <server>.<key>=<value> (a bare
                             key works when only one bundled server declares
                             it). Repeatable. Values are validated against the
                             schema and stored via the same path as the
                             interactive /plugin configure flow.
  -h, --help                 Display help for command
  --json                     Print one machine-readable result line on stdout
                             instead of the human message (same exit codes; a
                             marketplace-declared command is still shown and
                             must be confirmed — pass -y when not interactive)
  --registry <url>           For a <package>@npm install: resolve and download
                             from this npm registry instead of the one your npm
                             configuration selects
  -s, --scope <scope>        Installation scope: user, project, or local
                             (default: "user")
  -y, --yes                  Accept the displayed marketplace-declared command
                             without the confirmation prompt — a plugin
                             installed by running a command, or one whose
                             archive is fetched through a headersHelper command
                             (required when stdin or stdout is not a TTY)
[exit 0]

=== claude plugin uninstall --help
Usage: claude plugin uninstall|remove [options] <plugin>

Uninstall an installed plugin

Options:
  -h, --help           Display help for command
  --json               Print one machine-readable result line on stdout instead
                       of the human message (same exit codes; not with --prune)
  --keep-data          Preserve the plugin's persistent data directory
                       (~/.claude/plugins/data/{id}/)
  --prune              Also remove auto-installed dependencies that are no
                       longer needed (requires -y in non-interactive contexts)
  -s, --scope <scope>  Uninstall from scope: user, project, or local (default:
                       "user")
  -y, --yes            Skip the --prune confirmation prompt (required when stdin
                       or stdout is not a TTY)
[exit 0]

=== claude plugin enable --help
Usage: claude plugin enable [options] <plugin>

Enable a disabled plugin

Options:
  -h, --help           Display help for command
  --json               Print one machine-readable result line on stdout instead
                       of the human message (same exit codes)
  -s, --scope <scope>  Installation scope: user, project, local (default:
                       auto-detect)
[exit 0]

=== claude plugin disable --help
Usage: claude plugin disable [options] [plugin]

Disable an enabled plugin

Options:
  -a, --all            Disable all enabled plugins
  -h, --help           Display help for command
  --json               Print one machine-readable result line on stdout instead
                       of the human message (same exit codes)
  -s, --scope <scope>  Installation scope: user, project, local (default:
                       auto-detect)
[exit 0]

=== claude plugin update --help
Usage: claude plugin update [options] <plugin>

Update a plugin to the latest version (restart required to apply)

Options:
  --accept-command <sha256>  Accept the marketplace-declared command (a
                             command-source install, or the headersHelper that
                             fetches the archive) whose sha256 a previous --json
                             run reported as shownCommand.sha256; counts as -y
                             for exactly that command, for that plugin and
                             marketplace catalog, and nothing else. If either
                             changed (a refresh that moved the catalog counts),
                             the run refuses and reports the command again, to
                             be shown to a person again
  -h, --help                 Display help for command
  --json                     Print one machine-readable result line on stdout
                             instead of the human message (same exit codes; a
                             marketplace-declared command is still shown and
                             must be confirmed — pass -y when not interactive)
  -s, --scope <scope>        Installation scope: user, project, local, managed
                             (default: auto-detect)
  -y, --yes                  Accept the displayed marketplace-declared command
                             without the confirmation prompt — a changed install
                             command, or the headersHelper command that fetches
                             its archive (required when stdin or stdout is not a
                             TTY)
[exit 0]

=== claude plugin details --help
Usage: claude plugin details [options] <name>

Show a plugin's component inventory and projected token cost

Options:
  -h, --help  Display help for command
[exit 0]

=== claude plugin validate --help
Usage: claude plugin validate [options] <path>

Validate a plugin or marketplace manifest, or the skills, agents, and commands
in a directory

Options:
  -h, --help  Display help for command
  --json      Output the validation report as JSON (same exit codes)
  --strict    Treat warnings as errors (exit 1). Use in CI to fail on
              unrecognized fields, missing metadata, and other issues that the
              runtime tolerates.
[exit 0]

=== claude plugin init --help
Usage: claude plugin init|new [options] <name>

Scaffold a new plugin at ~/.claude/skills/<name>/ (auto-loads next session as
<name>@skills-dir)

Options:
  --author <name>         Author name (default: git config user.name)
  --author-email <email>  Author email (default: git config user.email)
  --description <text>    Manifest description
  -f, --force             Overwrite an existing .claude-plugin/ at the target
  -h, --help              Display help for command
  --with <components...>  Also scaffold: skills, agents, hooks, mcp, lsp,
                          output-style, channel
[exit 0]

=== claude plugin configure --help
Usage: claude plugin configure [options] <plugin>

Show a plugin's options and which are unset, or save values from stdin with
--values-stdin

Options:
  -h, --help      Display help for command
  --json          Output as JSON
  --values-stdin  Read option values from stdin as a JSON object of single-line
                  strings; options left out keep their values
[exit 0]

=== claude plugin prune --help
Usage: claude plugin prune|autoremove [options]

Remove auto-installed dependencies that are no longer needed

Options:
  --dry-run            List what would be removed without removing
  -h, --help           Display help for command
  -s, --scope <scope>  Prune at scope: user, project, or local (default: "user")
  -y, --yes            Skip the confirmation prompt (required when stdin or
                       stdout is not a TTY)
[exit 0]

=== claude plugin tag --help
Usage: claude plugin tag [options] [path]

Create a {name}--v{version} git tag for a plugin release, validating that
plugin.json and any enclosing marketplace entry agree

Options:
  --dry-run            Print what would be tagged without creating it
  -f, --force          Skip the dirty-working-tree and tag-already-exists checks
  -h, --help           Display help for command
  -m, --message <msg>  Tag annotation message (use %s for the version)
  --push               Push the tag to --remote after creating it
  --remote <name>      Remote to push to with --push (default: "origin")
[exit 0]

=== claude plugin test --help
claude plugin test: hooks modules are not turned on in this build yet (early access); set CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 to run a plugin's tests
[exit 1]

=== claude plugin eval --help
Usage: claude plugin eval [options] [command] [target]

Run eval cases (<eval dir>/**/case.yaml or prompt.md + graders/*.md; the eval
dir is evals/ unless --eval-dir or the manifest says otherwise) against a plugin
and report scored results. Target is a path, a plugin name, or a
`plugin@marketplace` id — installed and skills-dir plugins both resolve (and add
a no-plugin baseline arm). It loads the plugin and runs its eval suite (prompts,
graders; scaffold scripts and real MCP servers only when you opt in) on your
machine, as you: only evaluate plugins you trust — the run's sandboxing limits
what a malicious plugin can reach but is not a guarantee, and a bundled suite
passing is not a security vetting. The first run in an untrusted plugin
directory asks you to confirm (--trust-plugin answers for CI)

Options:
  --ablation <mode>         Run a no-plugin baseline arm and report the score
                            delta (none | with-without; default: with-without
                            whenever a plugin resolves — by name, or from the
                            target path — and none when nothing does; under
                            with-without, graders marked with-only, incl.
                            `tool_used: Skill`, are a plugin-fired indicator
                            rather than part of the score)
  --allow-real-servers      With --mocks record: also start the plugin's REAL
                            MCP server processes for servers that have no mock
                            (they run as you, outside the OS sandbox that
                            confines shell tools; use only on plugins you trust)
  --allow-tools <tools...>  Operator grant for gated tools (Bash, Write, Edit,
                            WebFetch, mcp__*). Supports Tool(pattern:*) syntax
  --case <glob>             Filter cases by name glob
  -j, --concurrency <n>     Run up to <n> agent runs at once (1-8; default 1).
                            Each run is a full claude child on your own
                            credential, so they share one rate limit; results
                            and the report keep case order
  --eval-dir <dir>          Directory name (below the plugin) that holds the
                            eval cases; results go to <plugin>/<dir>/results/ —
                            for an installed-plugin target, ./<dir>/results/
                            with this flag, else ./evals/results/ (default dir:
                            the manifest's experimental.evals value, else
                            evals/)
  -h, --help                Display help for command
  --json [path]             Print the full run result (prompts, graders, per-run
                            scores) as JSON to stdout, or write it to this .json
                            file
  --judge-model <model>     Override LLM-grader model (default: haiku)
  --keep-temp               Preserve scaffold dirs for debugging
  --max-cost-usd <usd>      Optional hard cost ceiling; abort and report partial
                            results if hit (exit 2). The ceiling is checked
                            before each run launches, so overrun is bounded to
                            the runs in flight (one, or up to --concurrency) —
                            when a run breaches, paid graders (llm/baseline) are
                            skipped while free graders still score it. Runs are
                            already bounded by max_turns and timeout_seconds —
                            only set this when you need a strict budget
  --mocks <mode>            Mock stand-ins for MCP servers, from <eval
                            dir>/mocks/ (record | off; default: record). record:
                            a plugin server with no mock is NOT started (see
                            --allow-real-servers); off: no stand-ins, every real
                            server starts (as you, outside the OS sandbox), its
                            tools gated by --allow-tools
  --model <model>           Override model for all cases
  --no-publish              Keep the HTML report local only; skip publishing it
                            to claude.ai
  --no-scaffold             Explicitly skip scaffold_script
  --output-dir <dir>        Directory for aggregate-result.json (default:
                            ./<eval dir>/results/<timestamp>/)
  --publish-report          Also require publishing the report to claude.ai
                            (already the default when your account supports it);
                            explains why if unavailable
  --report <path>           Write the self-contained HTML report (scores,
                            prompts, grader verdicts) to <path> instead of the
                            results dir
  --runs <n>                Override per-case runs (default: case.runs ?? 3)
  --scaffold                Run each case's scaffold_script (runs
                            author-supplied bash as you; off by default — only
                            use on case files you authored)
  --tag <tag...>            Filter cases by tag (repeatable)
  --threshold <0..1>        Exit 1 if any case score is below this threshold
                            (default: 1.0)
  --trust-plugin            Assert that you trust this plugin's code and eval
                            suite, and skip the first-run trust prompt (for CI;
                            like --dangerously-skip-permissions, only pass it
                            for plugins you would run yourself). Does not imply
                            --scaffold, --allow-tools or --mocks off
  --verbose                 Log per-message trace events to the debug log (use
                            --debug-file to read them)

Commands:
  init [options] [name]     Author an eval suite under the eval dir (evals/
                            unless --eval-dir or the manifest says otherwise)
                            via an interview that sources inputs and designs
                            graders. Use --bare <name> for a blank single-case
                            template.
[exit 0]

=== claude plugin marketplace --help
Usage: claude plugin marketplace [options] [command]

Manage Claude Code marketplaces

Options:
  -h, --help                  Display help for command

Commands:
  add [options] <source>      Add a marketplace from a URL, path, or GitHub repo
  help [command]              display help for command
  list [options]              List all configured marketplaces
  remove|rm [options] <name>  Remove a configured marketplace
  update [options] [name]     Update marketplace(s) from their source - updates
                              all if no name specified
[exit 0]

=== claude plugin marketplace add --help
Usage: claude plugin marketplace add [options] <source>

Add a marketplace from a URL, path, or GitHub repo

Options:
  --claudeai           Add the marketplace of this name that claude.ai hosts for
                       you, by its listed name or its local name (see: claude
                       plugin marketplace list)
  -h, --help           Display help for command
  --scope <scope>      Where to declare the marketplace: user (default),
                       project, or local
  --sparse <paths...>  Limit checkout to specific directories via git
                       sparse-checkout (for monorepos). Example: --sparse
                       .claude-plugin plugins
[exit 0]

=== claude plugin marketplace list --help
Usage: claude plugin marketplace list [options]

List all configured marketplaces

Options:
  -h, --help  Display help for command
  --json      Output as JSON
[exit 0]

=== claude plugin marketplace remove --help
Usage: claude plugin marketplace remove|rm [options] <name>

Remove a configured marketplace

Options:
  -h, --help       Display help for command
  --scope <scope>  Remove the marketplace declaration from a specific settings
                   scope: user, project, or local. Omit to remove it from every
                   scope.
[exit 0]

=== claude plugin marketplace update --help
Usage: claude plugin marketplace update [options] [name]

Update marketplace(s) from their source - updates all if no name specified

Options:
  -h, --help  Display help for command
[exit 0]
```

