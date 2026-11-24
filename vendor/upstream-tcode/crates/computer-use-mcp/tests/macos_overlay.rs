//! Opt-in AppKit regression on the process main thread. It imports the production
//! overlay owner, so reverting its animator call makes this test fail.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use computer_use_mcp::outline;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod ax {
    pub(super) use computer_use_mcp::frontmost_pid as frontmost_application_pid;
}
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[allow(dead_code, unused_imports)] // The runner imports the owner, including unused submission entry points.
#[path = "../src/backend/macos/overlay/mod.rs"]
mod overlay;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let requested = args
        .iter()
        .any(|arg| arg == "--ignored" || arg == "--include-ignored");
    // cargo-nextest enumerates custom harnesses with `--list --format terse`
    // and expects one `<name>: test` line per test; the ignored listing
    // (`--list --ignored`) is the only one this opt-in test belongs in.
    if args.iter().any(|arg| arg == "--list") {
        if requested {
            println!("macos_overlay: test");
        }
        return;
    }
    if !requested || !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        println!(
            "test macos_overlay ... ignored (requires macOS arm64 desktop and explicit --ignored; opens a passthrough test panel)"
        );
        return;
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    overlay::verify_native();
    println!("test macos_overlay ... ok");
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[allow(dead_code)] // Same production feedback owner; only native regression paths run here.
#[path = "../src/feedback.rs"]
mod feedback;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod backend {
    pub(crate) fn clear_feedback(owner: Option<u64>, action: Option<u64>) {
        super::overlay::clear(owner, action);
    }
}
