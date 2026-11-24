---
title: Local Development with LayerFS
description: "Use the Native Shell checkout's Linux LayerFS trial to capture Cargo build outputs, fork a workspace and verify snapshot reuse."
order: 61.5
group: tools
---

Native Shell's repository includes a local LayerFS workflow for testing reusable build workspaces. It imports a
committed source tree, runs Cargo inside a Linux Sandbox, saves the resulting files and runs the tests again from
a fork of that saved state. Source editing and Git commits happen in your existing NSH checkout.

> [!IMPORTANT]
> This workflow requires Docker Engine and Linux FUSE. Docker runs the control process and Sandbox; FUSE mounts
> the workspace inside the Sandbox. On macOS, the development command prepares this Linux deployment through
> a local Docker engine such as OrbStack.

## What the snapshot saves

LayerFS saves a filesystem tree: source files, Git metadata, ignored files and the build outputs created inside the
workspace. A fork starts from a saved tree and records its own changes. This lets several tasks start from the same
built state while keeping their changes separate.

In NSH's local trial, the first branch runs `nsh-package`'s tests and saves `target/` plus an ignored probe file.
The second branch starts from that commit, checks those files and repeats the tests offline. A third branch starts
from the original source state and verifies isolation.

| Phase | What happens |
| --- | --- |
| Init | Import an export of the checkout's `HEAD` commit and a fresh `.git` directory |
| Build | Fork a workspace and run `cargo +1.98.1 test --locked -p nsh-package` |
| Commit | Save the generated `target/` and probe file into LayerFS history |
| Reuse | Fork the saved branch, mount it and repeat the tests with `--offline` |
| Verify | Read commit history and check that the original source branch stays clean |

Each invocation creates a fresh Store and performs this complete sequence. Snapshot reuse happens between the
branches of that invocation. The trial's benefit to measure is the cost of reaching another usable, tested workspace,
including mount, commit and unmount time.

## Prepare the development environment

Use a checkout of the [NSH repository](https://github.com/longbridge/nsh) with:

- A local Linux Docker engine with ext4, XFS or Btrfs for its Store volume, running on Linux or through a macOS
  environment such as OrbStack.
- Docker Engine serving API 1.54 or newer; the verified local setup uses Engine 29.7.2.
- `/dev/fuse`, with Docker permitted to expose that device and `CAP_SYS_ADMIN` to the Sandbox.
- Git and Python 3.12 or newer; on Linux, rustup and a C/C++ compiler.
- Network access to fetch the pinned LayerFS source, Cargo dependencies and runtime image.

The trial builds LayerFS **1.0.0**, pinned to source commit
`2aaf2552ce21154928b9f9c83582e41a637f5910`. Its runtime image supplies Rust 1.98.1, Node and clang for the Linux
processes. The [upstream SDK guide](https://github.com/Ephemeral-AI-Lab/layerfs/blob/v1.0.0/sdk/js/README.md)
describes the Store and Sandbox lifecycle.

Run these commands from the NSH checkout root:

```sh
./scripts/layerfs prepare
./scripts/layerfs smoke
./scripts/layerfs status
```

The executable selects Python 3.12 or newer, including Homebrew's installation on macOS, and discovers the socket
from the active Docker context. Use `--docker-socket PATH` to select a local socket explicitly.

`prepare` checks the Docker API, installs Rust 1.85.1 and builds the pinned daemon, Node addon and runtime image.
On macOS it prepares these in a temporary Linux build container. It reuses the usual Cargo registry and `target/`,
including an existing `CARGO_TARGET_DIR` setting. The exact runtime image ID and daemon path are recorded in
`.nsh/layerfs/deployment.json`. `status` checks the saved build outputs and image and shows the latest validation result.

`smoke` creates `.nsh/layerfs/nsh-<id>/input` from the checkout's **committed `HEAD`**, then imports that input into
a new Store. To include source edits, commit them to your topic branch before running it. The generated input starts
with a fresh Git repository and acquires its build outputs inside the Sandbox.

The control process opens the Store in a named local Docker volume. The Sandbox daemon opens the same Store and
mounts the branch's filesystem view; Cargo executes in that mounted workspace. Your normal checkout remains the
place where you edit and commit source changes.

## Read the result

The command prints the receipt directory and Store volume name. Open the generated `result.json` to check
`"ok": true` and the elapsed time for each phase. The same directory holds `settings.json`, which identifies the
source commit and image, and `trace.jsonl`, which records SDK calls.

The local validation ran 22 package tests in each of the build and reuse phases. The initial build/test phase
included dependency downloads; the offline phase used the saved `target/` and the same Sandbox's Cargo registry.
Compare the complete cycle with ordinary Cargo builds on your workload when evaluating its value.

Use matching operating systems, architectures, toolchains and dependencies when reusing a built state. This guide's
workload is the Linux `nsh-package` test suite; platform UI checks use their respective platform environments
([Testing](./testing-and-trace.md)).

## Keep useful work and manage the Store

Keep source changes in Git and submit them through pull requests. The trial uses LayerFS's **Disposable** Store
profile for rebuildable workspace history. Retain useful receipts separately when recording measurements.

The generated input disables automatic Git maintenance during its initial commit, keeping the object database
stable while LayerFS imports it. Git status checks inside the trial use `--no-optional-locks` so reading status
preserves the original index. For custom scripts, finish background writers before importing or committing a tree.

Successful unmounted Sandboxes stop after LayerFS's idle timeout. Each trial retains its Store volume; manage that
specific volume and its containers by the identifiers in the receipt. A failed operation retains its recorded state
for diagnosis. The [maintainer reference](../../docs/dev/layerfs.md) covers the scripts, receipts and resource cleanup.
