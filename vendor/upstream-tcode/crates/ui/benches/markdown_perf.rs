use std::{
    hint::black_box,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use gpui::{
    App, AppContext as _, Bounds, Context, Element, ElementId, Entity, GlobalElementId,
    InspectorElementId, LayoutId, Pixels, Render, TestAppContext, Window,
};
use tcode_ui::markdown::{MarkdownState, MarkdownView};

const KIB: usize = 1024;
const STREAM_BYTES: usize = 50_000;
const STREAM_DELTA_BYTES: usize = 100;

struct BenchRoot {
    markdown: Entity<MarkdownState>,
    construction_time: Arc<Mutex<Duration>>,
}

impl Render for BenchRoot {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        TimedMarkdown {
            inner: MarkdownView::new(&self.markdown).selectable(true),
            construction_time: self.construction_time.clone(),
        }
    }
}

struct TimedMarkdown {
    inner: MarkdownView,
    construction_time: Arc<Mutex<Duration>>,
}

impl gpui::IntoElement for TimedMarkdown {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TimedMarkdown {
    type RequestLayoutState = <MarkdownView as Element>::RequestLayoutState;
    type PrepaintState = <MarkdownView as Element>::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        self.inner.id()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let start = Instant::now();
        let result = self
            .inner
            .request_layout(global_id, inspector_id, window, cx);
        *self.construction_time.lock().unwrap() = start.elapsed();
        result
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.inner
            .prepaint(global_id, inspector_id, bounds, request_layout, window, cx)
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.paint(
            global_id,
            inspector_id,
            bounds,
            request_layout,
            prepaint,
            window,
            cx,
        );
    }
}

struct RenderHarness {
    root: Entity<BenchRoot>,
    window: gpui::WindowHandle<BenchRoot>,
    construction_time: Arc<Mutex<Duration>>,
    // Must drop last: GPUI's leak detector expects external handles to be gone.
    cx: TestAppContext,
}

impl RenderHarness {
    fn new(document: &str) -> Self {
        Self::with_width(document, None)
    }

    fn with_width(document: &str, width: Option<Pixels>) -> Self {
        let mut cx = initialized_context();
        let document = document.to_owned();
        let state = cx.new(|cx| MarkdownState::new(&document, cx));
        let root_state = state.clone();
        let construction_time = Arc::new(Mutex::new(Duration::ZERO));
        let root_construction_time = construction_time.clone();
        let window = cx.add_window(move |_, _| BenchRoot {
            markdown: root_state,
            construction_time: root_construction_time,
        });
        if let Some(width) = width {
            cx.simulate_window_resize(window.into(), gpui::size(width, gpui::px(844.)));
        }
        let root = window.root(&mut cx).expect("benchmark window has a root");
        let mut harness = Self {
            root,
            window,
            construction_time,
            cx,
        };
        harness.draw();
        harness
    }

    fn construction_duration(&self) -> Duration {
        *self.construction_time.lock().unwrap()
    }

    fn draw(&mut self) {
        self.root.update(&mut self.cx, |_, cx| cx.notify());
        self.cx
            .update_window(self.window.into(), |_, window, cx| {
                let _ = black_box(window.draw(cx));
            })
            .expect("benchmark window remains open");
    }
}

fn initialized_context() -> TestAppContext {
    let cx = TestAppContext::single();
    cx.update(tcode_ui::theme::init);
    cx.update(tcode_ui::markdown::init);
    cx
}

