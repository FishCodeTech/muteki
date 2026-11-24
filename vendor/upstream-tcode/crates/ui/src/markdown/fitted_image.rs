//! Block images scaled to the available width with their aspect ratio kept.
//!
//! GPUI's `img` turns an auto width and height into the image's pixel size;
//! a `max_w` then narrows the box but leaves its height, and `ObjectFit`
//! letterboxes the picture inside it. This element gives `img` a size that
//! is already proportional, until `img` derives its height from a clamped
//! width itself.

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, Entity, GlobalElementId,
    ImageSource, InspectorElementId, IntoElement, LayoutId, ObjectFit, Pixels, Size, Style, Styled,
    StyledImage as _, WeakEntity, Window, div, img, px, relative, size,
};

use super::state::MarkdownState;

const MAX_HEIGHT: Pixels = px(720.);
const MIN_SIDE: Pixels = px(15.);
/// The size of the tile drawn for an image without a preview.
const UNPREVIEWED: Size<Pixels> = Size {
    width: px(120.),
    height: px(80.),
};

pub(super) struct FittedImage<E> {
    source: ImageSource,
    image: Option<E>,
    owner: WeakEntity<MarkdownState>,
    path: String,
    revision: usize,
    previous_size: Option<Size<Pixels>>,
}

impl<E: IntoElement + Styled + 'static> FittedImage<E> {
    /// `image` is the styled `img` of `source` that is painted; its width
    /// and height are set here.
    pub(super) fn new(
        source: ImageSource,
        image: E,
        owner: &Entity<MarkdownState>,
        path: String,
        cx: &App,
    ) -> Self {
        let (revision, previous_size) = owner.read(cx).image_layout(&path);
        Self {
            source,
            image: Some(image),
            owner: owner.downgrade(),
            path,
            revision,
            previous_size,
        }
    }
}

impl<E: IntoElement + Styled + 'static> IntoElement for FittedImage<E> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E: IntoElement + Styled + 'static> Element for FittedImage<E> {
    type RequestLayoutState = Size<Pixels>;
    type PrepaintState = Option<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let intrinsic = img(self.source.clone())
            .object_fit(ObjectFit::Contain)
            .with_fallback(|| {
                div()
                    .w(UNPREVIEWED.width)
                    .h(UNPREVIEWED.height)
                    .into_any_element()
            })
            .into_any_element()
            .layout_as_root(AvailableSpace::min_size(), window, cx);
        if self.previous_size != Some(intrinsic) {
            let owner = self.owner.clone();
            let path = self.path.clone();
            let revision = self.revision;
            // ListState can be borrowed while its children lay out. Invalidate
            // its cached heights after this frame, not from inside that borrow.
            cx.defer(move |cx| {
                let _ = owner.update(cx, |state, cx| {
                    state.image_resized(path, revision, intrinsic, cx)
                });
            });
        }
        let mut style = Style::default();
        style.max_size.width = relative(1.).into();
        let layout_id = window.request_measured_layout(
            style,
            move |known_dimensions, available_space, _, _| {
                let available_width = known_dimensions.width.or(match available_space.width {
                    AvailableSpace::Definite(width) => Some(width),
                    AvailableSpace::MinContent | AvailableSpace::MaxContent => None,
                });
                fit(intrinsic, available_width)
            },
        );
        (layout_id, intrinsic)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        intrinsic: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let fitted = fit(*intrinsic, Some(bounds.size.width));
        let mut image = self
            .image
            .take()?
            .w(fitted.width)
            .h(fitted.height)
            .into_any_element();
        image.prepaint_as_root(
            bounds.origin,
            size(
                AvailableSpace::Definite(fitted.width),
                AvailableSpace::Definite(fitted.height),
            ),
            window,
            cx,
        );
        Some(image)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        image: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(image) = image {
            image.paint(window, cx);
        }
    }
}

/// The largest size with `intrinsic`'s aspect ratio that fits the available
/// width and [`MAX_HEIGHT`], never larger than `intrinsic` itself. An image that has not
/// loaded holds the tile drawn for an image without a preview, the size it keeps if it
/// never loads; neither the host's preview nor a Mermaid picture reports its size before
/// it is decoded.
fn fit(intrinsic: Size<Pixels>, available_width: Option<Pixels>) -> Size<Pixels> {
    if intrinsic.width <= Pixels::ZERO || intrinsic.height <= Pixels::ZERO {
        return fit(UNPREVIEWED, available_width);
    }
    let ratio = intrinsic.height / intrinsic.width;
    let width = available_width
        .map_or(intrinsic.width, |available| intrinsic.width.min(available))
        .min(MAX_HEIGHT / ratio);
    size(width.max(MIN_SIDE), (width * ratio).max(MIN_SIDE))
}
