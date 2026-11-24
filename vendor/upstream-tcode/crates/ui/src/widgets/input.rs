#[path = "input_configuration.rs"]
mod input_configuration;

use crate::{
    material,
    theme::ActiveTheme as _,
    touch_selection::{EditMenuItem, TouchSelectionOverlay},
    typography::TextRole as _,
    widgets::menu::ContextMenuExt as _,
};
use gpui::{
    Action, AnyElement, App, DefiniteLength, Edges, Entity, Focusable as _, HitboxBehavior,
    InteractiveElement as _, IntoElement, LongPressEvent, MouseUpEvent, ParentElement as _,
    RenderOnce, SharedString, StyleRefinement, Styled, TextAlign, TouchPhase, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::StyledExt as _;
use gpui_base::{InputBase, RoleOverride};
use serde::Deserialize;

/// A right-click menu item of a field. The menu takes focus while it is open,
/// so its choice is forwarded to the field's own focus handle, where the
/// editor's Cut, Copy, Paste and Select All handlers live.
#[derive(Action, Clone, Copy, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_input, no_json)]
enum InputMenu {
    Cut,
    Copy,
    Paste,
    SelectAll,
}

/// What a browser's paste event delivered. A page receives the clipboard only
/// through that event, so on the web this stands in for the editor's Paste,
/// which reads the clipboard itself: the field's owner can take the images
/// before the field inserts the text.
#[derive(Action, Clone, PartialEq)]
#[action(namespace = tcode_input, no_json)]
pub(crate) struct BrowserPaste(pub(crate) gpui::ClipboardItem);

pub use gpui_base::input::{Copy, InputEvent, InputState, Paste, SelectAll, TextareaState};

pub(crate) fn commit_marked_text<M: gpui_base::input::InputModeKind>(
    state: &mut gpui_base::input::InputBaseState<M>,
    window: &mut Window,
    cx: &mut gpui::Context<gpui_base::input::InputBaseState<M>>,
) {
    use gpui::EntityInputHandler as _;
    // gpui-base leaves the mark and undo transaction open on blur;
    // its unmark_text also omits Change, so commit through replacement.
    if let Some(marked) = state.marked_text_range(window, cx)
        && let Some(text) = state.text_for_range(marked.clone(), &mut None, window, cx)
    {
        let selection = state.selected_range();
        state.replace_text_in_range(Some(marked), &text, window, cx);
        state.set_selected_range(selection, cx);
    }
}

#[derive(Clone)]
enum State {
    Input(Entity<InputState>),
    Textarea(Entity<TextareaState>),
}

macro_rules! dispatch_state {
    ($state:expr, |$concrete:ident| $body:expr) => {
        match $state {
            State::Input($concrete) => $body,
            State::Textarea($concrete) => $body,
        }
    };
}

#[derive(IntoElement)]
pub struct Input {
    state: State,
    style: StyleRefinement,
    height: Option<DefiniteLength>,
    appearance: bool,
    bordered: bool,
    disabled: bool,
    tab_index: isize,
    role: RoleOverride,
    aria_label: Option<SharedString>,
    placeholder: Option<SharedString>,
}

impl Input {
    pub fn new(state: &Entity<InputState>) -> Self {
        Self::with_state(State::Input(state.clone()))
    }
    fn from_textarea(state: &Entity<TextareaState>) -> Self {
        Self::with_state(State::Textarea(state.clone()))
    }
    fn with_state(state: State) -> Self {
        Self {
            state,
            style: StyleRefinement::default(),
            height: None,
            appearance: true,
            bordered: true,
            disabled: false,
            tab_index: 0,
            role: RoleOverride::Implicit,
            aria_label: None,
            placeholder: None,
        }
    }
    pub fn appearance(mut self, appearance: bool) -> Self {
        self.appearance = appearance;
        self
    }
    pub fn bordered(mut self, bordered: bool) -> Self {
        self.bordered = bordered;
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    pub fn tab_index(mut self, tab_index: isize) -> Self {
        self.tab_index = tab_index;
        self
    }
    pub fn role(mut self, role: impl Into<RoleOverride>) -> Self {
        self.role = role.into();
        self
    }
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }
    /// The placeholder as of this frame. The state keeps the text it was
    /// given, so a placeholder given only at construction stays in the
    /// language of that moment; given here, a language switch reaches it.
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = Some(placeholder.into());
        self
    }
}

