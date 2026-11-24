//! End-to-end: bundle the counter example, run it on the engine thread and
//! tap a button after the idle GC has run.

use std::{path::PathBuf, time::Duration};

use crossbeam_channel::Receiver;
use nsh_core::{ApproxTextEngine, EventKind, MountBatch, MountItem, Points, Size, geometry::px};
use nsh_engine::{Bundle, Engine, EngineConfig};

use crate::ChannelSink;

fn start() -> (Engine, Receiver<MountBatch>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    (
        Engine::start(config(), || ApproxTextEngine, ChannelSink(tx)).unwrap(),
        rx,
    )
}

/// The counter example, bundled and compiled.
fn config() -> EngineConfig {
    let entry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/counter/App.tsx");
    let bundle = nsh_bundler::bundle(&nsh_bundler::BundleOptions {
        entry,
        minify: false,
        split: false,
        ui_sets: vec![nsh_base::registry().into()],
    })
    .unwrap();
    let bytecode = nsh_js::compile_bytecode(&bundle.code, false).unwrap();
    EngineConfig {
        // SAFETY: compiled just now by this runtime.
        app: nsh_engine::App::Js(unsafe { Bundle::bytecode(bytecode) }),
        viewport: Size::new(px(390.0), px(844.0)),
        trace: false,
        network: None,
        websocket: None,
        filesystem: None,
        assets: None,
        environment: Default::default(),
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
    }
}

fn texts(batch: &MountBatch) -> Vec<String> {
    batch
        .items
        .iter()
        .filter_map(|i| match i {
            MountItem::Text { text, .. } => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn tap_after_idle_gc_updates_the_ui() {
    let (engine, rx) = start();
    let first = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first frame");
    assert!(texts(&first).contains(&"0".to_owned()));
    let buttons: Vec<_> = first
        .items
        .iter()
        .filter_map(|i| match i {
            MountItem::Events { id, events } if events.contains(EventKind::Press) => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(buttons.len(), 2, "two buttons");

    // Let the idle collector run: the app must stay reachable from JS.
    while rx.recv_timeout(Duration::from_millis(1300)).is_ok() {}

    let plus = buttons[1];
    engine
        .control()
        .dispatch_event(plus, EventKind::Press, nsh_core::EventPayload::None);
    let batch = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("update after tap");
    let texts = texts(&batch);
    assert!(texts.contains(&"1".to_owned()), "{texts:?}");
    assert!(texts.contains(&"• 1".to_owned()), "{texts:?}");
}

/// What the engine handed a [`ResizeSink`].
#[derive(Debug)]
enum Sent {
    Batch(MountBatch),
    Resized(Size<Points>),
}

/// Hands batches and resize reports to the test, in order.
struct ResizeSink(crossbeam_channel::Sender<Sent>);

impl nsh_engine::MountSink for ResizeSink {
    fn submit(&self, batch: MountBatch) {
        let _ = self.0.send(Sent::Batch(batch));
    }

    fn resized(&self, size: Size<Points>) {
        let _ = self.0.send(Sent::Resized(size));
    }
}

#[test]
fn resize_is_reported_after_its_layout() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let engine = Engine::start(config(), || ApproxTextEngine, ResizeSink(tx)).unwrap();
    let next = || rx.recv_timeout(Duration::from_secs(5)).expect("a message");
    assert!(matches!(next(), Sent::Batch(_)), "first frame");

    // The layout at the new size comes first, then the report.
    let size = Size::new(px(500.0), px(700.0));
    engine.control().resize(size);
    let Sent::Batch(batch) = next() else {
        panic!("the layout at the new size first");
    };
    assert!(
        batch.items.iter().any(|i| matches!(
            i,
            MountItem::Frame { frame, .. } if frame.size.width.0 == 500.0 && frame.size.height.0 == 700.0
        )),
        "the app's root view takes the size"
    );
    assert!(matches!(next(), Sent::Resized(s) if s == size));

    // Reported even when nothing changes.
    engine.control().resize(size);
    assert!(matches!(next(), Sent::Resized(s) if s == size));
}
