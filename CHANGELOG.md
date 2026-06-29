# Changelog

All notable public release changes are tracked here.

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
