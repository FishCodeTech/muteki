//! The window-level image viewer: one image on a host-filling dialog, zoomed
//! and panned by wheel, trackpad, pinch, drag, touch, keyboard and a toolbar.
//! Shared by the composer's pending strip, sent-message thumbnails, Markdown
//! images and image-link badges.

use std::sync::Arc;
use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;

use gpui::{
    App, AppContext as _, Bounds, ClickEvent, Context, FocusHandle, ImageCacheError, ImageSource,
    InteractiveElement as _, IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, PinchEvent, Pixels, Point, Render, RenderImage, ScrollDelta,
    ScrollWheelEvent, SharedString, Size, StatefulInteractiveElement as _, Styled as _, TouchPhase,
    Window, actions, div, img, point, prelude::FluentBuilder as _, px, size,
};
use gpui_base::motion::{Interpolate, Transition, transition};
#[cfg(target_family = "wasm")]
use web_time::Instant;

use crate::icon::IconName;
use crate::overlay::OverlayExt as _;
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};

actions!(image_viewer, [ZoomIn, ZoomOut, ZoomToFit, ActualSize]);

const CONTEXT: &str = "ImageViewer";

/// Zoom gained by one wheel notch, one key press or one toolbar press.
const ZOOM_STEP: f32 = 1.25;
/// What one wheel notch reports when a wheel reports pixels, as browsers do.
const WHEEL_NOTCH: Pixels = px(100.);
/// The pixel delta that zooms by a factor of e when a pinch arrives as a
/// Control-wheel, the encoding browsers give trackpad pinches.
const PINCH_PIXELS: Pixels = px(100.);
/// A pan's momentum carries no touch phase; scrolls arriving closer together
/// than this after the pan's fingers lifted still belong to it.
const MOMENTUM_GAP: Duration = Duration::from_millis(100);
/// Zoom relative to the fitted size past which zooming stops. The image's
/// own size stays reachable even when that lies further out.
const MAX_ZOOM: f32 = 8.;
/// What one wheel line is worth when a line delta pans.
const WHEEL_LINE: Pixels = px(20.);
/// Pointer travel before a press is a drag rather than the click it began as.
const DRAG_SLOP: Pixels = px(4.);
const STAGE_MARGIN: Pixels = px(16.);
/// How long keys, buttons and double-clicks take to reach their placement.
const GLIDE: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub(crate) enum ViewerSource {
    Image(ImageSource),
    Original {
        target: tcode_protocol::OriginalTarget,
        preview: Option<ImageSource>,
    },
}

impl From<ImageSource> for ViewerSource {
    fn from(source: ImageSource) -> Self {
        Self::Image(source)
    }
}
impl From<tcode_protocol::OriginalTarget> for ViewerSource {
    fn from(target: tcode_protocol::OriginalTarget) -> Self {
        Self::Original {
            target,
            preview: None,
        }
    }
}

impl ViewerSource {
    pub(crate) fn original(target: tcode_protocol::OriginalTarget, preview: ImageSource) -> Self {
        Self::Original {
            target,
            preview: Some(preview),
        }
    }
}

enum OriginalState {
    Loading { received: u64, total: Option<u64> },
    Cancelled,
    Failed(SharedString),
    Complete,
}

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("=", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("shift-=", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("secondary-=", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("-", ZoomOut, Some(CONTEXT)),
        KeyBinding::new("secondary--", ZoomOut, Some(CONTEXT)),
        KeyBinding::new("0", ZoomToFit, Some(CONTEXT)),
        KeyBinding::new("secondary-0", ZoomToFit, Some(CONTEXT)),
        KeyBinding::new("1", ActualSize, Some(CONTEXT)),
        KeyBinding::new("secondary-1", ActualSize, Some(CONTEXT)),
    ]);
}

/// Open `source` in the viewer on the window's dialog stack, where it
/// inherits Escape dismissal and focus restoration.
pub(crate) fn open(
    source: impl Into<ViewerSource>,
    title: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) {
    let viewer = cx.new(|cx| {
        let mut viewer = match source.into() {
            ViewerSource::Image(source) => ImageViewer::new(source, title.into(), cx),
            ViewerSource::Original { target, preview } => {
                let mut viewer = ImageViewer::new(
                    preview.unwrap_or_else(|| ImageSource::Custom(Arc::new(|_, _| None))),
                    title.into(),
                    cx,
                );
                viewer.target = Some(target);
                viewer.start_original(cx);
                viewer
            }
        };
        // A window keeps a drawn image's texture until it is dropped.
        cx.on_release(|viewer, cx| {
            if let Some(original) = viewer.original.take() {
                cx.drop_image(original, None);
            }
        })
        .detach();
        let namespace = cx
            .try_global::<crate::store::images::HostImages>()
            .map(|images| images.namespace);
        viewer.host_changes = Some(cx.observe_global::<crate::store::images::HostImages>(
            move |this, cx| {
                if cx
                    .try_global::<crate::store::images::HostImages>()
                    .is_none_or(|images| {
                        images.link.is_none() || Some(images.namespace) != namespace
                    })
                {
                    this.cancel_original(cx);
                }
            },
        ));
        viewer
    });
    let focus = viewer.read(cx).focus_handle.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let closing = viewer.clone();
        dialog
            .fill()
            .close_button(false)
            .on_close(move |_, _, cx| {
                closing.update(cx, |viewer, cx| viewer.cancel_original(cx));
            })
            .child(viewer.clone())
    });
    focus.focus(window, cx);
}

