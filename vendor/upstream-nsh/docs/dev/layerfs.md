# LayerFS development and CI trial

LayerFS is an opt-in Linux experiment for retaining build outputs in branchable filesystem snapshots. The
existing Cargo commands and platform CI remain the release checks. The trial uses upstream
[LayerFS 1.0.0](https://github.com/Ephemeral-AI-Lab/layerfs/releases/tag/v1.0.0), source commit
`2aaf2552ce21154928b9f9c83582e41a637f5910`; its [SDK guide](https://github.com/Ephemeral-AI-Lab/layerfs/blob/v1.0.0/sdk/js/README.md)
and [platform limits](https://layerfs.ai/docs/platform/) define its behavior.

## Scope

LayerFS captures complete filesystem trees, including `.git`, ignored files and build outputs. Forks share
committed content; a Workspace records its changes until commit. This is useful for several branches starting
from the same built tree. A warm snapshot must match the OS, architecture, Rust toolchain and dependencies of
its consumer. NSH's macOS, Windows, iOS and Android checks continue on their native runners.

The Store uses LayerFS's Disposable profile. Keep source changes in Git and publish through pull requests;
Store history is rebuildable experiment data. Each invocation retains its own local Docker volume. Transporting
Stores between hosts or making them the shared CI cache needs a separate qualification and workload measurement.

## Run locally

Requirements: a local Linux Docker Engine with API 1.54 or newer (the trial uses Engine 29.7.2),
local ext4, XFS or Btrfs Store volumes, `/dev/fuse`, Git and Python 3.12+. Docker must allow the Sandbox
`CAP_SYS_ADMIN` and FUSE device. On Linux, preparation uses the host's rustup and C/C++ compiler. On macOS,
preparation runs in a Linux build container through the active local Docker context (including OrbStack).
The runtime image supplies Node, clang and Rust for commands in Workspaces.

Run from the NSH checkout root:

```sh
./scripts/layerfs prepare
./scripts/layerfs smoke
./scripts/layerfs status
```

The executable entry `scripts/layerfs` selects Python 3.12 or newer from PATH or the standard Homebrew location.
It resolves the checkout from its own path, so it can be called from another directory. Docker's active context
selects the socket; `DOCKER_HOST` applies when `DOCKER_CONTEXT` is unset. `--docker-socket PATH` selects a local
socket explicitly for all operations. `status` checks the saved image and build outputs and shows the latest receipt.

`prepare` verifies the upstream source revision, installs the minimal Rust 1.85.1 toolchain, builds the daemon and
Node addon with the upstream lockfile, then builds the runtime image. On macOS it also builds
`nsh-layerfs-build:1.0.0` from `prepare.Dockerfile`, mounts the checkout and existing Cargo registry into a temporary
Linux container, and builds the same pinned tools there. The container exits after preparation. The existing
`DOCKER_DEFAULT_PLATFORM` and `CARGO_TARGET_DIR` settings apply; the resulting daemon path is saved in the deployment
record so later invocations find the same build outputs. NSH's normal `target/` and Cargo registry are reused.
The runtime starts from `rust:1.98.1-trixie`; the exact image ID is recorded in `.nsh/layerfs/deployment.json`
and used for every Sandbox in that invocation. The SDK is loaded directly from the pinned source, without npm
packages or changes to NSH's Rust dependencies.

`smoke` exports the checkout's **HEAD commit** into `.nsh/layerfs/nsh-<id>/input`, creates a fresh Git repository
there with automatic Git maintenance disabled, validates it with `git fsck`, and initializes LayerFS from a
read-only mount of that input. Git status checks use `--no-optional-locks` to keep the original index unchanged. This captures a reproducible input while the current working tree
and existing build cache remain available for normal development. To try source changes, commit them to a topic
branch first. The input has its own `.git` and initially has no `target/`.

The SDK runs in a short-lived Linux control container with the Docker socket and the Store volume mounted.
It connects over host loopback to the Sandbox daemon. Deployment paths are supplied explicitly in the pinned
SDK client's options; its environment-only constructor deployment defaults are overridden in one place in
`smoke.mjs`. This adapter must be reviewed when upgrading LayerFS. Project files and Workspaces use the SDK;
commands run inside the Sandbox, and the host checkout stays the development directory.

The trial checks:

1. Init and fork a complete Git tree; verify `git status` is clean inside the Workspace.
2. Run `cargo +1.98.1 test --locked -p nsh-package`, then commit its `target/` and an ignored probe file.
3. Unmount, fork from the committed branch, and mount again. Verify both files survived and repeat the tests offline.
4. Read the branch's commit history and verify the original `main` still has neither the probe nor build outputs.

`result.json` records each phase and elapsed time; `trace.jsonl` records SDK diagnostics. Cold and warm test times
include Cargo/test execution, while commit and mount are timed separately. Evaluate the entire cycle against
ordinary Cargo before making performance claims. `nsh-package` is the first build workload; core/engine/CLI,
GTK, larger caches and concurrent branches need additional measurements before adoption in required CI.

A failed operation is reported once and leaves its Store and Workspace for diagnosis. To inspect a Sandbox, use
`docker ps -a --filter label=layerfs.project=<project>` with the project name from `settings.json`, then
`docker logs <container>`. Successful unmounted Sandboxes stop after upstream's idle timeout. After retaining any
needed receipts, remove only that trial's containers and the exact volume printed by the command with
`docker rm <container>` and `docker volume rm <volume>`; its generated files live under `.nsh/layerfs/nsh-<id>/`.

## Initial local result

On 2026-10-22, Linux x86_64 with Docker 29.7.2 and local Btrfs, the trial passed all phases against NSH
`2c735b03c806d939a40d5f8b227e22b418fef4ba`. Both runs passed 22 package tests. Init took 328 ms,
the first mount 117 ms, cold build/test 23.160 s, commit 5.371 s, remount 9 ms and warm offline test 653 ms.
This is one functional trial, with dependency downloads in the cold phase and the same Sandbox's registry in
the warm phase; it does not establish a speedup over normal Cargo or reuse across fresh Sandboxes.
The local receipt is `.nsh/layerfs/nsh-87ff1931224e/result.json`; CI uploads receipts for each run.

The trial also established two input requirements: automatic Git maintenance must finish or be disabled before
Init (it removed loose object directories during the first captures), and read-only Git status checks must avoid
index refresh writes (the original branch otherwise becomes dirty on unmount). Both are handled by the trial.

## GitHub Actions

Run **LayerFS trial** manually from Actions. It provisions Docker 29.7.2 and explicitly passes its socket to
the control container; the pinned SDK uses API 1.54, which older hosted Engines reject. It has read-only repository permissions, checks out without stored
credentials, builds the pinned source and runs the same local smoke check. Receipts are uploaded on success or
failure. Each job gets its own Store and normal ephemeral runner lifecycle; no shared Store credentials or external
storage service is configured. This workflow is separate from the required `check` and `compat` statuses.