/// The handles and the edit menu of the selection a long press made.
///
/// The menu offers what the native context menu would: Cut, Copy, Paste and
/// Select All, leaving out what cannot apply right now rather than disabling
/// it. Cut, Copy and Paste go through the input's actions, so a custom key
/// binding or a capture handler above the input (the composer's image paste)
/// sees them the same way.
fn render_touch_selection(state: &State, window: &Window, cx: &App) -> Vec<AnyElement> {
    if dispatch_state!(state, |base| base.read(cx).touch_selection()).is_none() {
        return Vec::new();
    }
    let entity_id = dispatch_state!(state, |base| base.entity_id());
    let (capabilities, selectable, focus_handle) = dispatch_state!(state, |base| {
        let base = base.read(cx);
        (
            base.context_menu_capabilities(),
            // Select All has nothing left to offer once every character is
            // selected.
            base.text().len() > 0 && base.selected_range() != (0..base.text().len()),
            base.presentation().focus_handle().clone(),
        )
    });
    let editable = capabilities.is_editable();
    let copyable = capabilities.is_copyable();
    // Rendering must not probe the iOS clipboard without user paste intent.
    // An empty clipboard pastes nothing. A browser hands the clipboard only
    // to its own paste event, never to a menu the page draws.
    let pasteable = editable && !cfg!(target_family = "wasm");

    let dispatch = move |action: &dyn Action, window: &mut Window, cx: &mut App| {
        focus_handle.dispatch_action(action, window, cx);
    };
    let mut items = Vec::with_capacity(4);
    if editable && copyable {
        let dispatch = dispatch.clone();
        items.push(EditMenuItem::new(
            "cut",
            crate::tr!("edit_menu.cut").into_owned(),
            move |window, cx| dispatch(&gpui_base::input::Cut, window, cx),
        ));
    }
    if copyable {
        let dispatch = dispatch.clone();
        let state = state.clone();
        items.push(EditMenuItem::new(
            "copy",
            crate::tr!("edit_menu.copy").into_owned(),
            move |window, cx| {
                dispatch(&Copy, window, cx);
                dispatch_state!(&state, |base| base
                    .update(cx, |base, cx| base.close_edit_menu(cx)));
            },
        ));
    }
    if pasteable {
        let dispatch = dispatch.clone();
        items.push(
            EditMenuItem::new(
                "paste",
                crate::tr!("edit_menu.paste").into_owned(),
                move |window, cx| dispatch(&Paste, window, cx),
            )
            .native_paste(),
        );
    }
    if selectable {
        let state = state.clone();
        items.push(EditMenuItem::new(
            "select-all",
            crate::tr!("edit_menu.select_all").into_owned(),
            move |window, cx| {
                dispatch_state!(&state, |base| base
                    .update(cx, |base, cx| base.select_all_from_edit_menu(window, cx)))
            },
        ));
    }

    let drag_state = state.clone();
    let source_state = state.clone();
    TouchSelectionOverlay::new(("input-touch-selection", entity_id), move |_, cx| {
        dispatch_state!(&source_state, |base| base.read(cx).touch_selection()).map(Into::into)
    })
    .handles(move |edge, phase, position, _, cx| {
        dispatch_state!(&drag_state, |base| base.update(
            cx,
            |base, cx| match phase {
                TouchPhase::Started => base.begin_edge_drag(edge, position, cx),
                TouchPhase::Moved => base.update_edge_drag(position, cx),
                TouchPhase::Ended | TouchPhase::Cancelled => base.end_edge_drag(cx),
            }
        ))
    })
    .items(items)
    .into_elements(window, cx)
}
impl Styled for Input {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Input {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let multi_line = matches!(self.state, State::Textarea(_));
        let compact = crate::window_seam::window_is_compact(window, cx);
        dispatch_state!(&self.state, |state| state
            .update(cx, |state, cx| state.prepare(window, cx)));
        if let Some(placeholder) = self.placeholder {
            dispatch_state!(&self.state, |base| {
                // Setting notifies, so it is set only when it changed.
                if base.read(cx).presentation().placeholder() != &placeholder {
                    base.update(cx, |state, cx| {
                        state.set_placeholder(placeholder, window, cx)
                    });
                }
            });
        }
        let focused = dispatch_state!(&self.state, |base| base
            .read(cx)
            .focus_handle(cx)
            .is_focused(window))
            && !self.disabled;
        #[cfg(target_os = "ios")]
        if focused {
            gpui_ios::set_paste_input(dispatch_state!(&self.state, |base| base
                .entity_id()
                .as_u64()));
            gpui_ios::set_paste_control(None);
        }
        if !focused {
            dispatch_state!(&self.state, |base| base
                .update(cx, |state, cx| commit_marked_text(state, window, cx)));
        }
        let theme = cx.theme().clone();
        dispatch_state!(&self.state, |base| base.update(cx, |state, cx| {
            state.set_editor_style(gpui_base::input::InputEditorStyle {
                foreground: theme.foreground,
                muted_foreground: theme.muted_foreground,
                background: theme.background,
                border: theme.border,
                selection: theme.selection,
                caret: theme.foreground,
                highlight_styles: theme.highlight_theme.clone(),
                ..Default::default()
            });
            state.set_editor_paddings(if multi_line {
                Edges {
                    left: px(10.),
                    right: px(10.),
                    top: px(8.),
                    bottom: px(8.),
                }
            } else {
                Edges::default()
            });
            state.set_disabled(self.disabled, cx);
            state.set_text_align(self.style.text.text_align.unwrap_or(TextAlign::Left), cx);
        }));
        let placeholder = dispatch_state!(&self.state, |base| base
            .read(cx)
            .presentation()
            .placeholder()
            .clone());
        let aria_label = self
            .aria_label
            .or_else(|| (!placeholder.is_empty()).then_some(placeholder));
        // The selection's controls belong to the field the finger is in:
        // a field that lost focus to another keeps its selection, as any
        // input does, but not the handles and the menu over it.
        let touch_selection = if focused {
            render_touch_selection(&self.state, window, cx)
        } else {
            Vec::new()
        };
        dispatch_state!(&self.state, |base| {
            let editor = if multi_line {
                // Give the editor a definite viewport before InputBase measures its
                // soft-wrap width. Mounting it directly as a horizontal flex child
                // lets mixed-script intrinsic widths leak into that measurement.
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .min_w_0()
                    .child(div().relative().flex_1().min_w_0().child(base.clone()))
                    .into_any_element()
            } else {
                base.clone().into_any_element()
            };
            let input_entity = base.clone();
            let focus = base.read(cx).focus_handle(cx);
            let menu_focus = focus.clone();
            let menu_state = base.clone();
            let paste_state = base.clone();
            let disabled = self.disabled;
            InputBase::new(("input", base.entity_id()))
                .on_action(move |paste: &BrowserPaste, window, cx| {
                    paste_state.update(cx, |state, cx| {
                        gpui::EntityInputHandler::paste(state, paste.0.clone(), window, cx)
                    })
                })
                .on_action(move |action: &InputMenu, window, cx| match action {
                    InputMenu::Cut => {
                        menu_focus.dispatch_action(&gpui_base::input::Cut, window, cx)
                    }
                    InputMenu::Copy => menu_focus.dispatch_action(&Copy, window, cx),
                    InputMenu::Paste => menu_focus.dispatch_action(&Paste, window, cx),
                    InputMenu::SelectAll => menu_focus.dispatch_action(&SelectAll, window, cx),
                })
                .focused(focused)
                .disabled(self.disabled)
                // GPUI gives focus to any tracked handle under a press, and a
                // focused handle with an input handler raises the soft
                // keyboard; a field that takes no text is not a focus target.
                .when(disabled, |this| {
                    this.capture_any_mouse_down(|_, window, _| window.prevent_default())
                })
                .role(self.role)
                .styles(|styles| styles.focused(|style| style.border_color(theme.ring)))
                .when_some(aria_label.clone(), |this, label| {
                    this.accessibility_label(label)
                })
                .relative()
                .flex()
                .size_full()
                .items_center()
                // The editor inherits ambient typography unless the frame pins it.
                .text_token(&theme.tokens.typography.sm)
                .rounded(material::radius_input(cx))
                .when(!multi_line, |this| {
                    this.h(px(material::input_height(compact))).px(px(10.))
                })
                // Multi-line insets come from the editor paddings above; padding
                // the container as well doubles them.
                .when(multi_line, |this| {
                    this.h_auto()
                        .when_some(self.height, |this, height| this.h(height))
                })
                .when(self.appearance, |this| {
                    this.bg(theme.background).when(self.bordered, |this| {
                        this.border_1().border_color(theme.input)
                    })
                })
                .refine_style(&self.style)
                .child(editor)
                // Register after the editor paints so GPUI uses its configured handler.
                .child(
                    gpui::canvas(
                        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                        move |bounds, hitbox, window, cx| {
                            // gpui-fast-web summons the browser keyboard for a
                            // tap only when the tap was not default-prevented,
                            // but GPUI's focus transfer prevents default on
                            // every press into a focusable element. A tap into
                            // the focused field after the keyboard was
                            // dismissed would otherwise leave it closed.
                            if cfg!(target_family = "wasm") && !disabled {
                                let focus = focus.clone();
                                window.on_mouse_event(move |_: &MouseUpEvent, phase, window, _| {
                                    if phase.bubble()
                                        && hitbox.is_hovered(window)
                                        && focus.is_focused(window)
                                    {
                                        window.request_virtual_keyboard();
                                    }
                                });
                            }
                            // A long press here selects in this field. The
                            // editor, which bubbles after this, claims every
                            // press that reaches it here, but opens the menu
                            // only once the finger lifts; the haptic says now
                            // that the press has taken.
                            window.on_mouse_event(
                                move |event: &LongPressEvent, phase, window, _| {
                                    if phase.bubble()
                                        && event.phase == TouchPhase::Started
                                        && !window.default_prevented()
                                        && bounds.contains(&event.start_position)
                                        && !disabled
                                    {
                                        crate::haptics::long_press();
                                    }
                                },
                            );
                            let bounds = input_entity.read(cx).text_bounds().unwrap_or(bounds);
                            window.handle_input(
                                &focus,
                                input_configuration::ConfiguredInput::new(
                                    bounds,
                                    input_entity.clone(),
                                ),
                                cx,
                            );
                        },
                    )
                    .absolute()
                    .size_full(),
                )
                // The handles float above the field and the menu above the
                // window; both are deferred, so they draw over the editor.
                .children(touch_selection)
                // The editor already moved the caret to the press; what the
                // menu offers follows what the field can do right now, as the
                // touch edit menu does, with items disabled rather than left
                // out so the menu keeps one shape.
                .context_menu(move |menu, _, cx| {
                    if disabled {
                        return menu;
                    }
                    let (capabilities, has_text) = {
                        let base = menu_state.read(cx);
                        (base.context_menu_capabilities(), base.text().len() > 0)
                    };
                    let editable = capabilities.is_editable();
                    let copyable = capabilities.is_copyable();
                    let has_selection = capabilities.has_selection();
                    menu.menu_with_enable(
                        crate::tr!("edit_menu.cut").into_owned(),
                        Box::new(InputMenu::Cut),
                        editable && copyable && has_selection,
                    )
                    .menu_with_enable(
                        crate::tr!("edit_menu.copy").into_owned(),
                        Box::new(InputMenu::Copy),
                        copyable && has_selection,
                    )
                    // Offered whenever the text can change, without peeking
                    // at the clipboard: an empty clipboard pastes nothing. A
                    // browser hands the clipboard only to its own paste
                    // event, never to a menu the page draws.
                    .when(!cfg!(target_family = "wasm"), |menu| {
                        menu.menu_with_enable(
                            crate::tr!("edit_menu.paste").into_owned(),
                            Box::new(InputMenu::Paste),
                            editable,
                        )
                    })
                    .separator()
                    .menu_with_enable(
                        crate::tr!("edit_menu.select_all").into_owned(),
                        Box::new(InputMenu::SelectAll),
                        has_text,
                    )
                })
                .into_any_element()
        })
    }
}

