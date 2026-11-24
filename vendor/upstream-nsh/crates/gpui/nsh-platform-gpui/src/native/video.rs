//! Videos: GStreamer's `playbin` decoding into frames GPUI paints (feature
//! `video`).
//!
//! Each video is a `playbin` whose video sink is an `appsink` asking for
//! BGRA frames (what GPUI's `RenderImage` holds), keeping only the latest
//! one. Frames and bus messages come on GStreamer's threads: the sample
//! callback turns the frame into a `RenderImage` there and keeps it in the
//! video's [`Shared`] slot, and the bus's sync handler lets the message
//! through to the bus queue; both then wake the main thread once
//! ([`Shared::wake`], coalesced), which finds the video again by surface
//! key and node id, with the serial of the source the pipeline was made
//! for (a late wake of a replaced source is dropped), takes the frame and
//! drains the bus ([`VideoItem::pump`]). The picture is ordinary view
//! content: painted with `paint_image`, clipped, transformed (at its
//! bounds) and covered like any other. Each frame is a new image, so the
//! one it replaces is removed from the window's sprite atlas the next time
//! the video draws.
//!
//! Events follow the other platforms: `onLoadStart` when a source opens,
//! `onLoad` with its duration and size once the first frame came (and the
//! pipeline prerolled), `onProgress` every `progressUpdateInterval` while
//! playing (a timer), `onBuffer` from buffering messages, `onEnd` (not for
//! a looping video: it seeks back to the start instead), `onError` (with a
//! hint when a GStreamer plug-in is missing). `rate` seeks with that rate
//! once the pipeline can seek. `controls` draws a control bar of nsh's own
//! (play / pause, a scrubber, the time), as the Windows backend does.
//!
//! The picture follows the corners of a rounded clipping view (GPUI rounds
//! the sprite); the control bar is cut to the view's rectangle. Not
//! supported: audio-only sources
//! show nothing. When a video's item goes (after any removal animation),
//! its frames leave the sprite atlas with the next frame.
//!
//! Without feature `video` (or when GStreamer cannot start), a video draws
//! a neutral placeholder and reports `onError` for each source.

#[cfg(not(feature = "video"))]
pub(crate) use placeholder::VideoItem;
#[cfg(feature = "video")]
pub(crate) use player::VideoItem;

#[cfg(not(feature = "video"))]
mod placeholder {
    use std::{cell::RefCell, rc::Rc};

    use nsh_core::{EventKind, NodeId, VideoProps};

    use crate::{
        image::AssetBase,
        native::{Emitter, error},
        render::DrawCx,
        surface::Surface,
        view::Hosted,
    };

    /// A video that cannot play.
    pub(crate) struct VideoItem {
        emitter: Emitter,
        props: RefCell<Option<VideoProps>>,
    }

    impl VideoItem {
        pub(crate) fn new(surface: &Rc<Surface>, id: NodeId) -> Rc<Self> {
            Rc::new(VideoItem {
                emitter: Emitter::new(surface, id),
                props: RefCell::new(None),
            })
        }

        /// New props: each new source reports that it cannot play.
        pub(crate) fn set(
            &self,
            _surface: &Rc<Surface>,
            p: &VideoProps,
            _base: Option<&AssetBase>,
        ) {
            let old = self.props.borrow_mut().replace(p.clone());
            if p.source.is_some() && old.is_none_or(|o| o.source != p.source) {
                self.emitter.emit(EventKind::Error, || {
                    error("video playback is not available in this build".to_owned())
                });
            }
        }

        /// `seek(seconds)`.
        pub(crate) fn seek(&self, _seconds: f64) {}

        /// The video's view was deleted.
        pub(crate) fn close(&self) {}
    }

    impl Hosted for VideoItem {
        fn draw(&self, cx: &mut DrawCx<'_>, size: (f32, f32)) {
            crate::native::placeholder(cx, size, "Video is not available");
        }

        fn hit(&self, _x: f32, _y: f32, _size: (f32, f32)) -> bool {
            false
        }
    }
}

