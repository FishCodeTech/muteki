---
name: mbx
description: "Invoke before the first cargo command of a session (build, test, clippy, run, check) and whenever a build fills the disk or a worktree has no target/. Routes cargo through the mbx shared build cache when it is installed."
---

# mbx: the shared Rust build cache

mbx (mr_boxington) shares compiled work across every checkout and worktree on
the machine and prunes its own storage. A cargo command that runs through mbx
hits that cache; plain cargo builds a private, unmanaged `target/` of
25–35 GB per worktree and shares nothing.

## Detect

Run `mbx doctor`. If the command is missing, mbx is not installed: use cargo
as usual and stop here. If it reports `Cargo shim is not installed` or the
shell cannot run the mise shim (sandboxed agents commonly see this), the
explicit form below still works.

## Use

- Replace the word `cargo` with `mbx`: `mbx build --release -p tcode --locked`,
  `mbx nextest run --workspace --locked`, `mbx clippy ...`, `mbx run ...`.
  The subcommand and arguments are unchanged; `mbx cargo ...` is rejected.
- When `mbx` is not on PATH inside a sandbox, call the binary mise installed:
  `$(mise where mr-boxington)/.mise-bins/mbx <cargo-subcommand> <args>` (on this machine
  `~/.local/share/mise/installs/mr-boxington/latest/.mise-bins/mbx`).
- A worktree whose `target/` is a symlink into `~/Library/Caches/mbx/targets`
  is managed. A plain `target/` directory was built by raw cargo: run
  `mbx adopt` in that worktree once, then continue through mbx.
- Build jobs need no limit for cache reasons; a cold build through mbx is
  mostly cache hits once any checkout has built the same dependencies.

## Disk

`mbx stats` shows the cache; `mbx gc --dry-run` shows what pruning would
remove; `mbx clean` drops one worktree's managed target. Delete a finished
worktree's `target/` before removing the worktree so mbx does not keep an
orphaned managed target around.