#[derive(IntoElement)]
pub struct Textarea {
    input: Input,
}
impl Textarea {
    pub fn new(state: &Entity<TextareaState>) -> Self {
        Self {
            input: Input::from_textarea(state),
        }
    }
    pub fn appearance(mut self, value: bool) -> Self {
        self.input = self.input.appearance(value);
        self
    }
    pub fn bordered(mut self, value: bool) -> Self {
        self.input = self.input.bordered(value);
        self
    }
    pub fn disabled(mut self, value: bool) -> Self {
        self.input = self.input.disabled(value);
        self
    }
    pub fn h(mut self, value: impl Into<DefiniteLength>) -> Self {
        self.input.height = Some(value.into());
        self
    }
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.input = self.input.aria_label(label);
        self
    }
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.input = self.input.placeholder(placeholder);
        self
    }
}
impl Styled for Textarea {
    fn style(&mut self) -> &mut StyleRefinement {
        self.input.style()
    }
}
impl RenderOnce for Textarea {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.input
    }
}

/// The numeric form field; gpui-base owns stepping and keyboard handling.
#[derive(IntoElement)]
pub struct NumberInput {
    state: Entity<InputState>,
    accessibility_label: Option<SharedString>,
}

impl NumberInput {
    pub fn new(state: &Entity<InputState>) -> Self {
        Self {
            state: state.clone(),
            accessibility_label: None,
        }
    }
    pub fn accessibility_label(mut self, label: impl Into<SharedString>) -> Self {
        self.accessibility_label = Some(label.into());
        self
    }
}

