use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, FocusHandle, InteractiveElement as _, IntoElement, ParentElement,
    Pixels, RenderOnce, SharedString, StyleRefinement, Styled, WeakFocusHandle, Window, div,
    prelude::FluentBuilder as _, px,
};

use crate::{
    icon::IconName,
    sizing::Sizable as _,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{Button, ButtonVariant, ButtonVariants as _},
};
use gpui_base::StyledExt as _;

use super::OverlayExt as _;

/// gpui-base's `Dialog` is an unstyled host with no width; these two widths
/// are Tcode's, named here once. Forms take the default; content dialogs take
/// [`Dialog::wide`], which matches the palette so the app has one wide
/// overlay width.
const DIALOG_WIDTH: f32 = 448.;
const DIALOG_WIDE_WIDTH: f32 = 640.;

/// gpui `list`/`uniform_list` need a definite height and `DialogContent` is
/// `flex_1`, which the builder cannot read back; the two list dialogs (ACP
/// agents, plugins) share this one height.
pub(crate) const DIALOG_LIST_HEIGHT: f32 = 456.;

type Decision = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App) -> bool>;
type Closed = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type ContentBuilder = Rc<dyn Fn(DialogContent, &mut Window, &mut App) -> DialogContent>;

pub(crate) struct ActiveDialog {
    pub focus_handle: FocusHandle,
    pub previous_focus_handle: Option<WeakFocusHandle>,
    pub builder: DialogBuilder,
}

pub type DialogBuilder = Rc<dyn Fn(Dialog, &mut Window, &mut App) -> Dialog>;

#[derive(Clone)]
pub struct DialogButtons {
    ok_text: Option<SharedString>,
    cancel_text: Option<SharedString>,
    ok_variant: ButtonVariant,
    cancel_variant: ButtonVariant,
    show_cancel: bool,
    on_ok: Decision,
    on_cancel: Decision,
    on_close: Closed,
}

impl Default for DialogButtons {
    fn default() -> Self {
        Self {
            ok_text: None,
            cancel_text: None,
            ok_variant: ButtonVariant::Primary,
            cancel_variant: ButtonVariant::Default,
            show_cancel: false,
            on_ok: Rc::new(|_, _, _| true),
            on_cancel: Rc::new(|_, _, _| true),
            on_close: Rc::new(|_, _, _| {}),
        }
    }
}

impl DialogButtons {
    pub fn ok_text(mut self, text: impl Into<SharedString>) -> Self {
        self.ok_text = Some(text.into());
        self
    }
    pub fn cancel_text(mut self, text: impl Into<SharedString>) -> Self {
        self.cancel_text = Some(text.into());
        self
    }
    pub fn ok_variant(mut self, variant: ButtonVariant) -> Self {
        self.ok_variant = variant;
        self
    }
    pub fn cancel_variant(mut self, variant: ButtonVariant) -> Self {
        self.cancel_variant = variant;
        self
    }
    pub fn show_cancel(mut self, show: bool) -> Self {
        self.show_cancel = show;
        self
    }
    fn on_ok(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.on_ok = Rc::new(handler);
        self
    }
    fn on_cancel(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.on_cancel = Rc::new(handler);
        self
    }
}

#[derive(IntoElement)]
pub struct DialogContent {
    base: gpui::Div,
    children: Vec<AnyElement>,
}

impl DialogContent {
    fn new() -> Self {
        Self {
            base: div(),
            children: Vec::new(),
        }
    }
}

