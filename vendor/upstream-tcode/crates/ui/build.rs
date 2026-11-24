use std::{env, fs, path::PathBuf};

fn main() {
    let icons_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"))
        .join("../../assets/icons/bundled");

    // Browser assets must be available synchronously, including on an offline LAN.
    let mut icons = fs::read_dir(&icons_dir)
        .expect("read component icons")
        .map(|entry| entry.expect("read icon entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "svg"))
        .collect::<Vec<_>>();
    icons.sort();
    let mut source = String::from("const COMPONENT_ICONS: &[(&str, &[u8])] = &[\n");
    let mut names = String::from("#[derive(IntoElement, Clone)]\npub enum IconName {\n");
    let mut paths = String::from(
        "impl IconNamed for IconName { fn path(self) -> SharedString { match self {\n",
    );
    for icon in icons {
        let name = icon.file_name().expect("icon filename").to_string_lossy();
        let variant: String = icon
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .split('-')
            .map(|part| {
                let mut chars = part.chars();
                format!(
                    "{}{}",
                    chars.next().unwrap().to_ascii_uppercase(),
                    chars.as_str()
                )
            })
            .collect();
        names.push_str(&format!("{variant},\n"));
        paths.push_str(&format!("Self::{variant} => \"icons/{name}\",\n"));
        source.push_str(&format!(
            "(\"icons/{name}\", include_bytes!({:?})),\n",
            icon.to_str().expect("UTF-8 icon path")
        ));
    }
    source.push_str("];\n");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("component_icons.rs"),
        source,
    )
    .expect("write embedded component icon table");

    names.push_str("}\n");
    paths.push_str("}.into() } }\n");
    names.push_str(&paths);
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("icon_names.rs"),
        names,
    )
    .expect("write icon names");
    println!("cargo:rerun-if-changed={}", icons_dir.display());
    println!("cargo:rerun-if-changed=build.rs");

    // The targets where `native-preview` embeds a system browser in the window.
    println!("cargo::rustc-check-cfg=cfg(preview_backend)");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if env::var_os("CARGO_FEATURE_NATIVE_PREVIEW").is_some()
        && matches!(
            os.as_str(),
            "macos" | "windows" | "linux" | "android" | "ios"
        )
    {
        println!("cargo::rustc-cfg=preview_backend");
    }
}
