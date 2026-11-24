//! Stamps the CLI with its source revision without changing crate versions.
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn short_sha(value: &str) -> Option<String> {
    (value.len() >= 7 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| value[..7].to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"));
    let root = manifest.parent().and_then(Path::parent);
    // A published crate can live inside someone else's Git repository.
    let checkout = root
        .is_some_and(|root| root.join(".git").exists() && root.join("crates/nsh-cli") == manifest);
    let revision = if checkout {
        // Packed branches acquire a loose ref on the next commit. Watch
        // the refs directory too, so creating that file refreshes the SHA.
        for name in ["HEAD", "refs", "packed-refs"] {
            if let Some(path) = git(&manifest, &["rev-parse", "--git-path", name]) {
                let path = manifest.join(path);
                if path.exists() {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
        }
        git(&manifest, &["rev-parse", "HEAD"]).and_then(|s| short_sha(&s))
    } else {
        let vcs = manifest.join(".cargo_vcs_info.json");
        if vcs.exists() {
            println!("cargo:rerun-if-changed={}", vcs.display());
        }
        fs::read(vcs)
            .ok()
            .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok())
            .and_then(|data| data["git"]["sha1"].as_str().and_then(short_sha))
    };
    println!(
        "cargo:rustc-env=NSH_GIT_SHA={}",
        revision.as_deref().unwrap_or("unknown")
    );
}
