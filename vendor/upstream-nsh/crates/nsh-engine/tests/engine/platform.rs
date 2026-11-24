//! The OS the platform reports decides `platform` and which platform-only
//! props (`ios={{…}}`, `android={{…}}`, `desktop={{…}}`, `web={{…}}`)
//! apply; on the web, the idiom decides between mobile and desktop.

use std::time::Duration;

use nsh_core::{ApproxTextEngine, MountItem, Size, geometry::px};
use nsh_engine::{Bundle, Engine, EngineConfig, Environment, Idiom, Os};

use crate::ChannelSink;

const APP: &str = r#"import { mount, Text, View } from "nsh";
import { platform } from "nsh";

mount(
  () => (
    <View
      ios={{ accessibilityLabel: "ios" }}
      android={{ accessibilityLabel: "android" }}
      desktop={{ accessibilityLabel: "desktop" }}
      web={{ accessibilityHint: "web" }}
    >
      <Text>{`${platform.os} ${platform.idiom === "desktop"} ${platform.select({ ios: "i", mobile: "m", desktop: "d" })} ${platform.select({ native: "n", default: "-" })}`}</Text>
    </View>
  ),
);
"#;

/// The first frame's texts and accessibility labels and hints on `os`.
fn first_frame(os: Os, idiom: Idiom) -> (Vec<String>, Vec<String>) {
    let dir = std::env::temp_dir().join(format!(
        "nsh-platform-{}-{}-{idiom:?}",
        std::process::id(),
        os.name()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("App.tsx");
    std::fs::write(&entry, APP).unwrap();
    let bundle = nsh_bundler::bundle(&nsh_bundler::BundleOptions {
        entry,
        minify: false,
        split: false,
        ui_sets: vec![nsh_base::registry().into()],
    })
    .unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let config = EngineConfig {
        app: nsh_engine::App::Js(Bundle::source(bundle.code)),
        viewport: Size::new(px(320.0), px(480.0)),
        trace: false,
        network: None,
        websocket: None,
        filesystem: None,
        assets: None,
        environment: Environment {
            os,
            idiom,
            ..Environment::default()
        },
        header_heights: (nsh_core::Points(44.0), nsh_core::Points(96.0)),
        limits: nsh_engine::Limits::default(),
        ui_sets: Vec::new(),
        runtime: None,
        data_dir: None,
        cache: nsh_engine::CacheOptions::off(),
        storage: None,
        clock: Default::default(),
        context: Default::default(),
        pinned: Default::default(),
        clipboard: None,
        host: Default::default(),
        picker: None,
    };
    let _engine = Engine::start(config, || ApproxTextEngine, ChannelSink(tx)).unwrap();
    let batch = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first frame");
    let mut texts = Vec::new();
    let mut labels = Vec::new();
    for item in &batch.items {
        match item {
            MountItem::Text { text, .. } => texts.push(text.text.clone()),
            MountItem::ViewProps { props, .. } => {
                labels.extend(props.accessibility.label.as_deref().map(str::to_owned));
                labels.extend(props.accessibility.hint.as_deref().map(str::to_owned));
            }
            _ => {}
        }
    }
    (texts, labels)
}

#[test]
fn platform_props_and_select_follow_the_os() {
    let frame = |os, idiom, text: &str, labels: &[&str]| {
        assert_eq!(
            first_frame(os, idiom),
            (
                vec![text.to_owned()],
                labels.iter().map(|l| (*l).to_owned()).collect()
            ),
            "{os:?} {idiom:?}"
        );
    };
    frame(Os::Ios, Idiom::Phone, "ios false i n", &["ios"]);
    frame(Os::Android, Idiom::Phone, "android false m n", &["android"]);
    frame(Os::MacOs, Idiom::Desktop, "macos true d n", &["desktop"]);
    // `native` is not the web, as in React Native.
    frame(Os::Web, Idiom::Phone, "web false m -", &["web"]);
    frame(Os::Web, Idiom::Pad, "web false m -", &["web"]);
    // A desktop browser behaves as a desktop.
    frame(Os::Web, Idiom::Desktop, "web true d -", &["desktop", "web"]);
}
