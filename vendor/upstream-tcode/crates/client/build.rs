use std::{env, path::PathBuf, process::Command};

const SWIFT_SOURCES: [&str; 2] = [
    "src/speech/apple/MacDictation.swift",
    "src/speech/apple/Transcription.swift",
];

fn main() {
    for source in SWIFT_SOURCES {
        println!("cargo:rerun-if-changed={source}");
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && env::var_os("CARGO_FEATURE_VOICE").is_some()
    {
        build_mac_dictation();
    }
}

/// SpeechAnalyzer has no Objective-C interface, so macOS dictation is Swift
/// linked in as a static library.
fn build_mac_dictation() {
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "aarch64" => "arm64".to_string(),
        arch => arch.to_string(),
    };
    let deployment = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "27.0".into());
    let sdk = xcrun(&["--sdk", "macosx", "--show-sdk-path"]);
    let swiftc = PathBuf::from(xcrun(&["--sdk", "macosx", "--find", "swiftc"]));

    let status = xcrun_command()
        .args(["--sdk", "macosx", "swiftc"])
        .args([
            "-emit-library",
            "-static",
            "-parse-as-library",
            "-O",
            "-whole-module-optimization",
        ])
        .args(["-module-name", "TcodeDictation"])
        .arg("-target")
        .arg(format!("{arch}-apple-macosx{deployment}"))
        .arg("-sdk")
        .arg(&sdk)
        .args(SWIFT_SOURCES)
        .arg("-o")
        .arg(out.join("libtcode_dictation.a"))
        .status()
        .expect("run swiftc");
    assert!(status.success(), "swiftc failed to build macOS dictation");

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=tcode_dictation");
    // The Swift objects autolink the runtime, its back-deployment shims and
    // the frameworks they use; these are where the linker finds them.
    let toolchain = swiftc.parent().unwrap().parent().unwrap();
    println!(
        "cargo:rustc-link-search=native={}",
        toolchain.join("lib/swift/macosx").display()
    );
    println!("cargo:rustc-link-search=native={sdk}/usr/lib/swift");
}

fn xcrun(args: &[&str]) -> String {
    let output = xcrun_command().args(args).output().expect("run xcrun");
    assert!(output.status.success(), "xcrun {args:?} failed");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

// Runs only for macOS targets, where no console window can flash.
#[allow(clippy::disallowed_methods)]
fn xcrun_command() -> Command {
    Command::new("xcrun")
}
