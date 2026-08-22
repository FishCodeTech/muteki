# Changelog

All notable public release changes are tracked here.

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