#[cfg(feature = "video")]
mod player {
    use std::{
        cell::{Cell, RefCell},
        rc::{Rc, Weak},
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicBool, Ordering},
        },
    };

    use gpui::{RenderImage, px};
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;
    use gstreamer_video as gst_video;
    use gstreamer_video::prelude::*;
    use nsh_core::{Color, EventKind, EventPayload, NodeId, PayloadValue, ResizeMode, VideoProps};

    use super::{clock, finite, fit};
    use crate::{
        image::AssetBase,
        input::{Cursor, PointerKind},
        native::{Emitter, Host, Location, error, locate},
        render::DrawCx,
        surface::Surface,
        view::{Hosted, ItemPointer},
    };

    /// The control bar's height.
    const BAR: f32 = 40.0;
    /// The play / pause button's width at the bar's left.
    const BUTTON: f32 = 40.0;
    /// The room the time takes at the bar's right.
    const TIME: f32 = 96.0;

    /// The scrubber's ends on a control bar `width` wide.
    fn scrubber(width: f32) -> (f32, f32) {
        (BUTTON + 8.0, width - TIME - 4.0)
    }

    /// Starts GStreamer once; false (logged once) when it cannot.
    fn gst_ready() -> bool {
        static READY: OnceLock<bool> = OnceLock::new();
        *READY.get_or_init(|| match gst::init() {
            Ok(()) => true,
            Err(err) => {
                log::warn!(target: "js", "videos cannot play: GStreamer did not start: {err}");
                false
            }
        })
    }

    /// A decoded frame, ready to paint.
    struct Frame {
        image: Arc<RenderImage>,
        size: (u32, u32),
    }

    /// What GStreamer's threads share with the main thread.
    struct Shared {
        /// The latest frame not taken yet.
        frame: Mutex<Option<Frame>>,
        /// A wake is queued on the main thread.
        woken: AtomicBool,
        /// The surface and node of the video, and the source's serial.
        key: u64,
        id: NodeId,
        serial: u64,
    }

    impl Shared {
        /// Asks the main thread to take the frame and drain the bus (once
        /// until it did).
        fn wake(self: &Arc<Self>) {
            if self.woken.swap(true, Ordering::AcqRel) {
                return;
            }
            let shared = self.clone();
            crate::rt::invoke(move || {
                shared.woken.store(false, Ordering::Release);
                let Some(surface) = Surface::by_key(shared.key) else {
                    return;
                };
                let item = match surface.native.hosts.borrow().get(&shared.id) {
                    Some(Host::Video(item)) => item.clone(),
                    _ => return,
                };
                if item.serial.get() == shared.serial {
                    item.pump(&surface);
                }
            });
        }
    }

    /// A pipeline playing one source.
    struct Pipeline {
        playbin: gst::Element,
        shared: Arc<Shared>,
    }

    impl Drop for Pipeline {
        fn drop(&mut self) {
            if let Some(bus) = self.playbin.bus() {
                bus.unset_sync_handler();
            }
            // Stops decoding and audio now.
            let _ = self.playbin.set_state(gst::State::Null);
        }
    }

    /// A video: its pipeline and the picture its view shows.
    pub(crate) struct VideoItem {
        emitter: Emitter,
        surface: Weak<Surface>,
        id: NodeId,
        pipeline: RefCell<Option<Pipeline>>,
        /// Bumped with every source opened (and when closed): wakes for
        /// another serial are stale.
        serial: Cell<u64>,
        /// The props applied (`None` before the first).
        props: RefCell<Option<VideoProps>>,
        /// The frame shown, and whether it was painted (so it is in the
        /// sprite atlas).
        frame: RefCell<Option<(Frame, bool)>>,
        /// Painted frames replaced since, to remove from the atlas.
        retired: RefCell<Vec<Arc<RenderImage>>>,
        /// Playing (not paused by the prop or the control bar).
        playing: Cell<bool>,
        /// The source reached its end (and does not loop).
        ended: Cell<bool>,
        /// The source failed.
        failed: Cell<bool>,
        /// `onLoad` was sent.
        loaded: Cell<bool>,
        /// The pipeline prerolled (it can seek and report its duration).
        ready: Cell<bool>,
        /// The rate applied with a seek.
        rate: Cell<f64>,
        /// Buffering, as last reported.
        buffering: Cell<bool>,
        /// A progress timer runs.
        ticking: Cell<bool>,
        /// The control bar's scrubber is held.
        scrubbing: Cell<bool>,
    }

    impl VideoItem {
        pub(crate) fn new(surface: &Rc<Surface>, id: NodeId) -> Rc<Self> {
            Rc::new(VideoItem {
                emitter: Emitter::new(surface, id),
                surface: Rc::downgrade(surface),
                id,
                pipeline: RefCell::new(None),
                serial: Cell::new(0),
                props: RefCell::new(None),
                frame: RefCell::new(None),
                retired: RefCell::default(),
                playing: Cell::new(false),
                ended: Cell::new(false),
                failed: Cell::new(false),
                loaded: Cell::new(false),
                ready: Cell::new(false),
                rate: Cell::new(1.0),
                buffering: Cell::new(false),
                ticking: Cell::new(false),
                scrubbing: Cell::new(false),
            })
        }

        /// New props.
        pub(crate) fn set(&self, surface: &Rc<Surface>, p: &VideoProps, base: Option<&AssetBase>) {
            let old = self.props.borrow_mut().replace(p.clone());
            if old.as_ref().is_none_or(|o| o.source != p.source) {
                self.open(surface, p, base);
            } else if let Some(old) = &old {
                if old.paused != p.paused {
                    self.play(!p.paused);
                }
                if old.muted != p.muted || old.volume != p.volume {
                    self.apply_audio(p);
                }
                if old.rate != p.rate {
                    self.apply_rate();
                }
            }
            surface.invalidate();
        }

        /// Opens `p.source`, replacing the pipeline.
        fn open(&self, surface: &Rc<Surface>, p: &VideoProps, base: Option<&AssetBase>) {
            self.serial.set(self.serial.get() + 1);
            self.pipeline.borrow_mut().take();
            self.retire_frame();
            self.ended.set(false);
            self.failed.set(false);
            self.loaded.set(false);
            self.ready.set(false);
            self.rate.set(1.0);
            self.buffering.set(false);
            self.playing.set(!p.paused);
            let Some(source) = p.source.as_deref() else {
                return;
            };
            if !gst_ready() {
                self.failed.set(true);
                self.emitter.emit(EventKind::Error, || {
                    error("video playback is not available: GStreamer did not start".to_owned())
                });
                return;
            }
            let Some(uri) = locate(source, base).and_then(|l| uri(&l)) else {
                self.failed.set(true);
                let message = format!("{source}: cannot resolve the video source");
                self.emitter.emit(EventKind::Error, || error(message));
                return;
            };
            let shared = Arc::new(Shared {
                frame: Mutex::new(None),
                woken: AtomicBool::new(false),
                key: surface.key(),
                id: self.id,
                serial: self.serial.get(),
            });
            match build(&uri, &shared) {
                Ok(playbin) => {
                    *self.pipeline.borrow_mut() = Some(Pipeline { playbin, shared });
                    self.apply_audio(p);
                    self.emitter
                        .emit(EventKind::LoadStart, || EventPayload::None);
                    let state = if p.paused {
                        gst::State::Paused
                    } else {
                        gst::State::Playing
                    };
                    self.set_state(state);
                    if !p.paused {
                        self.tick();
                    }
                }
                Err(err) => {
                    self.failed.set(true);
                    let message = format!("{source}: {err}");
                    self.emitter.emit(EventKind::Error, || error(message));
                }
            }
        }

        fn set_state(&self, state: gst::State) {
            let failed = self
                .pipeline
                .borrow()
                .as_ref()
                .is_some_and(|p| p.playbin.set_state(state).is_err());
            if failed {
                // The bus says why.
                log::debug!("video: state change to {state:?} failed");
            }
        }

        /// Plays or pauses.
        fn play(&self, play: bool) {
            self.playing.set(play);
            if self.pipeline.borrow().is_none() || self.failed.get() {
                return;
            }
            if play {
                if self.ended.replace(false) {
                    self.seek_to(0.0);
                }
                self.set_state(gst::State::Playing);
                self.tick();
            } else {
                self.set_state(gst::State::Paused);
            }
        }

        fn apply_audio(&self, p: &VideoProps) {
            if let Some(pipeline) = &*self.pipeline.borrow() {
                pipeline.playbin.set_property("mute", p.muted);
                pipeline
                    .playbin
                    .set_property("volume", f64::from(p.volume.clamp(0.0, 10.0)));
            }
        }

        /// The rate the props ask for.
        fn wanted_rate(&self) -> f64 {
            let rate = self.props.borrow().as_ref().map_or(1.0, |p| p.rate);
            if rate.is_finite() && rate > 0.0 {
                f64::from(rate)
            } else {
                1.0
            }
        }

        /// Seeks to where it is with the props' rate (once it can seek).
        fn apply_rate(&self) {
            if !self.ready.get() {
                return;
            }
            let (time, _) = self.times();
            self.seek_to(time);
        }

        /// `seek(seconds)`.
        pub(crate) fn seek(&self, seconds: f64) {
            self.ended.set(false);
            self.seek_to(seconds);
            if self.active() {
                // Progress stopped at the end.
                self.tick();
            }
            if let Some(surface) = self.surface.upgrade() {
                surface.invalidate();
            }
        }

        fn seek_to(&self, seconds: f64) {
            let Some(pipeline) = &*self.pipeline.borrow() else {
                return;
            };
            let at = gst::ClockTime::from_nseconds((finite(seconds).max(0.0) * 1e9) as u64);
            let rate = self.wanted_rate();
            let flags = gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE;
            let result = pipeline.playbin.seek(
                rate,
                flags,
                gst::SeekType::Set,
                at,
                gst::SeekType::None,
                gst::ClockTime::NONE,
            );
            match result {
                Ok(()) => self.rate.set(rate),
                Err(err) => log::debug!("video: seek failed: {err}"),
            }
        }

        /// The video's view was deleted: the pipeline stops.
        pub(crate) fn close(&self) {
            self.serial.set(self.serial.get() + 1);
            self.pipeline.borrow_mut().take();
        }

        /// The frames it painted leave the atlas with the next frame (the
        /// item outlives its control while a removal animation shows its
        /// last frame).
        fn release(&self) {
            let retired = std::mem::take(&mut *self.retired.borrow_mut());
            let current = self.frame.borrow_mut().take().map(|(frame, _)| frame.image);
            for image in retired.into_iter().chain(current) {
                crate::image::release_later(image);
            }
        }

        /// The frame shown goes (removed from the atlas at the next draw).
        fn retire_frame(&self) {
            if let Some((frame, painted)) = self.frame.borrow_mut().take()
                && painted
            {
                self.retired.borrow_mut().push(frame.image);
            }
        }

        /// On the main thread after a wake: takes the new frame and handles
        /// the bus's messages.
        fn pump(&self, surface: &Rc<Surface>) {
            let (frame, bus) = {
                let pipeline = self.pipeline.borrow();
                let Some(pipeline) = &*pipeline else {
                    return;
                };
                let frame = pipeline
                    .shared
                    .frame
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                (frame, pipeline.playbin.bus())
            };
            if let Some(frame) = frame {
                self.retire_frame();
                *self.frame.borrow_mut() = Some((frame, false));
                surface.invalidate();
                self.try_load();
            }
            let Some(bus) = bus else { return };
            let serial = self.serial.get();
            while let Some(message) = bus.pop() {
                self.message(surface, &message);
                if self.serial.get() != serial {
                    // A handler replaced the source.
                    return;
                }
            }
        }

        fn message(&self, surface: &Rc<Surface>, message: &gst::Message) {
            use gst::MessageView;
            let from_pipeline = {
                let pipeline = self.pipeline.borrow();
                let Some(pipeline) = &*pipeline else { return };
                message
                    .src()
                    .is_some_and(|s| s == pipeline.playbin.upcast_ref::<gst::Object>())
            };
            match message.view() {
                MessageView::AsyncDone(_) => {
                    let first = !self.ready.replace(true);
                    if first && self.wanted_rate() != 1.0 {
                        self.apply_rate();
                    }
                    self.try_load();
                }
                MessageView::StateChanged(s) if from_pipeline => {
                    if s.current() == gst::State::Paused || s.current() == gst::State::Playing {
                        self.ready.set(true);
                        self.try_load();
                    }
                }
                MessageView::Eos(_) => {
                    let repeat = self.props.borrow().as_ref().is_some_and(|p| p.repeat);
                    if repeat {
                        self.seek_to(0.0);
                    } else {
                        self.ended.set(true);
                        self.emitter.emit(EventKind::End, || EventPayload::None);
                    }
                    surface.invalidate();
                }
                MessageView::Buffering(b) => {
                    let buffering = b.percent() < 100;
                    if self.buffering.replace(buffering) != buffering {
                        self.emitter
                            .emit(EventKind::Buffer, || buffer_payload(buffering));
                    }
                }
                MessageView::Error(e) => {
                    if self.failed.replace(true) {
                        return;
                    }
                    let err = e.error();
                    let source = self
                        .props
                        .borrow()
                        .as_ref()
                        .and_then(|p| p.source.as_deref().map(str::to_owned))
                        .unwrap_or_default();
                    let missing = err.matches(gst::CoreError::MissingPlugin)
                        || err.matches(gst::StreamError::CodecNotFound)
                        || err.matches(gst::StreamError::TypeNotFound);
                    let mut message = format!("{source}: {}", err.message());
                    if missing {
                        message.push_str(
                            " (a GStreamer plug-in is missing: install gst-plugins-good, \
                             gst-plugins-bad and gst-libav)",
                        );
                    }
                    log::warn!(target: "js", "video {message}");
                    if let Some(debug) = e.debug() {
                        log::debug!("video: {debug}");
                    }
                    self.emitter.emit(EventKind::Error, || error(message));
                    self.pipeline.borrow_mut().take();
                    surface.invalidate();
                }
                _ => {}
            }
        }

        /// Sends `onLoad` once the pipeline prerolled and a frame came.
        fn try_load(&self) {
            if self.loaded.get() || !self.ready.get() {
                return;
            }
            let Some(size) = self.frame.borrow().as_ref().map(|(f, _)| f.size) else {
                return;
            };
            self.loaded.set(true);
            let (_, duration) = self.times();
            self.emitter
                .emit(EventKind::Load, || load_payload(duration, size.0, size.1));
        }

        /// Current time and duration (seconds; 0 when unknown).
        fn times(&self) -> (f64, f64) {
            let Some(pipeline) = &*self.pipeline.borrow() else {
                return (0.0, 0.0);
            };
            let secs = |t: Option<gst::ClockTime>| t.map_or(0.0, |t| t.nseconds() as f64 / 1e9);
            (
                secs(pipeline.playbin.query_position::<gst::ClockTime>()),
                secs(pipeline.playbin.query_duration::<gst::ClockTime>()),
            )
        }

        fn active(&self) -> bool {
            self.playing.get()
                && !self.ended.get()
                && !self.failed.get()
                && self.pipeline.borrow().is_some()
        }

        /// Reports progress every `progressUpdateInterval` while playing.
        fn tick(&self) {
            if self.ticking.replace(true) {
                return;
            }
            self.schedule_tick();
        }

        fn schedule_tick(&self) {
            let interval = self
                .props
                .borrow()
                .as_ref()
                .map_or(250.0, |p| p.progress_interval_ms.max(16.0));
            let key = self.surface.upgrade().map(|s| s.key());
            let id = self.id;
            crate::rt::after_ms(interval as u64, move || {
                let Some(surface) = key.and_then(Surface::by_key) else {
                    return;
                };
                let item = match surface.native.hosts.borrow().get(&id) {
                    Some(Host::Video(item)) => item.clone(),
                    _ => return,
                };
                if !item.active() {
                    item.ticking.set(false);
                    return;
                }
                if item.loaded.get() {
                    let (time, duration) = item.times();
                    item.emitter
                        .emit(EventKind::Progress, || progress_payload(time, duration));
                    if item.controls() {
                        surface.invalidate();
                    }
                }
                item.schedule_tick();
            });
        }

        fn controls(&self) -> bool {
            self.props.borrow().as_ref().is_some_and(|p| p.controls)
        }

        fn resize_mode(&self) -> ResizeMode {
            self.props
                .borrow()
                .as_ref()
                .map_or(ResizeMode::Contain, |p| p.resize_mode)
        }

        /// Seeks to where `x` is on the scrubber of a bar `width` wide.
        fn scrub(&self, x: f32, width: f32) {
            let (start, end) = scrubber(width);
            let (_, duration) = self.times();
            if end <= start || duration <= 0.0 {
                return;
            }
            let fraction = ((x - start) / (end - start)).clamp(0.0, 1.0) as f64;
            self.seek(duration * fraction);
        }

        /// The control bar at the bottom of `size`.
        fn draw_bar(&self, cx: &mut DrawCx<'_>, size: (f32, f32)) {
            let (w, h) = size;
            let top = h - BAR;
            let cy = top + BAR / 2.0;
            let white = Color::rgba(255, 255, 255, 255);
            cx.fill_rect(
                crate::geom::rect(0.0, top, w, BAR),
                [0.0; 4],
                Color::rgba(0, 0, 0, 140),
            );
            let bx = BUTTON / 2.0;
            if self.active() {
                cx.fill_rect(
                    crate::geom::rect(bx - 6.0, cy - 7.0, 4.0, 14.0),
                    [1.0; 4],
                    white,
                );
                cx.fill_rect(
                    crate::geom::rect(bx + 2.0, cy - 7.0, 4.0, 14.0),
                    [1.0; 4],
                    white,
                );
            } else {
                cx.fill_contours(
                    &[vec![
                        (bx - 5.0, cy - 8.0),
                        (bx + 8.0, cy),
                        (bx - 5.0, cy + 8.0),
                    ]],
                    white,
                );
            }
            let (time, duration) = self.times();
            let (start, end) = scrubber(w);
            if end > start {
                let fraction = if duration > 0.0 {
                    (time / duration).clamp(0.0, 1.0) as f32
                } else {
                    0.0
                };
                let knob = start + (end - start) * fraction;
                cx.fill_rounded(
                    start,
                    cy - 2.0,
                    end - start,
                    4.0,
                    2.0,
                    Color::rgba(255, 255, 255, 90),
                );
                cx.fill_rounded(start, cy - 2.0, knob - start, 4.0, 2.0, white);
                let r = if self.scrubbing.get() { 7.0 } else { 6.0 };
                cx.fill_rounded(knob - r, cy - r, r * 2.0, r * 2.0, r, white);
            }
            let text = format!("{} / {}", clock(time), clock(duration));
            draw_text(cx, &text, w - 10.0, cy, white);
        }
    }

    /// Paints `text` (12pt) ending at `right`, centered on `cy` (item
    /// coordinates).
    fn draw_text(cx: &mut DrawCx<'_>, text: &str, right: f32, cy: f32, color: Color) {
        if cx.culled() {
            return;
        }
        let k = cx.transform_scale();
        let font_size = 12.0 * k;
        let line_height = px(font_size * 1.3);
        let color = cx.color(color);
        let anchor = cx.map(right, cy);
        let text = gpui::SharedString::from(text.to_owned());
        cx.paint(|window, app| {
            let run = gpui::TextRun {
                len: text.len(),
                font: gpui::font(crate::text::ui_family()),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window
                .text_system()
                .shape_line(text, px(font_size), &[run], None);
            let origin = gpui::point(anchor.x - line.width, anchor.y - line_height / 2.0);
            let _ = line.paint(
                origin,
                line_height,
                gpui::TextAlign::Left,
                None,
                window,
                app,
            );
        });
    }

    impl Drop for VideoItem {
        fn drop(&mut self) {
            self.release();
        }
    }

    impl Hosted for VideoItem {
        fn draw(&self, cx: &mut DrawCx<'_>, size: (f32, f32)) {
            // Frames replaced since the last draw leave the atlas.
            let retired = std::mem::take(&mut *self.retired.borrow_mut());
            if !retired.is_empty() {
                cx.paint(|window, _| {
                    for image in retired {
                        let _ = window.drop_image(image);
                    }
                });
            }
            let (w, h) = size;
            cx.push_clip_rect(0.0, 0.0, w, h);
            if !cx.culled() {
                let mode = self.resize_mode();
                let mut slot = self.frame.borrow_mut();
                if let Some((frame, painted)) = &mut *slot {
                    let natural = (frame.size.0 as f32, frame.size.1 as f32);
                    let (x, y, fw, fh) = fit(mode, natural, size);
                    let boxed = cx.rect(0.0, 0.0, w, h);
                    let picture = cx.rect(x, y, fw, fh);
                    // GPUI rounds the sprite: the corners of the innermost
                    // rounded clip (the view's own or a parent's) it reaches.
                    let (bounds, corners) =
                        cx.clip_rounded(boxed.intersect(&picture), gpui::Corners::default());
                    let image = frame.image.clone();
                    cx.paint(|window, _| {
                        let _ = window.paint_image(bounds, picture, corners, image, 0, false);
                    });
                    *painted = true;
                }
            }
            if self.controls() {
                self.draw_bar(cx, size);
            }
            cx.pop();
        }

        fn hit(&self, x: f32, y: f32, size: (f32, f32)) -> bool {
            // Only the control bar: touches elsewhere go to the view.
            self.controls() && x >= 0.0 && x < size.0 && y >= size.1 - BAR && y < size.1
        }

        fn pointer(&self, event: &ItemPointer) -> bool {
            let Some(surface) = self.surface.upgrade() else {
                return false;
            };
            let Some(width) = surface.view(self.id).map(|v| v.state().frame.size.width.0) else {
                return false;
            };
            match event.kind {
                PointerKind::Down if event.x < BUTTON => self.play(!self.active()),
                PointerKind::Down => {
                    self.scrubbing.set(true);
                    self.scrub(event.x, width);
                }
                PointerKind::Move if self.scrubbing.get() => self.scrub(event.x, width),
                PointerKind::Up | PointerKind::Cancel => self.scrubbing.set(false),
                _ => return false,
            }
            surface.invalidate();
            true
        }

        fn cursor(&self, x: f32, _y: f32) -> Option<Cursor> {
            self.controls().then_some(if x < BUTTON {
                Cursor::Hand
            } else {
                Cursor::Arrow
            })
        }
    }

    /// The URI `playbin` opens for `location`.
    fn uri(location: &Location) -> Option<String> {
        match location {
            Location::Url(url) => Some(url.clone()),
            Location::Path(path) => {
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    std::env::current_dir().ok()?.join(path)
                };
                match gst::glib::filename_to_uri(&path, None) {
                    Ok(uri) => Some(uri.to_string()),
                    Err(err) => {
                        log::warn!("video {}: {err}", path.display());
                        None
                    }
                }
            }
        }
    }

    /// A `playbin` for `uri` whose frames and messages go to `shared`.
    fn build(uri: &str, shared: &Arc<Shared>) -> Result<gst::Element, String> {
        let caps = gst_video::VideoCapsBuilder::new()
            .format(gst_video::VideoFormat::Bgra)
            .build();
        let sink = gst_app::AppSink::builder()
            .caps(&caps)
            .max_buffers(1)
            .drop(true)
            .build();
        let frames = shared.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(frame) = to_frame(&sample) {
                        *frames.frame.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
                        frames.wake();
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .new_preroll(|_| Ok(gst::FlowSuccess::Ok))
                .build(),
        );
        let playbin = gst::ElementFactory::make("playbin")
            .property("uri", uri)
            .property("video-sink", &sink)
            .build()
            .map_err(|_| "GStreamer's playbin is missing (install gst-plugins-base)".to_owned())?;
        let bus = playbin.bus().ok_or("the pipeline has no bus")?;
        let messages = shared.clone();
        bus.set_sync_handler(move |_, _| {
            messages.wake();
            gst::BusSyncReply::Pass
        });
        Ok(playbin)
    }

    /// The sample's picture as a GPUI image (BGRA, rows packed).
    fn to_frame(sample: &gst::Sample) -> Option<Frame> {
        let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
        let buffer = sample.buffer()?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
        let (w, h) = (info.width(), info.height());
        let stride = *frame.plane_stride().first()? as usize;
        let data = frame.plane_data(0).ok()?;
        let row = w as usize * 4;
        let mut pixels = Vec::with_capacity(row * h as usize);
        for y in 0..h as usize {
            pixels.extend_from_slice(data.get(y * stride..y * stride + row)?);
        }
        let image = image::RgbaImage::from_raw(w, h, pixels)?;
        Some(Frame {
            image: Arc::new(RenderImage::new(vec![image::Frame::new(image)])),
            size: (w, h),
        })
    }

    fn load_payload(duration: f64, width: u32, height: u32) -> EventPayload {
        EventPayload::Map(vec![
            ("duration", PayloadValue::Number(finite(duration))),
            ("width", PayloadValue::Number(width as f64)),
            ("height", PayloadValue::Number(height as f64)),
        ])
    }

    fn progress_payload(time: f64, duration: f64) -> EventPayload {
        EventPayload::Map(vec![
            ("currentTime", PayloadValue::Number(finite(time))),
            ("duration", PayloadValue::Number(finite(duration))),
        ])
    }

    fn buffer_payload(buffering: bool) -> EventPayload {
        EventPayload::Map(vec![("isBuffering", PayloadValue::Bool(buffering))])
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn makes_file_uris() {
            assert_eq!(
                uri(&Location::Path("/videos/a b#1.mp4".into())).as_deref(),
                Some("file:///videos/a%20b%231.mp4")
            );
            assert_eq!(
                uri(&Location::Url("https://example.com/a.mp4".into())).as_deref(),
                Some("https://example.com/a.mp4")
            );
        }
    }
}