/// What an image draws when it has no preview to show; the viewer it still opens reads the
/// original. `caption` names the action on tiles large enough for text, and `framed` gives
/// the placeholder its own surface where nothing behind the image draws one.
pub(crate) fn unpreviewed(
    caption: bool,
    framed: bool,
    cx: &App,
) -> impl Fn() -> gpui::AnyElement + 'static {
    let color = cx.theme().muted_foreground;
    let surface = framed.then(|| (cx.theme().muted, crate::material::radius_card(cx)));
    let caption_text = cx.theme().tokens.typography.xs;
    let spacing_xs = cx.theme().tokens.spacing.xs;
    let caption =
        caption.then(|| SharedString::from(crate::tr!("markdown.open_image").into_owned()));
    move || {
        gpui_base::v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(spacing_xs)
            .text_color(color)
            .when_some(surface, |tile, (background, radius)| {
                tile.rounded(radius).bg(background)
            })
            .child(
                crate::icon::Icon::default()
                    .path("icons/image.svg")
                    .text_color(color),
            )
            .when_some(caption.clone(), |tile, caption| {
                tile.child(div().text_token(&caption_text).child(caption))
            })
            .into_any_element()
    }
}

/// Where the image sits: its zoom relative to the fitted size, where 1 fits
/// the stage, and the pan of its centre from the stage centre.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Placement {
    zoom: f32,
    offset: Point<Pixels>,
}

impl Placement {
    const FIT: Self = Self {
        zoom: 1.,
        offset: Point {
            x: Pixels::ZERO,
            y: Pixels::ZERO,
        },
    };
}

impl Interpolate for Placement {
    fn interpolate(&self, target: &Self, progress: f32) -> Self {
        Self {
            zoom: self.zoom + (target.zoom - self.zoom) * progress,
            offset: self.offset + (target.offset - self.offset) * progress,
        }
    }
}

/// A scroll gesture delimited by touch phases: a trackpad, or fingers on a
/// touch screen.
#[derive(Clone, Copy)]
enum Pan {
    Touching,
    /// The fingers lifted at this instant, or the momentum after it last moved.
    Coasting(Instant),
}

/// A primary press on the stage, kept until the click it completes reads it
/// or a release elsewhere ends it.
struct Drag {
    origin: Point<Pixels>,
    last: Point<Pixels>,
    moved: bool,
}

pub(crate) struct ImageViewer {
    source: ImageSource,
    title: SharedString,
    focus_handle: FocusHandle,
    placement: Placement,
    drag: Option<Drag>,
    pan: Option<Pan>,
    /// Keys, buttons and double-clicks glide to their placement; wheel,
    /// pinch and drag track the pointer.
    animate: bool,
    /// The last frame's geometry, so pointer input resolves against what was
    /// drawn: the stage is the whole dialog host, the frame is the area a
    /// fitted image fills, and `natural` is the decoded image's own size, or
    /// the original's while a preview stands in for it.
    stage: Bounds<Pixels>,
    frame: Size<Pixels>,
    natural: Option<Size<Pixels>>,
    /// The original's size as its metadata names it, before its bytes arrive.
    original_size: Option<Size<Pixels>>,
    failed: bool,
    target: Option<tcode_protocol::OriginalTarget>,
    remote_progress: bool,
    original: Option<Arc<RenderImage>>,
    original_state: Option<OriginalState>,
    transfer: Option<gpui::Task<()>>,
    host_changes: Option<gpui::Subscription>,
}

impl ImageViewer {
    fn new(source: ImageSource, title: SharedString, cx: &mut Context<Self>) -> Self {
        Self {
            source,
            title,
            focus_handle: cx.focus_handle(),
            placement: Placement::FIT,
            drag: None,
            pan: None,
            animate: false,
            stage: Bounds::default(),
            frame: Size::default(),
            natural: None,
            original_size: None,
            failed: false,
            target: None,
            remote_progress: false,
            original: None,
            original_state: None,
            transfer: None,
            host_changes: None,
        }
    }

    fn cancel_original(&mut self, cx: &mut Context<Self>) {
        if self.transfer.take().is_some()
            && matches!(self.original_state, Some(OriginalState::Loading { .. }))
        {
            self.original_state = Some(OriginalState::Cancelled);
            cx.notify();
        }
    }