fn realistic_markdown(target_bytes: usize) -> String {
    assert!(target_bytes >= 8 * KIB);
    let code_lines = match target_bytes {
        0..=20_000 => 200,
        20_001..=100_000 => 400,
        _ => 800,
    };
    let mut out = String::with_capacity(target_bytes);
    out.push_str("# Streaming agent analysis\n\n");
    out.push_str("This is a realistic **long response** with `inline_code`, links, and enough prose to wrap across several lines in a chat message. The renderer must remain responsive while tokens arrive.\n\n");
    out.push_str("## Rust implementation\n\n```rust\n");
    for ix in 0..code_lines {
        out.push_str(&format!("let r{ix}={ix};\n"));
    }
    out.push_str("```\n\n## TypeScript client\n\n```ts\n");
    for ix in 0..code_lines {
        out.push_str(&format!("const t{ix}={ix};\n"));
    }
    out.push_str("```\n\n");

    let section = "### Findings\n\n- [x] Parse the streamed content\n- [ ] Avoid rebuilding offscreen blocks\n- [ ] Cache expensive code measurements\n\n| phase | owner | status |\n|:--|:--:|--:|\n| parse | UI | measured |\n| layout | GPUI | pending |\n| paint | GPU | pending |\n\n> A representative blockquote keeps the generated input structurally varied.\n\nThe response continues with *emphasis*, ~~obsolete text~~, an [example](https://example.com), and ordinary prose that exercises inline parsing and wrapping.\n\n";
    while out.len() + section.len() <= target_bytes {
        out.push_str(section);
    }
    if out.len() < target_bytes {
        let remaining = target_bytes - out.len();
        if remaining >= 2 {
            out.push_str(&"x".repeat(remaining - 2));
            out.push_str("\n\n");
        } else {
            out.push_str(&"x".repeat(remaining));
        }
    }
    out.truncate(target_bytes);
    assert_eq!(out.len(), target_bytes);
    assert!(
        out.is_ascii(),
        "100-byte streaming boundaries must be valid UTF-8"
    );
    out
}

/// Long assistant prose that wraps heavily at phone width: the timeline
/// scrolling case from issue #371, where per-character selection geometry
/// dominated frame time.
fn long_prose_markdown(paragraphs: usize) -> String {
    let sentence = "The renderer must stay responsive while a long explanation streams in, so every paragraph here wraps across many visual lines on a narrow screen. ";
    let mut out = String::new();
    for ix in 0..paragraphs {
        out.push_str(&format!("Paragraph {ix}: "));
        for _ in 0..10 {
            out.push_str(sentence);
        }
        out.push_str("\n\n");
    }
    out
}

fn alternate_document(document: &str) -> String {
    let mut alternate = document.to_owned();
    let byte = alternate.as_bytes().last().copied().unwrap_or(b'x');
    alternate.replace_range(alternate.len() - 1.., if byte == b'x' { "y" } else { "x" });
    alternate
}

