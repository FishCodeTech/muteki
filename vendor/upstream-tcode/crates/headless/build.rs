fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_WEB");
    if std::env::var_os("CARGO_FEATURE_WEB").is_none() {
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    for file in ["index.html", "tcode_web.js", "tcode_web_bg.wasm"] {
        let path = root.join("../web/dist").join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        assert!(
            path.is_file(),
            "missing browser bundle file {}: run crates/web/build.sh before building tcode-headless --features web",
            path.display()
        );
    }
}