    fn start_original(&mut self, cx: &mut Context<Self>) {
        self.cancel_original(cx);
        let Some(target) = self.target.clone() else {
            return;
        };
        let Some(host) = cx
            .try_global::<crate::store::images::HostImages>()
            .and_then(|images| images.link.clone())
        else {
            self.original_state = Some(OriginalState::Failed(
                crate::tr!("image_viewer.load_failed").into_owned().into(),
            ));
            return;
        };
        self.remote_progress = host.is_remote();
        let remote = self.remote_progress;
        let renderer = cx.svg_renderer();
        self.original_state = Some(OriginalState::Loading {
            received: 0,
            total: None,
        });
        self.transfer = Some(cx.spawn(async move |this, cx| {
            let mut transfer = host.read_original(target);
            let result = async {
                let metadata = transfer.metadata().await?;
                let total = metadata.len;
                let dimensions = metadata.dimensions;
                let _ = this.update(cx, |this, cx| {
                    this.original_size =
                        dimensions.map(|(width, height)| size(px(width as f32), px(height as f32)));
                    if remote {
                        this.original_state = Some(OriginalState::Loading {
                            received: 0,
                            total: Some(total),
                        });
                    }
                    cx.notify();
                });
                let mut bytes = Vec::new();
                while let Some(block) = transfer.read().await? {
                    bytes
                        .try_reserve(block.len())
                        .map_err(tcode_client::original::error)?;
                    bytes.extend_from_slice(&block);
                    let received = transfer.received();
                    if remote
                        && this
                            .update(cx, |this, cx| {
                                this.original_state = Some(OriginalState::Loading {
                                    received,
                                    total: Some(total),
                                });
                                cx.notify();
                            })
                            .is_err()
                    {
                        return Err(tcode_client::original::error("viewer closed"));
                    }
                }
                let format = image::guess_format(&bytes)
                    .ok()
                    .and_then(|format| gpui::ImageFormat::from_mime_type(format.to_mime_type()))
                    .ok_or_else(|| tcode_client::original::error("unsupported image format"))?;
                cx.background_executor()
                    .spawn(async move {
                        gpui::Image::from_bytes(format, bytes)
                            .to_image_data(renderer)
                            .map_err(tcode_client::original::error)
                    })
                    .await
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(original) => {
                        this.source = ImageSource::Render(original.clone());
                        this.original = Some(original);
                        this.original_state = Some(OriginalState::Complete);
                    }
                    Err(error) => {
                        this.original_state = Some(OriginalState::Failed(error.message.into()))
                    }
                }
                cx.notify();
            });
        }));
    }

    /// The scale at which the image fits the frame. A smaller image is shown
    /// at its own size rather than enlarged, except a preview of an original
    /// whose size is not known yet, which stands in at the frame's size.
    fn fit_scale(&self) -> Option<f32> {
        let natural = self.natural?;
        if natural.width <= Pixels::ZERO
            || natural.height <= Pixels::ZERO
            || self.frame.width <= Pixels::ZERO
            || self.frame.height <= Pixels::ZERO
        {
            return None;
        }
        let fit = (self.frame.width / natural.width).min(self.frame.height / natural.height);
        Some(
            if self.original_size.is_none()
                && matches!(self.original_state, Some(OriginalState::Loading { .. }))
            {
                fit
            } else {
                fit.min(1.)
            },
        )
    }

    /// The zoom that shows the image at its own size.
    fn actual_zoom(&self) -> f32 {
        self.fit_scale().map_or(1., |fit| 1. / fit)
    }

    fn max_zoom(&self) -> f32 {
        MAX_ZOOM.max(self.actual_zoom())
    }

    fn image_bounds(&self, placement: Placement) -> Option<Bounds<Pixels>> {
        let natural = self.natural?;
        let scale = self.fit_scale()? * placement.zoom;
        let size = size(natural.width * scale, natural.height * scale);
        let centre = self.stage.center() + placement.offset;
        Some(Bounds {
            origin: point(centre.x - size.width / 2., centre.y - size.height / 2.),
            size,
        })
    }

    /// The zoom kept in range, and the image kept covering the stage on each
    /// axis where it is larger and centred on each where it is not.
    fn clamped(&self, placement: Placement) -> Placement {
        let zoom = placement.zoom.clamp(1., self.max_zoom());
        let centred = Placement {
            zoom,
            offset: Point::default(),
        };
        let Some(bounds) = self.image_bounds(centred) else {
            return centred;
        };
        let slack = |image: Pixels, stage: Pixels| ((image - stage) / 2.).max(Pixels::ZERO);
        let x = slack(bounds.size.width, self.stage.size.width);
        let y = slack(bounds.size.height, self.stage.size.height);
        Placement {
            zoom,
            offset: point(
                placement.offset.x.max(-x).min(x),
                placement.offset.y.max(-y).min(y),
            ),
        }
    }

    fn pannable(&self) -> bool {
        self.image_bounds(self.placement).is_some_and(|bounds| {
            bounds.size.width > self.stage.size.width || bounds.size.height > self.stage.size.height
        })
    }

    fn set_placement(&mut self, placement: Placement, animate: bool, cx: &mut Context<Self>) {
        self.placement = self.clamped(placement);
        self.animate = animate;
        cx.notify();
    }

    /// Zoom to `zoom` keeping the image point under `anchor` where it is,
    /// as far as the clamp allows.
    fn zoom_to(&mut self, zoom: f32, anchor: Point<Pixels>, animate: bool, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(1., self.max_zoom());
        let ratio = zoom / self.placement.zoom;
        let from_centre = anchor - self.stage.center();
        let offset = from_centre * (1. - ratio) + self.placement.offset * ratio;
        self.set_placement(Placement { zoom, offset }, animate, cx);
    }

    fn step(&mut self, factor: f32, cx: &mut Context<Self>) {
        let zoom = self.placement.zoom * factor;
        self.zoom_to(zoom, self.stage.center(), true, cx);
    }

    fn pan_by(&mut self, delta: Point<Pixels>, cx: &mut Context<Self>) {
        let offset = self.placement.offset + delta;
        self.set_placement(
            Placement {
                offset,
                ..self.placement
            },
            false,
            cx,
        );
    }

    /// A double-click or double-tap: back to the fit when zoomed, otherwise
    /// in to the image's own size, or twice the fit when that is nearer.
    fn toggle_zoom(&mut self, anchor: Point<Pixels>, cx: &mut Context<Self>) {
        let zoom = if self.placement.zoom > 1. {
            1.
        } else {
            self.actual_zoom().max(2.)
        };
        self.zoom_to(zoom, anchor, true, cx);
    }

    fn pressed(&mut self, event: &MouseDownEvent) {
        self.drag = Some(Drag {
            origin: event.position,
            last: event.position,
            moved: false,
        });
    }

    fn pointer_moved(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = &mut self.drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        let delta = event.position - drag.last;
        drag.last = event.position;
        let travel = event.position - drag.origin;
        if !drag.moved && travel.x.abs() < DRAG_SLOP && travel.y.abs() < DRAG_SLOP {
            return;
        }
        drag.moved = true;
        self.pan_by(delta, cx);
    }

    fn clicked(&mut self, event: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.drag.take().is_some_and(|drag| drag.moved) {
            return;
        }
        let position = event.position();
        if event.click_count() == 2 {
            self.toggle_zoom(position, cx);
            return;
        }
        let on_image = self
            .image_bounds(self.placement)
            .is_some_and(|bounds| bounds.contains(&position));
        if !on_image {
            self.cancel_original(cx);
            window.close_dialog(cx);
        }
    }

    /// A trackpad or touch pan moves the image; any other scroll is a wheel,
    /// and zooms about the pointer, as does a scroll with Control or the
    /// platform modifier held, as browsers do.
    fn scrolled(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        self.pan = match (event.touch_phase, self.pan) {
            (TouchPhase::Started, _) => Some(Pan::Touching),
            (TouchPhase::Ended | TouchPhase::Cancelled, Some(_)) => Some(Pan::Coasting(now)),
            (TouchPhase::Moved, Some(Pan::Touching)) => Some(Pan::Touching),
            (TouchPhase::Moved, Some(Pan::Coasting(last)))
                if now.saturating_duration_since(last) <= MOMENTUM_GAP =>
            {
                Some(Pan::Coasting(now))
            }
            _ => None,
        };
        let modified = event.modifiers.control || event.modifiers.platform;
        if self.pan.is_some() && !modified {
            self.pan_by(event.delta.pixel_delta(WHEEL_LINE), cx);
            return;
        }
        let factor = match event.delta {
            ScrollDelta::Lines(lines) => ZOOM_STEP.powf(lines.y),
            ScrollDelta::Pixels(pixels) if modified => (pixels.y / PINCH_PIXELS).exp(),
            ScrollDelta::Pixels(pixels) => ZOOM_STEP.powf(pixels.y / WHEEL_NOTCH),
        };
        if factor != 1. {
            self.zoom_to(self.placement.zoom * factor, event.position, false, cx);
        }
    }

    fn pinched(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        if event.delta > -1. {
            let zoom = self.placement.zoom * (1. + event.delta);
            self.zoom_to(zoom, event.position, false, cx);
        }
    }

    fn decode(&mut self, window: &mut Window, cx: &mut App) {
        let decoded = match &self.source {
            ImageSource::Resource(resource) => {
                window.use_asset::<gpui::ImgResourceLoader>(resource, cx)
            }
            ImageSource::Custom(load) => load(window, cx),
            ImageSource::Render(image) => Some(Ok(image.clone())),
            ImageSource::Image(image) => image.clone().use_render_image(window, cx).map(Ok),
        };
        let natural = decoded.map(|result: Result<Arc<RenderImage>, ImageCacheError>| {
            result
                .ok()
                .map(|image| image.size(0))
                .filter(|size| size.width.0 > 0 && size.height.0 > 0)
        });
        self.failed = matches!(natural, Some(None));
        let decoded = natural
            .flatten()
            .map(|size| gpui::size(px(size.width.0 as f32), px(size.height.0 as f32)));
        self.natural = match self.original_state {
            Some(OriginalState::Complete) => decoded,
            _ => decoded.and(self.original_size).or(decoded),
        };
    }
}

