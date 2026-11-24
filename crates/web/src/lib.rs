//! Shared Rust root rendered by NSH's WebAssembly DOM backend.
#[cfg(target_arch = "wasm32")]
nsh_platform_web::rust_app!(
    vec![nsh_base::registry(), muteki_ui::registry()],
    nsh_core::element::Root::of::<muteki_ui::MutekiApp>()
        .tokens(nsh_base::Theme::default().into_tokens())
        .app_id("app.muteki")
);
