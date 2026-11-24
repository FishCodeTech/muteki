//! `cargo run -p example-embed-gpui` from the repository's root: runs the
//! GPUI Kit example, which is a workspace of its own (so GPUI stays out of
//! the main workspace's builds), with `cargo run` there, in the same
//! profile and into the repository's `target/`. It only forwards: the
//! arguments go to the example, its exit code comes back.

use std::{env, path::Path, process::Command};

fn main() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    // `cargo run` gives its program the cargo that started it.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cargo = Command::new(cargo);
    cargo
        .current_dir(example)
        .args(["run", "--manifest-path"])
        .arg(example.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(example.join("../../target"));
    if !cfg!(debug_assertions) {
        cargo.arg("--release");
    }
    let status = cargo
        .arg("--")
        .args(env::args_os().skip(1))
        .status()
        .unwrap_or_else(|err| panic!("cannot run cargo: {err}"));
    std::process::exit(status.code().unwrap_or(1));
}