/// Where a picture of `natural` size goes in a box of `size` (x, y, width,
/// height).
#[cfg(feature = "video")]
fn fit(mode: nsh_core::ResizeMode, natural: (f32, f32), size: (f32, f32)) -> (f32, f32, f32, f32) {
    use nsh_core::ResizeMode;
    let (nw, nh) = natural;
    let (w, h) = size;
    if nw <= 0.0 || nh <= 0.0 || matches!(mode, ResizeMode::Stretch) {
        return (0.0, 0.0, w, h);
    }
    let s = match mode {
        ResizeMode::Cover => (w / nw).max(h / nh),
        // Natural size (cropped where larger).
        ResizeMode::Center => 1.0,
        // Shrunk to fit, never enlarged.
        ResizeMode::ScaleDown => (w / nw).min(h / nh).min(1.0),
        _ => (w / nw).min(h / nh),
    };
    let (fw, fh) = (nw * s, nh * s);
    ((w - fw) / 2.0, (h - fh) / 2.0, fw, fh)
}

/// A time for the control bar: `m:ss`, `h:mm:ss` from an hour on.
#[cfg(feature = "video")]
fn clock(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 {
        seconds.floor() as u64
    } else {
        0
    };
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// `x`, or 0 when it is not a finite number (live streams have no
/// duration).
#[cfg(feature = "video")]
fn finite(x: f64) -> f64 {
    if x.is_finite() { x } else { 0.0 }
}

#[cfg(all(test, feature = "video"))]
mod tests {
    use nsh_core::ResizeMode;

    use super::*;

    #[test]
    fn fits_pictures() {
        let natural = (1920.0, 1080.0);
        assert_eq!(
            fit(ResizeMode::Stretch, natural, (100.0, 100.0)),
            (0.0, 0.0, 100.0, 100.0)
        );
        let (x, y, w, h) = fit(ResizeMode::Contain, natural, (320.0, 320.0));
        assert_eq!((x, w), (0.0, 320.0));
        assert!((h - 180.0).abs() < 1e-3 && (y - 70.0).abs() < 1e-3);
        let (x, y, w, h) = fit(ResizeMode::Cover, natural, (320.0, 320.0));
        assert_eq!((y, h), (0.0, 320.0));
        assert!(w > 320.0 && x < 0.0);
        // Center and scale-down never enlarge; center never shrinks.
        for mode in [ResizeMode::Center, ResizeMode::ScaleDown] {
            assert_eq!(
                fit(mode, (100.0, 50.0), (400.0, 200.0)),
                (150.0, 75.0, 100.0, 50.0)
            );
        }
        assert_eq!(
            fit(ResizeMode::Center, (800.0, 400.0), (400.0, 200.0)),
            (-200.0, -100.0, 800.0, 400.0)
        );
    }

    #[test]
    fn formats_times() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(65.9), "1:05");
        assert_eq!(clock(3725.0), "1:02:05");
        assert_eq!(clock(f64::NAN), "0:00");
        assert_eq!(finite(f64::INFINITY), 0.0);
    }
}
