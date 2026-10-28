# Changelog

All notable public release changes are tracked here.

## 0.4.7 - 2026-10-28

### Fixed

- Fixed Flag and answer copying on LAN and other ordinary HTTP pages, where `navigator.clipboard` can be unavailable. The run inspector, conversation conclusion, status banner, and expanded result list now use the same copy implementation.
- Attempted synchronous copying within the click or key gesture, then used the Clipboard API when needed. Success and failure messages reflect the browser's result in Chinese or English.
- Updated Next.js and its matching lint configuration to `15.5.28`, including patched image-processing, source-map, and development dependencies identified during release checks.
- Updated the ZCode source-build toolchain to pnpm 10.34.9, which fixes the hoisted-linker race that could abort Worker image builds while installing `esbuild`.
- Delayed publishing versioned and `latest` image indexes until the complete release image and desktop build set succeeds.

### Changed

- Updated application versions, lock metadata, Compose image tags, Worker build defaults, and installation examples to `0.4.7` / `v0.4.7`.
- Expanded the `0.4.6` notes below with the features, design decisions, fixes, and implementation areas included since `0.4.5`.

### Validation

- Reproduced the original error on a LAN HTTP page. Single-click copies from the inspector, status banner, and conversation conclusion wrote the expected text to the system clipboard.
- Verified Enter, Space, and touch at a 390px viewport; also verified the secure-context Clipboard API fallback and failure feedback when both copy methods were unavailable.
- TypeScript passed. ESLint reported no errors and 33 existing warnings. The affected Windows deployment was not retested directly.
- The dependency audit reported no critical or high findings after the update. Three low-severity findings remain in the math-rendering dependency chain; the suggested change crosses a major-version boundary.

### Upgrade

- Use `muteki upgrade v0.4.7` or `./run.sh upgrade v0.4.7` for a managed Web installation; add `--compose` for a container deployment. Existing `.env`, `state/`, credentials, and sessions remain outside the release bundle.
- Rebuild or replace the deployed Web frontend and reload the page to receive the copy fix. Updating a source checkout does not replace an already running production build.
- Desktop packages target Apple silicon and Intel macOS. Windows and Linux desktop installers are not included; Web and container deployments remain available.
- macOS packages are ad-hoc signed and are not notarized. The in-app candidate updater requires a stable Developer ID signature; manual installation remains available.