impl Render for ImageViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let insets = crate::window_seam::content_insets(window);
        let compact = crate::window_seam::window_is_compact(window, cx);
        self.stage = Bounds {
            origin: Point::default(),
            size: viewport,
        };
        // The frame is centred on the stage, so the band the toolbar takes
        // above it is mirrored below.
        let band = insets.top.max(insets.bottom)
            + px(crate::material::bar_height(
                crate::material::Bar::Strip,
                compact,
            ))
            + STAGE_MARGIN;
        self.frame = size(
            (viewport.width - insets.left - insets.right - STAGE_MARGIN * 2.).max(px(1.)),
            (viewport.height - band * 2.).max(px(1.)),
        );
        self.decode(window, cx);
        self.placement = self.clamped(self.placement);
        let policy = Transition::new(if self.animate { GLIDE } else { Duration::ZERO });
        let shown = transition("image-viewer-placement", self.placement, policy, window, cx);
        let image = self.image_bounds(shown);
        let zoomed = self.placement.zoom > 1.;
        let percent = self
            .fit_scale()
            .map(|fit| format!("{}%", (fit * shown.zoom * 100.).round() as i32));
        let pannable = self.pannable();
        let muted = cx.theme().muted_foreground;

        let stage = div()
            .id("image-viewer-stage")
            .absolute()
            .inset_0()
            .occlude()
            .when(pannable, |el| el.cursor_grab())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event, _, _| this.pressed(event)),
            )
            .on_mouse_move(cx.listener(|this, event, _, cx| this.pointer_moved(event, cx)))
            .on_click(cx.listener(|this, event, window, cx| this.clicked(event, window, cx)))
            .on_scroll_wheel(cx.listener(|this, event, _, cx| this.scrolled(event, cx)))
            .on_pinch(cx.listener(|this, event, _, cx| this.pinched(event, cx)))
            .when_some(image, |el, bounds| {
                el.child(
                    img(self.source.clone())
                        .id("image-viewer-content")
                        .debug_selector(|| "image-viewer-image".into())
                        .absolute()
                        .left(bounds.origin.x)
                        .top(bounds.origin.y)
                        .w(bounds.size.width)
                        .h(bounds.size.height),
                )
            })
            .when(image.is_none(), |el| {
                el.flex().items_center().justify_center().map(|el| {
                    if self.failed
                        || matches!(
                            self.original_state,
                            Some(OriginalState::Failed(_) | OriginalState::Cancelled)
                        )
                    {
                        el.child(
                            div()
                                .text_token(&cx.theme().tokens.typography.sm)
                                .text_color(muted)
                                .child(match &self.original_state {
                                    Some(OriginalState::Failed(message)) => message.clone(),
                                    Some(OriginalState::Cancelled) => {
                                        crate::tr!("image_viewer.cancelled").into_owned().into()
                                    }
                                    _ => crate::tr!("image_viewer.load_failed").into_owned().into(),
                                }),
                        )
                    } else if self.target.is_none() || self.remote_progress {
                        el.child(crate::widgets::Spinner::new())
                    } else {
                        el.child(crate::icon::Icon::default().path("icons/image.svg"))
                    }
                })
            });

        let fit_or_actual = if zoomed {
            crate::material::toolbar_icon_button(
                "image-viewer-fit",
                IconName::Minimize2,
                crate::tr!("image_viewer.fit").into_owned(),
                compact,
            )
            .on_click(cx.listener(|this, _, _, cx| this.set_placement(Placement::FIT, true, cx)))
        } else {
            crate::material::toolbar_icon_button(
                "image-viewer-actual-size",
                IconName::Maximize2,
                crate::tr!("image_viewer.actual_size").into_owned(),
                compact,
            )
            .disabled(self.actual_zoom() <= 1.)
            .on_click(cx.listener(|this, _, _, cx| {
                let zoom = this.actual_zoom();
                this.zoom_to(zoom, this.stage.center(), true, cx);
            }))
        };
        let show_transfer = (self.remote_progress
            || !matches!(self.original_state, Some(OriginalState::Loading { .. })))
            && matches!(
                self.original_state,
                Some(
                    OriginalState::Loading { .. }
                        | OriginalState::Cancelled
                        | OriginalState::Failed(_)
                )
            );
        let transfer_controls = crate::material::overlay_contour(div(), cx)
            .absolute()
            .bottom(insets.bottom + STAGE_MARGIN)
            .left(insets.left + STAGE_MARGIN)
            .right(insets.right + STAGE_MARGIN)
            .flex()
            .items_center()
            .justify_center()
            .gap(cx.theme().tokens.spacing.sm)
            .p(cx.theme().tokens.spacing.sm)
            .rounded(crate::material::radius_overlay(cx))
            .text_color(cx.theme().foreground)
            .occlude()
            .when_some(self.original_state.as_ref(), |panel, state| match state {
                OriginalState::Loading { received, total } => panel
                    .child(
                        div().text_secondary(false, cx).child(match total {
                            Some(total) => crate::tr!(
                                "image_viewer.progress",
                                received = received.to_string(),
                                total = total.to_string()
                            )
                            .into_owned(),
                            None => crate::tr!("image_viewer.connecting").into_owned(),
                        }),
                    )
                    .child(
                        Button::new("image-viewer-cancel")
                            .secondary()
                            .small()
                            .label(crate::tr!("image_viewer.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_original(cx))),
                    ),
                OriginalState::Cancelled | OriginalState::Failed(_) => panel.child(
                    Button::new("image-viewer-retry")
                        .secondary()
                        .small()
                        .label(crate::tr!("image_viewer.retry"))
                        .on_click(cx.listener(|this, _, _, cx| this.start_original(cx))),
                ),
                OriginalState::Complete => panel,
            });

        let traffic_lights = crate::window_caption::traffic_light_inset(window);
        let bar = crate::material::bar_divider_below(
            crate::material::bar(
                "image-viewer-toolbar",
                crate::material::Bar::Strip,
                compact,
                cx,
            ),
            cx,
        )
        .when(traffic_lights > Pixels::ZERO, |bar| bar.pl(traffic_lights))
        .bg(cx.theme().popover)
        .child(crate::material::bar_title(self.title.clone(), cx))
        .child(
            crate::material::toolbar_icon_button(
                "image-viewer-zoom-out",
                IconName::ZoomOut,
                crate::tr!("image_viewer.zoom_out").into_owned(),
                compact,
            )
            .disabled(!zoomed)
            .on_click(cx.listener(|this, _, _, cx| this.step(1. / ZOOM_STEP, cx))),
        )
        .child(
            // Fixed so the zoom controls hold still as the percentage changes.
            div()
                .w(px(48.))
                .text_center()
                .text_secondary(false, cx)
                .children(percent),
        )
        .child(
            crate::material::toolbar_icon_button(
                "image-viewer-zoom-in",
                IconName::ZoomIn,
                crate::tr!("image_viewer.zoom_in").into_owned(),
                compact,
            )
            .disabled(self.placement.zoom >= self.max_zoom())
            .on_click(cx.listener(|this, _, _, cx| this.step(ZOOM_STEP, cx))),
        )
        .child(fit_or_actual)
        .child(
            crate::material::toolbar_icon_button(
                "image-viewer-close",
                IconName::Close,
                crate::tr!("image_viewer.close").into_owned(),
                compact,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.cancel_original(cx);
                window.close_dialog(cx);
            })),
        );
        let toolbar = div()
            .absolute()
            .top(insets.top)
            .left(insets.left)
            .right(insets.right)
            .occlude()
            .child(bar);

        div()
            .id("image-viewer")
            .track_focus(&self.focus_handle)
            .key_context(CONTEXT)
            .size_full()
            .relative()
            .overflow_hidden()
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.step(ZOOM_STEP, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.step(1. / ZOOM_STEP, cx)))
            .on_action(cx.listener(|this, _: &ZoomToFit, _, cx| {
                this.set_placement(Placement::FIT, true, cx)
            }))
            .on_action(cx.listener(|this, _: &ActualSize, _, cx| {
                let zoom = this.actual_zoom();
                this.zoom_to(zoom, this.stage.center(), true, cx);
            }))
            .child(stage)
            .child(toolbar)
            .when(show_transfer, |view| view.child(transfer_controls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, TouchPhase, VisualTestContext};
    use image::{Frame, ImageBuffer, Rgba};

    struct Body;

    impl Render for Body {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    fn open_viewer(cx: &mut TestAppContext, width: u32, height: u32) -> &mut VisualTestContext {
        cx.update(crate::theme::init);
        cx.update(init);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let body = cx.new(|_| Body);
            gpui_base::Root::new(body, window, cx)
        });
        cx.simulate_resize(size(px(800.), px(600.)));
        let frame = Frame::new(ImageBuffer::from_pixel(width, height, Rgba([0, 0, 0, 255])));
        let image = Arc::new(RenderImage::new(vec![frame]));
        cx.update(|window, cx| open(ImageSource::Render(image), "shot.png", window, cx));
        draw(cx);
        cx
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            _ = window.draw(cx);
        });
    }

    fn image(cx: &mut VisualTestContext) -> Bounds<Pixels> {
        cx.debug_bounds("image-viewer-image")
            .expect("the viewer shows its image")
    }

    fn scroll(cx: &mut VisualTestContext, delta: ScrollDelta, touch_phase: TouchPhase) {
        cx.simulate_event(ScrollWheelEvent {
            position: point(px(400.), px(300.)),
            delta,
            modifiers: Modifiers::default(),
            touch_phase,
        });
        draw(cx);
    }

    /// The image's bounds to a tenth of a pixel, past float noise in zooms
    /// built from exponentials.
    fn rounded(cx: &mut VisualTestContext) -> [f32; 4] {
        let bounds = image(cx);
        [
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.width,
            bounds.size.height,
        ]
        .map(|value| (f32::from(value) * 10.).round() / 10.)
    }

    /// A preview stands in for a pending original at the size and place the
    /// original will have, so the original replaces it without a jump; here
    /// an original smaller than the frame, which is shown at its own size.
    #[gpui::test]
    fn a_pending_original_is_laid_out_at_its_own_size(cx: &mut TestAppContext) {
        use tcode_client::original::{
            OpenedOriginal, OriginalFuture, OriginalOpener, OriginalReader,
        };
        struct Pending;
        impl OriginalReader for Pending {
            fn read(&mut self) -> OriginalFuture<'_, Option<Vec<u8>>> {
                Box::pin(std::future::pending())
            }
            fn cancel(&mut self) {}
        }
        struct Sized;
        impl OriginalOpener for Sized {
            fn open(
                &self,
                _: tcode_protocol::OriginalTarget,
            ) -> OriginalFuture<'static, OpenedOriginal> {
                Box::pin(async {
                    Ok(OpenedOriginal {
                        metadata: tcode_protocol::OriginalMetadata {
                            len: 1024,
                            mime: Some("image/png".into()),
                            dimensions: Some((400, 300)),
                        },
                        reader: Box::new(Pending),
                    })
                })
            }
        }
        let cx = open_viewer(cx, 10, 10);
        cx.simulate_keystrokes("escape");
        draw(cx);
        let (to_host, _control) = async_channel::unbounded();
        let (_replies, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new_local(to_host, from_host, Some(Arc::new(Sized)));
        cx.update(|window, cx| {
            cx.set_global(crate::store::images::HostImages {
                link: Some(link),
                namespace: 1,
            });
            let preview = Arc::new(RenderImage::new(vec![Frame::new(ImageBuffer::from_pixel(
                120,
                90,
                Rgba([255, 0, 0, 255]),
            ))]));
            open(
                ViewerSource::original(
                    tcode_protocol::OriginalTarget::File {
                        path: "/host/small.png".into(),
                    },
                    ImageSource::Render(preview),
                ),
                "small.png",
                window,
                cx,
            );
        });
        draw(cx);
        assert_eq!(image(cx).size, size(px(400.), px(300.)));
    }

    /// An original's bytes replace its preview: the decoded original, smaller
    /// than the frame, is shown at its own size, which neither the preview at
    /// its own size nor the preview standing in at the frame's size has.
    #[gpui::test]
    fn a_delivered_original_replaces_its_preview(cx: &mut TestAppContext) {
        use tcode_client::original::{
            OpenedOriginal, OriginalFuture, OriginalOpener, OriginalReader,
        };
        struct Bytes(Option<Vec<u8>>);
        impl OriginalReader for Bytes {
            fn read(&mut self) -> OriginalFuture<'_, Option<Vec<u8>>> {
                let block = self.0.take();
                Box::pin(async move { Ok(block) })
            }
            fn cancel(&mut self) {}
        }
        struct Delivered(Vec<u8>);
        impl OriginalOpener for Delivered {
            fn open(
                &self,
                _: tcode_protocol::OriginalTarget,
            ) -> OriginalFuture<'static, OpenedOriginal> {
                let bytes = self.0.clone();
                Box::pin(async move {
                    Ok(OpenedOriginal {
                        metadata: tcode_protocol::OriginalMetadata {
                            len: bytes.len() as u64,
                            mime: Some("image/png".into()),
                            dimensions: None,
                        },
                        reader: Box::new(Bytes(Some(bytes))),
                    })
                })
            }
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(400, 300, Rgba([0, 0, 255, 255]))
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode the original");
        let cx = open_viewer(cx, 10, 10);
        cx.simulate_keystrokes("escape");
        draw(cx);
        let (to_host, _control) = async_channel::unbounded();
        let (_replies, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new_local(
            to_host,
            from_host,
            Some(Arc::new(Delivered(png.into_inner()))),
        );
        cx.update(|window, cx| {
            cx.set_global(crate::store::images::HostImages {
                link: Some(link),
                namespace: 1,
            });
            let preview = Arc::new(RenderImage::new(vec![Frame::new(ImageBuffer::from_pixel(
                120,
                90,
                Rgba([255, 0, 0, 255]),
            ))]));
            open(
                ViewerSource::original(
                    tcode_protocol::OriginalTarget::File {
                        path: "/host/delivered.png".into(),
                    },
                    ImageSource::Render(preview),
                ),
                "delivered.png",
                window,
                cx,
            );
        });
        draw(cx);
        assert_eq!(image(cx).size, size(px(400.), px(300.)));
    }

    #[gpui::test]
    fn pointer_gestures_zoom_about_the_pointer_pan_within_the_stage_and_close(
        cx: &mut TestAppContext,
    ) {
        // 400×200 fits the 800×600 window untouched, centred on (400, 300).
        let cx = open_viewer(cx, 400, 200);
        assert_eq!(
            image(cx),
            Bounds::new(point(px(200.), px(200.)), size(px(400.), px(200.)))
        );

        // One wheel notch at the centre grows the image a step, still centred.
        cx.simulate_event(ScrollWheelEvent {
            position: point(px(400.), px(300.)),
            delta: ScrollDelta::Lines(point(0., 1.)),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });
        draw(cx);
        assert_eq!(
            image(cx),
            Bounds::new(point(px(150.), px(175.)), size(px(500.), px(250.)))
        );

        // A pinch at the image's corner doubles it about that corner; now
        // wider than the stage, it is held flush to the left edge instead of
        // leaving a gap, and still centred vertically.
        cx.simulate_event(PinchEvent {
            position: point(px(150.), px(175.)),
            delta: 1.,
            modifiers: Modifiers::default(),
            phase: TouchPhase::Moved,
        });
        draw(cx);
        assert_eq!(
            image(cx),
            Bounds::new(point(px(0.), px(50.)), size(px(1000.), px(500.)))
        );

        // Dragging pans, and the release of a drag is not a click.
        cx.simulate_mouse_down(
            point(px(400.), px(300.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(px(300.), px(300.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(300.), px(300.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        draw(cx);
        assert_eq!(image(cx).origin, point(px(-100.), px(50.)));

        // A trackpad or touch pan, which opens with a touch phase, moves the
        // image too.
        scroll(
            cx,
            ScrollDelta::Pixels(Point::default()),
            TouchPhase::Started,
        );
        scroll(
            cx,
            ScrollDelta::Pixels(point(px(-50.), px(0.))),
            TouchPhase::Moved,
        );
        assert_eq!(image(cx).origin, point(px(-150.), px(50.)));

        // A click beside the image closes the viewer.
        cx.simulate_click(point(px(400.), px(580.)), Modifiers::default());
        draw(cx);
        assert!(cx.debug_bounds("image-viewer-image").is_none());
    }

    #[gpui::test]
    fn pixel_scrolls_zoom_unless_a_touch_phased_pan_or_its_momentum_owns_them(
        cx: &mut TestAppContext,
    ) {
        // 400×200 fits the 800×600 window untouched, centred on (400, 300).
        let cx = open_viewer(cx, 400, 200);

        // A wheel reporting pixels, as browsers and smooth-scrolling mice do,
        // zooms a step per notch even while the image cannot pan.
        scroll(
            cx,
            ScrollDelta::Pixels(point(px(0.), WHEEL_NOTCH)),
            TouchPhase::Moved,
        );
        assert_eq!(rounded(cx), [150., 175., 500., 250.]);

        // A browser's trackpad pinch arrives as a Control-wheel and scales by
        // e per 100 pixels: ln 2 of them double the image.
        cx.simulate_event(ScrollWheelEvent {
            position: point(px(400.), px(300.)),
            delta: ScrollDelta::Pixels(point(px(0.), PINCH_PIXELS * std::f32::consts::LN_2)),
            modifiers: Modifiers::control(),
            touch_phase: TouchPhase::Moved,
        });
        draw(cx);
        assert_eq!(rounded(cx), [-100., 50., 1000., 500.]);

        // A pan's momentum carries no phase yet keeps panning.
        scroll(
            cx,
            ScrollDelta::Pixels(Point::default()),
            TouchPhase::Started,
        );
        scroll(
            cx,
            ScrollDelta::Pixels(point(px(-50.), px(0.))),
            TouchPhase::Moved,
        );
        scroll(cx, ScrollDelta::Pixels(Point::default()), TouchPhase::Ended);
        scroll(
            cx,
            ScrollDelta::Pixels(point(px(-20.), px(0.))),
            TouchPhase::Moved,
        );
        assert_eq!(rounded(cx), [-170., 50., 1000., 500.]);

        // Once it has died away, the next pixel scroll is a wheel again and
        // zooms about the pointer.
        cx.executor().advance_clock(MOMENTUM_GAP * 2);
        scroll(
            cx,
            ScrollDelta::Pixels(point(px(0.), WHEEL_NOTCH)),
            TouchPhase::Moved,
        );
        assert_eq!(rounded(cx), [-312.5, -12.5, 1250., 625.]);
    }

    #[gpui::test]
    fn keys_glide_between_fit_and_actual_size_and_escape_closes(cx: &mut TestAppContext) {
        // 1600×800 fits the 768×480 frame at 0.48: 768×384, centred.
        let cx = open_viewer(cx, 1600, 800);
        let fitted = Bounds::new(point(px(16.), px(108.)), size(px(768.), px(384.)));
        assert_eq!(image(cx), fitted);

        cx.simulate_keystrokes("1");
        draw(cx);
        assert_eq!(image(cx), fitted, "the glide starts from the fit");
        // The frame after the key press began the glide; the next ends it.
        cx.executor().advance_clock(GLIDE);
        draw(cx);
        assert_eq!(
            image(cx),
            Bounds::new(point(px(-400.), px(-100.)), size(px(1600.), px(800.)))
        );

        cx.simulate_keystrokes("0");
        draw(cx);
        cx.executor().advance_clock(GLIDE);
        draw(cx);
        assert_eq!(image(cx), fitted);

        cx.simulate_keystrokes("escape");
        draw(cx);
        assert!(cx.debug_bounds("image-viewer-image").is_none());

        // Contract: Escape cancels an in-flight original even while a dialog
        // frame still holds the viewer's callbacks.
        use tcode_client::original::{
            OpenedOriginal, OriginalFuture, OriginalOpener, OriginalReader,
        };
        struct PendingReader(async_channel::Receiver<Vec<u8>>);
        impl OriginalReader for PendingReader {
            fn read(&mut self) -> OriginalFuture<'_, Option<Vec<u8>>> {
                Box::pin(async move {
                    self.0
                        .recv()
                        .await
                        .map(Some)
                        .map_err(tcode_client::original::error)
                })
            }
            fn cancel(&mut self) {}
        }
        struct PendingOriginal {
            source: std::sync::Mutex<Option<async_channel::Receiver<Vec<u8>>>>,
            opened: async_channel::Sender<()>,
        }
        impl OriginalOpener for PendingOriginal {
            fn open(
                &self,
                _: tcode_protocol::OriginalTarget,
            ) -> OriginalFuture<'static, OpenedOriginal> {
                let source = self.source.lock().unwrap().take().unwrap();
                self.opened.try_send(()).unwrap();
                Box::pin(async move {
                    Ok(OpenedOriginal {
                        metadata: tcode_protocol::OriginalMetadata {
                            len: 1024,
                            mime: Some("image/png".into()),
                            dimensions: None,
                        },
                        reader: Box::new(PendingReader(source)),
                    })
                })
            }
        }
        let (blocks, source) = async_channel::bounded(2);
        let (opened, requests) = async_channel::bounded(1);
        let (to_host, _control) = async_channel::unbounded();
        let (_replies, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new_remote(
            to_host,
            from_host,
            Some(Arc::new(PendingOriginal {
                source: std::sync::Mutex::new(Some(source)),
                opened,
            })),
        );
        cx.update(|window, cx| {
            cx.set_global(crate::store::images::HostImages {
                link: Some(link),
                namespace: 1,
            });
            let preview = Arc::new(RenderImage::new(vec![Frame::new(ImageBuffer::from_pixel(
                240,
                120,
                Rgba([255, 0, 0, 255]),
            ))]));
            open(
                ViewerSource::original(
                    tcode_protocol::OriginalTarget::File {
                        path: "/host/pending.png".into(),
                    },
                    ImageSource::Render(preview),
                ),
                "pending.png",
                window,
                cx,
            );
        });
        draw(cx);
        assert!(
            cx.debug_bounds("image-viewer-image").is_some(),
            "the loaded preview remains visible while the original is pending"
        );
        requests
            .try_recv()
            .expect("explicit open starts the viewer transfer");
        assert!(!blocks.is_closed());
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert!(blocks.is_closed(), "Escape must drop the transfer reader");
    }
}