impl RenderOnce for NumberInput {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        let focused = self.state.read(cx).focus_handle(cx).is_focused(window);
        gpui_base::NumberInput::new(&self.state)
            .when_some(self.accessibility_label, |this, label| {
                this.accessibility_label(label)
            })
            .controls_right()
            .w(px(material::NUMBER_INPUT_WIDTH))
            .h(px(material::input_height(compact)))
            .rounded(material::radius_input(cx))
            .bg(cx.theme().background)
            .border_1()
            .border_color(if focused {
                cx.theme().ring
            } else {
                cx.theme().input
            })
            .input(Input::new(&self.state).appearance(false).h_full())
            .increment_button(|button| {
                button
                    .w_4()
                    .items_center()
                    .justify_center()
                    .child(crate::icon::Icon::new(crate::icon::IconName::ChevronUp).size_3())
            })
            .decrement_button(|button| {
                button
                    .w_4()
                    .items_center()
                    .justify_center()
                    .child(crate::icon::Icon::new(crate::icon::IconName::ChevronDown).size_3())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        AppContext as _, Context, EntityInputHandler as _, Modifiers, Render, TestAppContext,
    };
    use std::{cell::Cell, rc::Rc};

    struct Field(Entity<TextareaState>, bool);
    impl Render for Field {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Textarea::new(&self.0).disabled(self.1)
        }
    }