[Patch comparison](https://github.com/FishCodeTech/muteki/compare/v0.4.6...v0.4.7) · [All changes since 0.4.5](https://github.com/FishCodeTech/muteki/compare/v0.4.5...v0.4.7)

## 0.4.6 - 2026-10-28

This section covers the implementation interval after `0.4.5`, including changes grouped into the public release snapshot.

### Conversations and subagents

- Added cross-provider subagent delegation and Orchestrate controls. Parent and child conversations, the sidebar, and the Subagents panel show progress, results, elapsed time, routing details, navigation, and cancellation for application-managed agents.
- Recorded runtime, model, reasoning effort, acceptance criteria, routing rationale, and worktree state. Provider-native and application-managed subagents remain distinguishable; Orchestrate follows the capabilities bound to the current thread.
- Preserved native sessions and structured handoff history when changing runtimes. Fixed retry and project-admission boundaries, sidebar ordering, and Codex MCP/authentication status labels.
- Refined streamed tool activity, command output, file differences, and research sources. Reduced-motion preferences remain supported.
- Separated Pi effort and speed controls, retained supported choices, hid unavailable plan controls, and reduced composer layout movement.

### Activity, evidence, and run views

- Replaced the run trace waterfall with an activity timeline, event ledger, and history replay. Hierarchy, parallel work, duration, errors, token usage, and cost are visible across agent, model, and tool activity.
- Added search, category and error filters, interval selection, zoom, live following, alternate layouts, and narrow-screen stacking. Compressed idle periods retain their actual timestamp mapping.
- Kept tool inputs and outputs in expanded details. Collaboration views show subgoals, step results, evidence references, retired facts, and replacement links; exports are available as JSONL, text timeline, and SQLite.
- Improved CTF and penetration-testing typography, contrast, wrapping, charts, model lists, empty states, statistics, provider error details, and completed Worker presentation.

### Pricing and usage

- Centralized model pricing, model mappings, custom rates, and missing-model entries in Agent settings. Conversations, runs, orchestration, and global usage views use shared accounting.
- Distinguished provider-reported amounts, estimates, partial records, subscription pricing, and missing prices. Standard and fast service-tier rates remain separate, and model matching preserves pricing scope.
- Made credential draft probes honor the selected local or container Worker backend.

### SRC workspace and research records

- Added a task-bound SRC workspace with conversations, search, assets, findings, evidence, reports, research material, and budgeted monitoring.
- Connected the real workspace to persistent backend domains for assets, findings, tasks, reports, and suggestions. Mentions, references, drag-to-composer, save state, and pending proposals connect conversations with structured records.
- Added source-aware asset ingestion, relationships, revisions, selected reuse, artifact preservation, request and protocol evidence, review states, frozen reports, exports, feedback, and retest records.
- Added policy and approval records, task-bound browser and identity context, and controlled access to retained evidence. Fixed scope checks, HTTP authority handling, and evidence integrity.
- Added versioned monitoring definitions, baselines, candidate review, notifications, and bounded cycles. Corrected run-once acquisition, revision checks, and cost reconciliation.
- Kept independent prototypes and demonstration routes identifiable. Some prototype interactions remain local; planned external integrations and operating environments require their own acceptance.

### Backend reliability and deployment

- Moved blocking project operations off the server event loop, serialized extension lifecycle changes by extension, and atomically reserved concurrent upload filenames.
- Moved large Worker prompts out of process arguments while retaining full prompt text and session continuation, avoiding operating-system argument-size limits.
- Improved scheduling and shared-graph provenance, fact replacement, lifecycle state, deadline handling, and the distinction between provider failures, timeouts, cancellations, and output limits.
- Updated Pi RPC controls, runtime adapters, Worker readiness, packaged dependencies, and desktop runtime resources. Fixed Ubuntu systemd installation and service validation.
- Cleaned up stray root components, stopped tracking local prototype material, and kept generated contracts and existing checks aligned with the interfaces.

### Desktop and navigation

- Retained workspace pages, mode-specific routes, drafts, and reading positions when switching contexts. Hidden desktop pages reduce refresh work, and panel handoffs restore canonical routes and history navigation.
- Consolidated chat and SRC controls in the desktop title bar, hid thread controls in other modes, simplified workspace navigation, and removed duplicate remote-page chrome.
- Added a conversation turn rail with previews and jump navigation, bounded around the current reading position for long threads.
- Designed activity, pricing, provenance, and capability-dependent controls to show recorded state while keeping conversation and structured records connected.

### Implementation map

| Area | Main source locations |
| --- | --- |
| Conversations and runtime adapters | `muteki/conversation/`, `muteki/external_agents/`, `apps/web/conversation_api.py`, `apps/web/ui/components/conversation/` |
| Activity and collaboration | `apps/web/ui/components/activity-timeline/`, `apps/web/ui/components/collaboration/`, `muteki/swarm/` |
| Pricing and accounting | `muteki/core/model_pricing.py`, `apps/web/ui/lib/modelPricing.ts`, `apps/web/ui/components/settings/agents/`, `apps/web/ui/components/UsageDashboard.tsx` |
| SRC records, evidence, and monitoring | `muteki/src/`, `apps/web/ui/components/src/`, `apps/web/ui/components/studio-prototype/real/`, `docker/src-browser/`, `docker/src-egress/` |
| Desktop and navigation | `apps/desktop/src/`, `apps/desktop/renderer/`, `apps/web/ui/components/RetainedWorkspaces.tsx`, `apps/web/ui/components/WorkspaceNav.tsx` |
| Worker transport and packaging | `muteki/solver/`, `docker/worker/`, `docker/worker-slim/`, `scripts/build_desktop_runtime.py`, `scripts/verify_worker_image.sh` |
| HTTP copy fix in 0.4.7 | `apps/web/ui/lib/clipboard.ts`, `apps/web/ui/lib/useCopyText.ts`, `apps/web/ui/components/Conversation.tsx`, `apps/web/ui/components/RunInspector.tsx` |

### Upgrade and verification scope

- Use `muteki upgrade v0.4.6` or `./run.sh upgrade v0.4.6` for this release, or install `0.4.7` to include the HTTP copy fix. Container installations can add `--compose`; application state and credentials remain outside the release archive.
- The `0.4.6` release workflow passed application-bundle creation, native amd64/arm64 image checks, and Apple silicon/Intel macOS packaging and startup checks. External-provider and SRC operational acceptance depend on the configured environment.
- The default monitoring runner supports configured asset sources with query capabilities; other source types need a suitable runner. Report export and recorded manual status do not submit to external platforms automatically.
- Windows and Linux desktop installers are not included. macOS packages are ad-hoc signed and are not notarized.

[Release validation](https://github.com/FishCodeTech/muteki/actions/runs/40639311562) · [Full code comparison](https://github.com/FishCodeTech/muteki/compare/v0.4.5...v0.4.6)

## 0.4.5 - 2026-10-12

### Fixed

- Bundled Node and Pi in the Web API image so containerized CTF and penetration-testing Reason calls can start and generate summaries without `FileNotFoundError` ([#205](https://github.com/FishCodeTech/muteki/issues/205)).
- Checked browser and Chromium executability before advertising Worker browser Skills, including Slim images, and kept transient Docker failures out of the capability cache.
- Included Droid in the shared Worker CLI capability mappings and added native amd64/arm64 Web and Worker runtime checks before publishing image indexes.
- Generated Cursor SDK conversation titles and summaries through the selected SDK runtime and account. Temporary metadata sessions now clean up their resources without appearing in native conversation history.
- Corrected OpenCode streamed text replacement after reconnects.

### Added

- Added the unauthorized-use disclaimer and responsibility notice in [DISCLAIMER.md](DISCLAIMER.md).

### Upgrade

- Use `muteki upgrade v0.4.5` or `./run.sh upgrade v0.4.5` for a managed Web installation, and add `--compose` for a container deployment. Existing `.env`, `state/`, credentials, and sessions remain outside the release bundle.
- Download the macOS `.dmg` or `.zip` matching your Mac from the release page. Windows and Linux desktop packages are not included.

## 0.4.4 - 2026-10-12

### Fixed

- Included built-in Worker Skills in macOS desktop packages. Local Workers can now prepare their Blackboard and browser resources without failing on a missing `SKILL.md`.
- Added packaged-runtime checks for ordinary, autonomous, and penetration-testing Worker Skill preparation before desktop assets are published.

### Upgrade

- Use `muteki upgrade v0.4.4` or `./run.sh upgrade v0.4.4` for a managed Web installation, and add `--compose` for a container deployment. Existing `.env`, `state/`, credentials, and sessions remain outside the release bundle.
- Download the macOS `.dmg` or `.zip` matching your Mac from the release page.

## 0.4.3 - 2026-10-11

### Fixed

- Fixed Worker settings being blocked by missing model tests when switching execution environments. Model tests now use the current environment selection before the settings are saved.
- Preserved successful credential tests when saving unchanged credentials or updating model selections. Changes to credentials or endpoints still invalidate earlier test results.
- Preserved complete model-test commands and output, including the actual model response and provider failure reasons.
- Allowed desktop connections to remote services over HTTP as well as HTTPS, including services running in local virtual machines.
- Fixed resumed and continued conversations failing while recording context delivery.
- Fixed previews of images generated inside a conversation, including conversations without a project and SVG images.
- Fixed disabled Workers with system-login credentials blocking a switch to container execution.
- Fixed desktop task-retention settings requests, preserved zero-valued task ordering, and corrected semantic version comparisons during updates.

### Changed

- Improved conversation context controls, attachments, native Agent environment reuse, and provider integration.
- Updated Worker images and installation support for the current Agent integrations.

### Upgrade

- Use `muteki upgrade v0.4.3` or `./run.sh upgrade v0.4.3` for a managed Web installation, and add `--compose` for a container deployment. Existing `.env`, `state/`, credentials, and sessions remain outside the release bundle.
- Download the macOS `.dmg` or `.zip` matching your Mac from the release page.

## 0.4.2 - 2026-10-09

### Fixed

- Restored installed Agent CLI and native login discovery in regular desktop workspaces, including user-local, NVM, and Bun installations. Candidate startup checks retain isolated homes and credentials.
- Fixed desktop candidate identity checks to hash the physical ASAR archive and report recovery failures clearly.
- Reused the Agent credential view and fixed Switch CSS and narrow cards in the settings interface.

### Changed

- Moved version updates out of the CTF and penetration-testing settings into global settings. Existing update links redirect to the new page.

### Added

- Added global desktop update settings with official macOS release checks, GitHub SHA-256 verification, strict signature checks, isolated candidate startup, data backups, and restart-based app replacement. Service updates remain available in Web settings.
- Added separate CTF and penetration-testing retention settings with saved configuration and independent archive and delete switches. Defaults archive after 15 idle days and delete after 30 idle days. Idle time starts at the run's last activity, and deletion applies only to already archived tasks.

### Upgrade

- Use `muteki upgrade v0.4.2` or `./run.sh upgrade v0.4.2` for a managed Web installation, and add `--compose` for a container deployment. Existing `.env`, `state/`, credentials, and sessions remain outside the release bundle.
- Download the macOS `.dmg` or `.zip` matching your Mac from the release page. Windows and Linux desktop packages are not included.
- Retention thresholds now come from each mode's saved settings. `MUTEKI_ARCHIVE_DAYS` and `MUTEKI_DELETE_DAYS` no longer set these thresholds; `MUTEKI_RETENTION_ENABLED=0` still disables all automatic retention actions.
- Published 0.4.1 desktop installs need one manual install of 0.4.2 because 0.4.1 has no in-app update screen. Once on 0.4.2, official updates are available in global settings and verify the release SHA-256 and strict signatures before preparing a candidate.
- Local candidate updates require a stable Developer ID signature. The published macOS packages remain ad-hoc signed and are not notarized. There is no unsigned update bypass.

## 0.4.1 - 2026-10-09

### Added

- Added macOS desktop packages for Apple silicon and Intel, with an isolated local workspace, bundled Python/Node/Web runtime, and remote service connections.
- Added shared settings for Web and desktop, including Agent accounts, model visibility, permissions, keyboard shortcuts, notifications, and appearance preferences.
- Added provider usage and limit views, a usage ledger, notification inbox, conversation checkpoints, subagent activity, and scoped browser and computer-control tools.

### Changed

- Updated the CTF and penetration-testing workspaces with shared navigation, Worker extensions, evidence-backed reports, and clearer report printing and export.
- Improved Agent compatibility, model discovery, session recovery, approvals, and process cleanup across supported runtimes.
- Refined conversation navigation, drag-and-drop ordering, message rendering, previews, and context controls. The default Web view continues to show CTF and penetration testing; conversation and competition workspaces remain optional.
- Release versions and default image tags now use `0.4.1` / `v0.4.1`. Container images continue to support Linux amd64 and arm64; desktop packages in this release are macOS only.

### Fixed

- Fixed the Web interface freezing while deleting an Agent credential and detaching its conversation and Worker references, as reported with Ubuntu container deployments ([#199](https://github.com/FishCodeTech/muteki/issues/199)).
- Fixed desktop notification ownership, permission persistence, foreground read tracking, and controlled overlay interactions.
- Fixed Worker settings navigation, stale draft runtime panels, local directory error feedback, and findings counts for older penetration-testing reports.
- Kept a compatible ONNX Runtime version for Intel macOS so the bundled Python environment installs and loads Magika correctly; other platforms retain their existing runtime version.

### Upgrade

- Use `muteki upgrade v0.4.1` or `./run.sh upgrade v0.4.1` for a managed Web installation, and add `--compose` for a container deployment. Existing `.env`, `state/`, credentials, and sessions remain outside the release bundle.
- Download the macOS `.dmg` or `.zip` matching your Mac from the release page. Windows and Linux desktop packages are deferred.
- The macOS packages are ad-hoc signed and are not notarized. Install desktop updates manually; the signed-candidate update flow requires a stable Developer ID signature.

## 0.4.0 - 2026-09-28

### Added

- Added the goal-driven Web workbench with single-task solving and optional conversation, competition, and extension workspaces. The Coordinator schedules heterogeneous Workers over a shared evidence graph with operator controls.
- Added structured Agent runtime and credential-account support for the supported CLI engines, with capability and health views in the Web settings.
- Added the Ubuntu 24.04 single-node installer and release checks for the Web, UI, and Worker images.

### Changed

- The Web UI now opens in single-task mode by default. The Chinese README includes installation paths for macOS, Docker, Ubuntu, and an Ubuntu VM, plus run controls and screenshots.
- Refined the BTW side-query panel and kept its temporary read-only Worker separate from the main solving slots.
- Release packages, Compose defaults, and Worker build defaults use `0.4.0`; Git and image tags use `v0.4.0`.

### Removed

- Removed the Textual TUI, F01–F13 research implementations, evaluation and development scripts, embedded test scripts, local run artifacts, and obsolete reports from the current product tree.
- Removed the optional research Swarm loader and direct-write framework commands from the shipped Web and blackboard paths. Older explicit research `swarm_class` values are no longer supported.

### Upgrade

- The managed updater accepts `muteki upgrade v0.4.0` or `./run.sh upgrade v0.4.0` after the GitHub Release is published. Existing `.env`, `state/`, saved credentials, and sessions remain outside the release bundle.
- The release includes a versioned application archive, SHA-256 manifest, and multi-architecture container images. Container deployments can use `muteki upgrade v0.4.0 --compose`.

## 0.3.2 - 2026-08-22

### Added

- Added explicit post-solve Ask and writeup lifecycles with correlated started, completed, failed, and cancelled events. Finished tasks now show the real Worker response or generated report in the conversation.
- Added durable recovery for interrupted follow-ups. A service restart records an orphaned Ask or writeup as failed, so the finished-task composer does not remain disabled.
- Added ordinary Worker reuse for Review and Verifier roles, including candidate filtering and configuration validation in Worker Settings.

### Changed

- Winner continuation data now lives in Coordinator-private storage. Post-solve Ask, writeup, resolve, and BTW recover the trusted Worker identity, account, backend, session, and validated work directory from that record.
- Worker backend, credential mode, enabled seats, Review, and Verifier settings now save as one validated configuration snapshot.
- Claude local system-login Workers omit `--bare`; injected-credential Workers continue to use isolated authentication.
- Writeup workers use confirmed session evidence, avoid new investigation, and have an explicit five-minute response timeout.
- Package versions are `0.3.2`. Git tags and GHCR image tags keep the leading `v`, for example `v0.3.2`.

### Fixed

- Fixed local/container mode changes failing to save while Review or Verifier referenced a Worker configuration affected by the mode switch.
- Fixed system-login health checks reporting logged-in local Workers as unavailable.
- Fixed Verifier being enabled with an empty Worker list and then becoming impossible to save.
- Fixed completed-task Ask and writeup actions finishing without displaying their output.
- Fixed cancelled, failed, timed-out, or interrupted follow-ups leaving Ask, writeup, resolve, and false-positive controls permanently disabled.

## 0.3.1 - 2026-08-20

### Fixed

- The sidebar folder menu item **Delete folder** could not be clicked. The open menu overlapped the next folder row, and that later row received the click.

### Changed

- Package versions are `0.3.1`. Git tags and GHCR image tags keep the leading `v`, for example `v0.3.1`.

## 0.3.0 - 2026-08-17

### Added

- Added managed install, upgrade, and rollback through `muteki` / `./run.sh`, using a signed GitHub Release bundle and SHA-256 verification.
- Added a versioned Compose deployment (`docker-compose.release.yml`) with `muteki upgrade --compose` / `muteki rollback --compose`.
- Added **Settings → System update** in the web command deck, including compose-mode commands that stay on the host.
- Added `/api/health` for Compose liveness checks.

### Changed

- Package versions are `0.3.0`. Git tags and GHCR image tags keep the leading `v`, for example `v0.3.0`.
- Slim worker image documentation now matches the nine engine CLIs.

### Fixed

- Fixed draft-run attachment upload returning 422 when using the file-picker button: the live `FileList` was cleared before the async upload finished on new solves.

## 0.2.5 - 2026-06-30

### Changed

- Release metadata, package versions, and worker build examples now point at `0.2.5`.

### Fixed

- Resolved container workspace permission mismatches by detecting the worker image's actual `kali` UID/GID before chowning shared run state.

## 0.2.4 - 2026-06-30

### Changed

- Release metadata, package versions, and worker build examples now point at `0.2.4`.

### Fixed

- Fixed Codex custom endpoint dispatch so a settings-page credential account `base_url` is applied to the actual worker profile instead of falling back to OpenAI.
- Made Codex custom endpoint health checks run the real Codex CLI Responses turn, surfacing LiteLLM/DeepSeek schema failures before a run starts.
- Preserved file-backed API key probing for Codex custom endpoints by injecting the resolved key into the CLI health-check environment.

## 0.2.3 - 2026-06-29

### Added

- Added the `/btw` side-query drawer to the web command deck for quick, local multi-turn Q&A over a run.
- Added a worker-backed `/api/runs/{run_id}/btw` stream that starts a short-lived read-only CLI worker for each turn.
- Added deterministic tests for `/btw` prompt construction, transcript handling, read-only graph access, and worker-slot isolation.
- Documented in Worker Settings that `/btw` follows the configured Review worker by default.

### Changed

- `/btw` now reads run files, JSONL, shared graph state, winner snapshots, and artifacts through the worker instead of answering from a compressed summary.
- `/btw` defaults to the configured Review worker when the frontend does not specify a profile, while still allowing explicit API overrides.
- Release metadata, package versions, and worker build examples now point at `0.2.3`.
- Expanded `.env.example` into a fuller operator map covering web auth, compose deployment, worker backends, `/btw` timeouts, credential fallbacks, CLI binary overrides, retention, and internal runtime envs.
- Aligned the default worker image across backend code, Worker Settings, Docker Compose, and docs on `ghcr.io/fishcodetech/muteki-worker:latest`.

### Fixed

- Reduced `/btw` answer distortion by letting the side worker inspect source run evidence directly.
- Kept `/btw` out of swarm scheduling, review concurrency, max-worker slots, graph writes, and run cost accounting.
- Removed the redundant read-only explainer banner from the `/btw` drawer.
- Fixed Docker Compose env passthrough for `MUTEKI_DEEPSEEK_BASE_URL`, `MUTEKI_LLM_TRUST_ENV`, and custom worker network names.

## 0.2.1 - 2026-06-29

### Added

- Added Docker deployment documentation to both English and Chinese READMEs.
- Documented the official GHCR images for the web API, UI, full worker, and slim worker.
- Added guidance for choosing the full worker image versus the slim worker image.

### Changed

- `./run.sh web` is now documented as a production Next.js build/server path rather than a Next dev server.
- The default container worker image now points to `ghcr.io/fishcodetech/muteki-worker:latest`.
- Docker Compose deployment docs now clarify that compose builds the control plane from the checkout but expects the worker image to exist on the host Docker daemon.
- Release/build script examples now use the `ghcr.io/fishcodetech/*` image namespace.

### Fixed

- Fixed GHCR release workflow image tags by lowercasing the registry owner namespace.
- Excluded generated worker build artifacts from public release syncs.
- Passed `MUTEKI_DEEPSEEK_API_KEY` through Docker Compose into the `web-api` container.

## 0.2.0 - 2026-06-29

### Added

- Published the initial public release with GHCR images for the web API, UI, full worker, and slim worker.

### Changed

- Switched the local web command deck runner to production-mode Next.js serving.
- Improved container worker probing and standby behavior so worker checks run in container mode when configured.
