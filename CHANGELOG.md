# Changelog

All notable public release changes are tracked here.

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