fn percentile(samples: &[Duration], percentile: f64) -> Duration {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let ix = ((sorted.len() - 1) as f64 * percentile).ceil() as usize;
    sorted[ix]
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn time_repeated(mut operation: impl FnMut(), repetitions: usize) -> Vec<Duration> {
    (0..repetitions)
        .map(|_| {
            let start = Instant::now();
            operation();
            start.elapsed()
        })
        .collect()
}

fn measure_parse(document: &str) -> Vec<Duration> {
    let mut cx = initialized_context();
    let state = cx.new(|cx| MarkdownState::new("", cx));
    let alternate = alternate_document(document);
    let mut use_alternate = false;
    time_repeated(
        || {
            let next = if use_alternate { &alternate } else { document };
            state.update(&mut cx, |state, cx| state.set_text(black_box(next), cx));
            use_alternate = !use_alternate;
        },
        31,
    )
}

fn measure_streaming(document: &str) -> (Duration, Vec<Duration>) {
    assert_eq!(document.len(), STREAM_BYTES);
    let mut cx = initialized_context();
    let state = cx.new(|cx| MarkdownState::new("", cx));
    let mut deltas = Vec::with_capacity(STREAM_BYTES / STREAM_DELTA_BYTES);
    let total_start = Instant::now();
    let mut offset = 0;
    for end in (STREAM_DELTA_BYTES..=STREAM_BYTES).step_by(STREAM_DELTA_BYTES) {
        let start = Instant::now();
        state.update(&mut cx, |state, cx| {
            state.push_str(black_box(&document[offset..end]), cx)
        });
        deltas.push(start.elapsed());
        offset = end;
    }
    (total_start.elapsed(), deltas)
}

fn print_measurement_table(documents: &[(usize, String)]) {
    println!("\nmarkdown_perf one-shot report (release profile; warm render/highlight cache)");
    println!(
        "size_bytes\tparse_p50_ms\tstream_total_ms\tstream_p50_ms\tstream_p95_ms\tstream_max_ms\trender_root_p50_ms\tframe_p50_ms"
    );
    for (size, document) in documents {
        let parse = measure_parse(document);
        let mut render = RenderHarness::new(document);
        let mut construction = Vec::with_capacity(15);
        let frames = time_repeated(
            || {
                render.draw();
                construction.push(render.construction_duration());
            },
            15,
        );
        if *size == STREAM_BYTES {
            let (total, deltas) = measure_streaming(document);
            println!(
                "{}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}",
                size,
                milliseconds(percentile(&parse, 0.50)),
                milliseconds(total),
                milliseconds(percentile(&deltas, 0.50)),
                milliseconds(percentile(&deltas, 0.95)),
                milliseconds(*deltas.iter().max().unwrap()),
                milliseconds(percentile(&construction, 0.50)),
                milliseconds(percentile(&frames, 0.50)),
            );
        } else {
            println!(
                "{}\t{:.3}\t-\t-\t-\t-\t{:.3}\t{:.3}",
                size,
                milliseconds(percentile(&parse, 0.50)),
                milliseconds(percentile(&construction, 0.50)),
                milliseconds(percentile(&frames, 0.50)),
            );
        }
    }
    let phone = long_prose_markdown(40);
    let mut render = RenderHarness::with_width(&phone, Some(gpui::px(390.)));
    let frames = time_repeated(|| render.draw(), 15);
    println!(
        "phone_390px_long_prose\t{}\t-\t-\t-\t-\t-\t-\t{:.3}",
        phone.len(),
        milliseconds(percentile(&frames, 0.50))
    );
    println!(
        "parse includes set_text's same-size String copy, selection reset, and notify; render_root includes MarkdownView request-layout registration"
    );
}

fn markdown_benchmarks(c: &mut Criterion) {
    let documents = [10 * KIB, STREAM_BYTES, 200 * KIB]
        .into_iter()
        .map(|size| (size, realistic_markdown(size)))
        .collect::<Vec<_>>();

    print_measurement_table(&documents);

    let mut parse_group = c.benchmark_group("markdown_parse_via_set_text");
    for (size, document) in &documents {
        let mut cx = initialized_context();
        let state = cx.new(|cx| MarkdownState::new("", cx));
        let alternate = alternate_document(document);
        let mut use_alternate = false;
        parse_group.bench_function(BenchmarkId::from_parameter(size), |b| {
            b.iter(|| {
                let next = if use_alternate { &alternate } else { document };
                state.update(&mut cx, |state, cx| state.set_text(black_box(next), cx));
                use_alternate = !use_alternate;
            });
        });
    }
    parse_group.finish();

    let stream_document = documents
        .iter()
        .find(|(size, _)| *size == STREAM_BYTES)
        .unwrap()
        .1
        .clone();
    c.bench_function("markdown_streaming_50kb_500x100b", |b| {
        let mut cx = initialized_context();
        let state = cx.new(|cx| MarkdownState::new("", cx));
        b.iter(|| {
            state.update(&mut cx, |state, cx| state.set_text("", cx));
            let mut start = 0;
            for end in (STREAM_DELTA_BYTES..=STREAM_BYTES).step_by(STREAM_DELTA_BYTES) {
                state.update(&mut cx, |state, cx| {
                    state.push_str(black_box(&stream_document[start..end]), cx)
                });
                start = end;
            }
        });
    });

    let mut render_group = c.benchmark_group("markdown_render_root_request_layout");
    for (size, document) in &documents {
        let mut harness = RenderHarness::new(document);
        render_group.bench_function(BenchmarkId::from_parameter(size), |b| {
            b.iter_custom(|iterations| {
                let mut measured = Duration::ZERO;
                for _ in 0..iterations {
                    harness.draw();
                    measured += harness.construction_duration();
                }
                measured
            });
        });
    }
    render_group.finish();

    let mut frame_group = c.benchmark_group("markdown_full_frame_draw");
    for (size, document) in &documents {
        let mut harness = RenderHarness::new(document);
        frame_group.bench_function(BenchmarkId::from_parameter(size), |b| {
            b.iter(|| harness.draw());
        });
    }
    frame_group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2));
    targets = markdown_benchmarks
}
criterion_main!(benches);
