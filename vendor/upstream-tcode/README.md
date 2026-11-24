<div align="center">

<img src="assets/icons/app/tcode.png" width="88" alt="Tcode app icon">

# Tcode

**A native GUI for the terminal coding agents you already use.**

Claude Code, Codex, pi, OpenCode, Cursor, Grok, and any agent that speaks ACP — one window,
on your desktop, your phone, or in a browser.

[Download](https://github.com/Tryanks/tcode/releases) ·
[Getting started](#getting-started) ·
[Contributing](CONTRIBUTING.md)

[![CI](https://github.com/Tryanks/tcode/actions/workflows/ci.yml/badge.svg)](https://github.com/Tryanks/tcode/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

<img src="docs/images/chat-light.png" width="840" alt="Tcode chat view">

</div>

## What it is

Tcode is a native GUI proxy for the agent CLIs already installed on your
machine. One Tcode is the *host*: it spawns the agents, speaks their native
protocols, and keeps your threads, terminals and projects. Every other device —
another desktop, a phone, a tablet, a browser tab — is a screen for that host.

Tcode surfaces what each agent's own CLI can do: persistent threads, rendered
diffs, a readable approval panel, and native provider actions when the
underlying CLI exposes them. It does **not** replace your agent, proxy your API
keys, or invent capabilities the agent lacks. Your accounts, subscriptions,
models and tooling keep working exactly as they do today — Tcode just drives
them, and anything it adds to what the model sees is shown to you.

## What you get

**Threads that persist.** Every conversation is an event log on disk, grouped by
project. Close the app, reopen it, keep talking — the agent resumes where it left
off.

**Provider-native rewind.** Claude Code sessions expose Claude's own checkpoint
options for restoring code, conversation, or both. Tcode forwards those native
operations and records the confirmed result; it does not snapshot your Git
working tree or synthesize rollback for providers that lack the capability.

**Diffs, not scrollback.** Syntax-highlighted per-turn diffs in a resizable
split, with a changed-files card on each turn.

**Approvals you can read.** Command execution and file edits surface as a panel
showing the actual command and the actual diff — approve, allow for the session,
or deny. Permission modes run from "ask about everything" to "don't ask".

**Queue and steer.** The composer stays live while a turn runs.
<kbd>Enter</kbd> queues your message and sends it when the turn finishes;
<kbd>⌘</kbd><kbd>Enter</kbd> on macOS or <kbd>Ctrl</kbd><kbd>Enter</kbd> on
Windows/Linux steers when the provider supports it, injecting the message into
the turn in flight. Providers without steering support queue it instead.
Queued messages show above the composer and offer a steer action where supported.

**A terminal, a browser, and a plan.** Per-thread terminal drawer (select output,
send it as context), an embedded preview browser the agent can drive over MCP,
and a live plan/task panel.

<div align="center">
<img src="docs/images/diff.png" width="49%" alt="Diff panel">
<img src="docs/images/queue.png" width="49%" alt="Queued messages above the composer">
</div>

## Supported agents

**Native integrations** — the deepest support, over each CLI's own protocol:

| Agent | Requirement |
| --- | --- |
| [Claude Code](https://claude.com/claude-code) | `claude` on your `PATH` |
| [Codex](https://developers.openai.com/codex/cli) | `codex` on your `PATH` |
| [pi](https://github.com/earendil-works/pi) | `pi` on your `PATH` |
| [OpenCode](https://opencode.ai) | `opencode` on your `PATH` |
| [Cursor](https://cursor.com/cli) | `cursor-agent` on your `PATH` |
| [Grok](https://x.ai) | `grok` on your `PATH` |

**Everything else, over [ACP](https://agentclientprotocol.com).** Tcode ships a
marketplace backed by the official ACP registry.
Install one from **Settings → Providers**, or point Tcode at any command that
speaks ACP.

<div align="center">
<img src="docs/images/acp-marketplace.png" width="720" alt="ACP agent marketplace in Settings → Providers">
</div>

> ACP entries that duplicate a native integration are deliberately hidden from
> the marketplace so each CLI has one clear, highest-fidelity path.

## Use Tcode from other devices

Tcode runs on the machine that holds your projects, starts your agents, and
keeps your threads. You can open that machine from another desktop, a phone or
tablet, or a browser tab. Native apps connect over **Traverse**: one
end-to-end encrypted connection per device, direct when the network allows it
and through a relay when it does not. Traverse is the relay and lookup service
a machine publishes to; it has no accounts, sees only encrypted traffic and
machine and device ids, and is never part of authentication — the machine
itself decides which devices may connect. It is on by default, can point at a
self-hosted `tcode-traverse` instance, or can be turned off, in which case
devices reach the machine only on its own network: through its DNS-SD
advertisement on the LAN or at an address typed when nothing finds it, and,
once paired, at the addresses that worked before. The default instance
uses n0's public iroh relays and lookup service, which n0 describes as
rate-limited with no uptime guarantee.

Every device shows the *same* app. There is no reduced phone build: a phone, or
a tablet narrower than 900px, shows a machines → threads → thread stack; a
wider tablet and every desktop window use the split layout. Some actions depend
on the device in your hands, such as opening a native file dialog or driving the
embedded preview browser.

**Use your desktop as the machine.** In **Settings → Remote**, turn on
**Let other devices connect to this machine**. The sidebar's **Machines** page
then shows the invitation: scan its QR code with the device you want to
connect, or send it the invitation link. An invitation admits one device and
expires after five minutes; the link is the secret, so share it only with that
device.

**Use a server as the machine.** Download `tcode-headless` from
[Releases](https://github.com/Tryanks/tcode/releases), install your agent CLIs
on the server, then:

```sh
tcode-headless serve --name build-server   # prints the machine id, invitation link and QR
tcode-headless pair      # reprints the current invitation while it is valid
```

For a NAS or homelab, [Docker images and a Compose template](docs/docker.md)
provide Linux amd64/arm64 deployment with selectable CLIs and persistent logins.

`serve --traverse off` or `--traverse https://your.instance` changes the
Traverse mode. Headless release builds also serve the browser app, on
`http://127.0.0.1:47420/` by default; `--browser-listen` binds it to a LAN
address. Set a password on first open, or preset it with `TCODE_PASSWORD`.

**Connect another device.** Open the sidebar's **Machines** row on a desktop,
or the opening screen on a phone. Scan the machine's QR code or paste its
invitation link; pairing connects at once. In a browser, open the printed HTTP
link and log in with the password; the browser is for direct access on the
machine or its LAN and does not use Traverse. Its **Settings → Remote** shows
the machine's invitation for native clients and controls pairing and
revocation.
See [Use Tcode from other devices](docs/remote.md) for the full guide.

[Shared spaces](docs/remote.md#shared-spaces) let trusted collaborators pair
with a reusable link and work in selected projects on your machine. Members
run commands with the machine's agents and permissions; spaces scope Tcode
visibility and control, without isolating commands.

**Security.** Native connections are authenticated by device and machine keys
and encrypted end to end; a relay carries ciphertext. Removing a device on the
machine closes its connection at once. The browser page is plain HTTP,
protected by the password; keep it on loopback or a trusted LAN. See
[Security](docs/remote.md#security) and
[Self-hosting Traverse](docs/remote.md#self-hosting-traverse) in the remote
guide.

## Getting started

**1. Install Tcode.** Download a build for your platform from
[Releases](https://github.com/Tryanks/tcode/releases) — macOS 27+ (Apple
silicon), Windows 11+ (x64 / ARM64), Linux (x64 / ARM64) — and run it. Check the
release notes for runtime requirements and signing status. Linux needs the listed
system libraries and a Vulkan driver.

For an unsigned macOS build, remove quarantine after installing the app with
`xattr -dr com.apple.quarantine /Applications/Tcode.app`. The embedded preview
browser is available on macOS, Windows, Android, iOS and Linux under X11 (on a
Wayland session, start Tcode with `WAYLAND_DISPLAY` unset to run it on
XWayland); the browser client opens dev servers in a new tab instead.

Each release uses the native application icon format for its platform: `.icns`
inside the macOS app bundle, an `.ico` resource embedded directly in the Windows
executable, and an XDG desktop entry plus themed PNG on Linux. Release downloads
also include a `SHA256SUMS.txt` file.

| Platform / device | Release download |
| --- | --- |
| macOS, arm64 / x64 | Desktop `.zip` / `.dmg`; headless `.zip` |
| Windows, x64 / arm64 | Desktop or headless `.zip` |
| Linux, x64 / arm64 | Desktop or headless `.tar.gz` |
| Android, arm64 | `tcode-<version>-android-arm64.apk` — signed with the project's release key; install with adb |
| iOS / iPadOS 27+, arm64 | `tcode-<version>-ios-arm64-unsigned.ipa` — unsigned; re-sign with your own Apple ID or install with a sideloading tool such as AltStore or Sideloadly |
| Browser | Embedded in the headless release; open the HTTP link it prints (HTTPS only through your own tunnel). No separate signed app package |

**2. Have an agent installed.** Tcode drives the CLIs, it doesn't bundle them.
Make sure `claude`, `codex`, `pi`, `opencode`, `cursor-agent` or `grok` is on your `PATH` — or install an ACP agent from
the marketplace once Tcode is running.

**3. Add a project and start a thread.** Point Tcode at a directory, type, send.
No API keys, no config file required. Optional [project configuration](docs/project-config.md)
can provide a shared project icon.

The interface is localized and follows your system language; you can override it
in Settings. Everything Tcode stores — sessions, settings, installed ACP agents —
lives in `~/.tcode` on every platform. A data directory an earlier version kept
in your platform's app-data directory moves there on the first start.

<div align="center">
<img src="docs/images/chat-dark.png" width="840" alt="Tcode in dark mode">
</div>

## Building from source

Build instructions, platform prerequisites, workspace layout, tests and provider
probes are in [CONTRIBUTING.md](CONTRIBUTING.md). To review the compact layout
without a device, open the shared shell at phone geometry:

```sh
cargo run -p tcode-ui --example phone              # 393×852
cargo run -p tcode-ui --example phone -- --android # 412×915
```

The editable macOS 26 source is
[`assets/icons/app/tcode.icon`](assets/icons/app/tcode.icon). Icon Composer's
official 16-bit Display P3 render is committed as
[`assets/icons/app/tcode.png`](assets/icons/app/tcode.png), then converted into
the native macOS and Windows icon formats used by releases.

## Contributing

Issues and pull requests are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers
how to build, test, and what to expect from review; participation is governed by
the [Code of Conduct](CODE_OF_CONDUCT.md).

Good places to start: a bug you hit, a rough edge in the UI, or a new ACP agent
that doesn't render well.

## Acknowledgements

Tcode's interaction model was inspired by **[T3 Code](https://t3.gg)** by
T3 Tools, and Tcode keeps borrowing good ideas from it: it reads T3 Code's
`t3.json` project configuration and its Claude model manifest.

Built with [GPUI](https://gpui.rs) and
[gpui-kit](https://github.com/longbridge/gpui-kit).

## License

[MIT](LICENSE)
