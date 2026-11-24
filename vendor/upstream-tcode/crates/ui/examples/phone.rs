//! The compact layout in a desktop window, at phone geometry.
//!
//! It opens the same `run_shell` every client opens, at a width below the
//! compact breakpoint. A desktop build is always the wide layout, so the one
//! phone-specific thing here is `force_mobile_layout`: a preview-only switch
//! that makes this process lay out as a mobile build would, so the desktop can
//! review the compact layout without a device.
//!
//! ```sh
//! cargo run -p tcode-ui --example phone            # 393×852
//! cargo run -p tcode-ui --example phone -- --wide   # 768×852
//! cargo run -p tcode-ui --example phone -- --android  # 412×915
//! TCODE_DATA_DIR=$(mktemp -d) cargo run -p tcode-ui --example phone -- --local
//! ```

use std::borrow::Cow;
use std::rc::Rc;

use gpui::{Bounds, WindowBackgroundAppearance, WindowBounds, WindowOptions, point, px, size};
use tcode_client::host::ClientHost;
use tcode_ui::{ShellOptions, ShellSetup};

type LocalTransport = Rc<dyn Fn(&mut gpui::App) -> Result<tcode_client::host::Transport, String>>;

fn main() {
    let local = std::env::args().any(|arg| arg == "--local");
    let wide = std::env::args().any(|arg| arg == "--wide");
    let android = std::env::args().any(|arg| arg == "--android");
    gpui_platform::application()
        .with_assets(tcode_ui::assets::Assets)
        .run(move |cx| {
            tcode_ui::force_mobile_layout(cx);
            let dimensions = if wide {
                size(px(768.), px(852.))
            } else if android {
                size(px(412.), px(915.))
            } else {
                size(px(393.), px(852.))
            };
            let data_dir = tcode_services::store::data_dir().expect("no data directory");
            let host: Rc<dyn ClientHost> = Rc::new(tcode_traverse::NativeClientHost::new(
                data_dir,
                tcode_traverse::native_host::default_device_name(),
            ));
            let local_transport: Option<LocalTransport> = local.then(|| {
                let store = tcode_services::store::SessionStore::open_at(
                    tcode_services::store::data_dir().expect("data directory"),
                )
                .expect("open local host");
                let mut services = tcode_runtime::pipe::HostServices::default();
                if let Err(error) = services.start_mcp_servers(true) {
                    eprintln!("phone: MCP servers unavailable: {error}");
                }
                let host =
                    tcode_runtime::pipe::spawn_host(store, services).expect("start local host");
                let mux = tcode_traverse::HostMux::new(host.attachments, host.from_host);
                Rc::new(move |_: &mut gpui::App| {
                    let connection = mux.attach(
                        tcode_protocol::Principal::Full,
                        tcode_client::attachment::AttachmentPolicy::LOCAL,
                    );
                    let (_, state) = async_channel::unbounded();
                    Ok(tcode_client::host::Transport {
                        originals: None,
                        to_host: connection.to_host.into(),
                        from_host: connection.from_host,
                        state,
                        current_host: None,
                    })
                }) as LocalTransport
            });
            tcode_ui::run_shell(
                cx,
                host.clone(),
                ShellOptions {
                    window: WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            point(px(0.), px(0.)),
                            dimensions,
                        ))),
                        titlebar: None,
                        window_background: WindowBackgroundAppearance::Opaque,
                        ..Default::default()
                    },
                    title: "Tcode phone".into(),
                    theme_json: Cow::Owned(tcode_ui::flattened_theme_json()),
                    activate: true,
                    setup: ShellSetup {
                        initial: if local {
                            Some(tcode_ui::remote::AttachmentTarget::Local)
                        } else {
                            tcode_ui::last_host_target(host.as_ref())
                        },
                        initial_pairing_error: None,
                        client_host: Some(host),
                        local: local_transport,
                        local_failure: None,
                        seed_blocking: local,
                        restore_navigation: true,
                    },
                    ..Default::default()
                },
            );
        });
}