impl ParentElement for DialogContent {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl Styled for DialogContent {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl RenderOnce for DialogContent {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.base.flex_1().min_h_0().children(self.children)
    }
}

#[derive(IntoElement)]
pub struct DialogActions {
    style: StyleRefinement,
    leading: Vec<AnyElement>,
    children: Vec<AnyElement>,
}

impl DialogActions {
    pub fn new() -> Self {
        Self {
            style: StyleRefinement::default(),
            leading: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Actions on the left of the row, wrapping before they push the trailing
    /// actions (the element's children) off the right edge.
    pub fn leading(mut self, children: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.leading
            .extend(children.into_iter().map(IntoElement::into_any_element));
        self
    }
}

impl Default for DialogActions {
    fn default() -> Self {
        Self::new()
    }
}

impl ParentElement for DialogActions {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl Styled for DialogActions {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for DialogActions {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        gpui_base::h_flex()
            .items_center()
            .gap_2()
            .justify_end()
            .refine_style(&self.style)
            .when(!self.leading.is_empty(), |el| {
                el.child(
                    gpui_base::h_flex()
                        .flex_1()
                        .flex_wrap()
                        .gap_2()
                        .children(self.leading),
                )
            })
            .children(self.children)
    }
}

enum DialogBase {
    Dialog(gpui_base::Dialog),
    Alert(gpui_base::AlertDialog),
}

impl DialogBase {
    // Internal fan-out straight into the base builder calls; a params struct
    // would be used exactly once.
    #[allow(clippy::too_many_arguments)]
    fn render(
        self,
        layer: usize,
        topmost: bool,
        focus: FocusHandle,
        keyboard: bool,
        overlay_closable: bool,
        backdrop: impl IntoElement,
        popup: impl IntoElement,
        on_ok: Decision,
        on_cancel: Decision,
        on_close: Closed,
    ) -> AnyElement {
        match self {
            Self::Dialog(base) => base
                .layer(layer, topmost)
                .focus_handle(focus)
                .close_on_escape(keyboard)
                .close_on_backdrop_press(overlay_closable)
                .backdrop(backdrop)
                .popup(popup)
                .on_ok(move |event, window, cx| on_ok(event, window, cx))
                .on_cancel(move |event, window, cx| on_cancel(event, window, cx))
                .on_close(move |event, window, cx| on_close(event, window, cx))
                .request_close(|_, window, cx| window.close_dialog(cx))
                .into_any_element(),
            Self::Alert(base) => base
                .layer(layer, topmost)
                .focus_handle(focus)
                .close_on_escape(keyboard)
                .backdrop(backdrop)
                .popup(popup)
                .on_ok(move |event, window, cx| on_ok(event, window, cx))
                .on_cancel(move |event, window, cx| on_cancel(event, window, cx))
                .on_close(move |event, window, cx| on_close(event, window, cx))
                .request_close(|_, window, cx| window.close_dialog(cx))
                .into_any_element(),
        }
    }
}

#[derive(IntoElement)]
pub struct Dialog {
    base: DialogBase,
    style: StyleRefinement,
    title: Option<AnyElement>,
    footer: Option<AnyElement>,
    content: Option<ContentBuilder>,
    children: Vec<AnyElement>,
    width: Pixels,
    max_width: Option<Pixels>,
    close_button: bool,
    fill: bool,
    overlay: bool,
    overlay_closable: bool,
    keyboard: bool,
    button_props: DialogButtons,
    focus_handle: FocusHandle,
    layer: usize,
    topmost: bool,
}

impl Dialog {
    pub fn new(cx: &mut App) -> Self {
        Self {
            base: DialogBase::Dialog(gpui_base::Dialog::new(cx)),
            style: StyleRefinement::default(),
            title: None,
            footer: None,
            content: None,
            children: Vec::new(),
            width: px(DIALOG_WIDTH),
            max_width: None,
            close_button: true,
            fill: false,
            overlay: true,
            overlay_closable: true,
            keyboard: true,
            button_props: DialogButtons::default(),
            focus_handle: cx.focus_handle(),
            layer: 0,
            topmost: true,
        }
    }
    fn alert(cx: &mut App) -> Self {
        let mut this = Self::new(cx);
        this.base = DialogBase::Alert(gpui_base::AlertDialog::new(cx));
        this.close_button = false;
        this.overlay_closable = false;
        this
    }
    pub fn title(mut self, title: impl IntoElement) -> Self {
        self.title = Some(title.into_any_element());
        self
    }
    pub fn content<F>(mut self, builder: F) -> Self
    where
        F: Fn(DialogContent, &mut Window, &mut App) -> DialogContent + 'static,
    {
        self.content = Some(Rc::new(builder));
        self
    }
    pub fn footer(mut self, footer: impl IntoElement) -> Self {
        self.footer = Some(footer.into_any_element());
        self
    }
    pub fn button_props(mut self, props: DialogButtons) -> Self {
        self.button_props = props;
        self
    }
    pub fn close_button(mut self, value: bool) -> Self {
        self.close_button = value;
        self
    }

    /// Span the whole window instead of floating a card: the surface brings
    /// its own chrome, as the image viewer does.
    pub fn fill(mut self) -> Self {
        self.fill = true;
        self
    }
    pub fn w(mut self, width: impl Into<Pixels>) -> Self {
        self.width = width.into();
        self
    }
    pub fn width(self, width: impl Into<Pixels>) -> Self {
        self.w(width)
    }
    /// The content-dialog width; forms keep the default.
    pub fn wide(self) -> Self {
        self.w(px(DIALOG_WIDE_WIDTH))
    }
    pub fn max_w(mut self, width: impl Into<Pixels>) -> Self {
        self.max_width = Some(width.into());
        self
    }
    pub fn overlay(mut self, value: bool) -> Self {
        self.overlay = value;
        self
    }
    pub fn overlay_closable(mut self, value: bool) -> Self {
        self.overlay_closable = value;
        self
    }
    pub fn keyboard(mut self, value: bool) -> Self {
        self.keyboard = value;
        self
    }
    pub fn on_ok(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.button_props = self.button_props.on_ok(handler);
        self
    }
    pub fn on_cancel(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.button_props = self.button_props.on_cancel(handler);
        self
    }
    pub fn on_close(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.button_props.on_close = Rc::new(handler);
        self
    }
    pub(crate) fn layer(mut self, layer: usize, topmost: bool) -> Self {
        self.layer = layer;
        self.topmost = topmost;
        self
    }
    pub(crate) fn focus_handle(mut self, focus: FocusHandle) -> Self {
        self.focus_handle = focus;
        self
    }
}

impl ParentElement for Dialog {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl Styled for Dialog {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Dialog {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let viewport = window.viewport_size();
        // Every dialog carries a desktop design width, and the same views now
        // open in phone-sized and browser viewports. Clamp here rather than at
        // each call site: a wider-than-viewport dialog is also positioned
        // off-screen, because `x` is derived from the same width. The height cap
        // below keeps the footer actions reachable; `DialogContent` is already
        // `flex_1 min_h_0`, so a scrolling body shrinks into it.
        let width = crate::sizing::fit_viewport(f32::from(self.width), viewport.width);
        let x = viewport.width / 2. - width / 2.;
        let inset = cx.theme().tokens.spacing.lg;
        let y = viewport.height / 10. + inset * self.layer as f32;
        let content = self
            .content
            .map(|builder| builder(DialogContent::new(), window, cx));
        // gpui-base's backdrop parent dismisses but does not block; the scrim stops wheels without occluding it.
        let backdrop = div()
            .absolute()
            .size_full()
            .when(self.overlay, |el| el.bg(crate::material::scrim(1., cx)))
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
        let popup = div()
            .id(("tcode-dialog", self.layer))
            .absolute()
            .map(|el| {
                if self.fill {
                    return el.inset_0().overflow_hidden();
                }
                el.left(x)
                    .top(y)
                    .w(width)
                    .when_some(self.max_width, |el, width| el.max_w(width))
                    .max_h(viewport.height - y - inset)
                    .min_h_24()
                    .flex()
                    .flex_col()
                    .gap(inset)
                    .p(inset)
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(crate::material::radius_overlay(cx))
                    .shadow_xl()
            })
            .occlude()
            .refine_style(&self.style)
            .when_some(self.title, |el, title| {
                // `pr_8` keeps a long title clear of the close button.
                el.child(
                    div()
                        .pr_8()
                        .text_token(&cx.theme().tokens.typography.lg)
                        .font_semibold()
                        .child(title),
                )
            })
            .when_some(content, |el, content| el.child(content))
            .when(!self.children.is_empty(), |el| el.children(self.children))
            .when_some(self.footer, |el, footer| el.child(footer))
            .when(self.close_button, |el| {
                el.child(
                    gpui_base::DialogClose::new()
                        .absolute()
                        .top_2()
                        .right_2()
                        .child(
                            Button::new(("dialog-close-button", self.layer))
                                .ghost()
                                .small()
                                .icon(IconName::Close),
                        ),
                )
            });
        self.base.render(
            self.layer,
            self.topmost,
            self.focus_handle,
            self.keyboard,
            self.overlay_closable,
            backdrop,
            popup,
            self.button_props.on_ok,
            self.button_props.on_cancel,
            self.button_props.on_close,
        )
    }
}

#[derive(IntoElement)]
pub struct AlertDialog {
    base: Dialog,
    title: Option<AnyElement>,
    description: Option<AnyElement>,
    footer: Option<AnyElement>,
    button_props: DialogButtons,
}

impl AlertDialog {
    pub fn new(cx: &mut App) -> Self {
        Self {
            base: Dialog::alert(cx),
            title: None,
            description: None,
            footer: None,
            button_props: DialogButtons::default(),
        }
    }
    pub fn title(mut self, title: impl IntoElement) -> Self {
        self.title = Some(title.into_any_element());
        self
    }
    pub fn description(mut self, description: impl IntoElement) -> Self {
        self.description = Some(description.into_any_element());
        self
    }
    pub fn footer(mut self, footer: impl IntoElement) -> Self {
        self.footer = Some(footer.into_any_element());
        self
    }
    pub fn button_props(mut self, props: DialogButtons) -> Self {
        self.button_props = props;
        self
    }
    pub fn width(mut self, width: impl Into<Pixels>) -> Self {
        self.base = self.base.width(width);
        self
    }
    pub fn wide(mut self) -> Self {
        self.base = self.base.wide();
        self
    }
    pub fn show_cancel(mut self, value: bool) -> Self {
        self.button_props.show_cancel = value;
        self
    }
    pub fn close_button(mut self, value: bool) -> Self {
        self.base = self.base.close_button(value);
        self
    }
    pub fn keyboard(mut self, value: bool) -> Self {
        self.base = self.base.keyboard(value);
        self
    }
    pub fn on_ok(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.button_props = self.button_props.on_ok(handler);
        self
    }
    pub fn on_cancel(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.button_props = self.button_props.on_cancel(handler);
        self
    }
    pub fn on_close(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.base = self.base.on_close(handler);
        self
    }

    pub(crate) fn build_surface(mut self, _window: &mut Window, cx: &mut App) -> Dialog {
        let on_ok = self.button_props.on_ok.clone();
        let on_cancel = self.button_props.on_cancel.clone();
        let cancel = self.button_props.show_cancel.then(|| {
            let label = self
                .button_props
                .cancel_text
                .clone()
                .unwrap_or_else(|| "Cancel".into());
            Button::new("alert-cancel")
                .label(label)
                .with_variant(self.button_props.cancel_variant)
                .on_click(move |event, window, cx| {
                    if on_cancel(event, window, cx) {
                        window.close_dialog(cx);
                    }
                })
                .into_any_element()
        });
        let ok_label = self
            .button_props
            .ok_text
            .clone()
            .unwrap_or_else(|| "OK".into());
        let ok = Button::new("alert-ok")
            .label(ok_label)
            .with_variant(self.button_props.ok_variant)
            .on_click(move |event, window, cx| {
                if on_ok(event, window, cx) {
                    window.close_dialog(cx);
                }
            });
        let footer = self.footer.take().unwrap_or_else(|| {
            DialogActions::new()
                .children(cancel)
                .child(ok)
                .into_any_element()
        });
        let typography = &cx.theme().tokens.typography;
        self.base.button_props = self.button_props;
        self.base.title = None;
        self.base.footer = Some(footer);
        self.base.children.push(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .when_some(self.title, |el, title| {
                    el.child(
                        div()
                            .text_token(&typography.lg)
                            .font_semibold()
                            .child(title),
                    )
                })
                .when_some(self.description, |el, description| {
                    el.child(div().text_token(&typography.sm).child(description))
                })
                .into_any_element(),
        );
        self.base
    }
}

impl Styled for AlertDialog {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.base.style
    }
}

impl RenderOnce for AlertDialog {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        self.build_surface(window, cx)
    }
}