    // GPUI focuses any tracked handle under a press, and the platform raises
    // the soft keyboard for a focused handle with an input handler, so a
    // disabled field has to refuse the press to stay keyboard-free on a phone.
    #[gpui::test]
    fn a_press_focuses_an_enabled_field_but_not_a_disabled_one(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        for disabled in [false, true] {
            let (field, cx) = cx.add_window_view(|window, cx| {
                Field(cx.new(|cx| TextareaState::new(window, cx)), disabled)
            });
            let input = field.read_with(cx, |field, _| field.0.clone());
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let bounds = input
                .read_with(cx, |state, _| state.text_bounds())
                .expect("the field was painted");
            cx.simulate_mouse_down(
                bounds.center(),
                gpui::MouseButton::Left,
                Modifiers::default(),
            );
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let focused =
                cx.update(|window, cx| input.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(focused, !disabled, "disabled={disabled}");
        }
    }

    #[gpui::test]
    fn blur_or_disable_commits_preedit_before_the_native_handler_is_removed(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::theme::init);
        for disable in [false, true] {
            let (field, cx) = cx.add_window_view(|window, cx| {
                Field(cx.new(|cx| TextareaState::new(window, cx)), false)
            });
            let input = field.read_with(cx, |field, _| field.0.clone());
            let committed = Rc::new(Cell::new(false));
            let observed = committed.clone();
            let _subscription = cx.update(|_, cx| {
                cx.subscribe(&input, move |_, event, _| {
                    if matches!(event, InputEvent::Change) {
                        observed.set(true);
                    }
                })
            });
            cx.update(|window, cx| {
                input.update(cx, |state, cx| {
                    state.focus(window, cx);
                    state.replace_and_mark_text_in_range(None, "你好", Some(0..0), window, cx);
                });
                window.draw(cx).clear(cx);
                if disable {
                    field.update(cx, |field, cx| {
                        field.1 = true;
                        cx.notify();
                    });
                } else {
                    window.blur(cx);
                }
                window.draw(cx).clear(cx);
            });
            assert!(
                committed.get(),
                "leaving the editable field must publish the completed draft (disable={disable})"
            );
            cx.update(|window, cx| {
                field.update(cx, |field, cx| {
                    field.1 = false;
                    cx.notify();
                });
                window.draw(cx).clear(cx);
                input.update(cx, |state, cx| {
                    assert_eq!(state.marked_text_range(window, cx), None);
                    assert_eq!(state.selected_range(), 0..0);
                    state.focus(window, cx);
                });
                window.draw(cx).clear(cx);
            });
            cx.simulate_input("!");
            assert_eq!(input.read_with(cx, |state, _| state.value()), "!你好");
        }
    }
}
