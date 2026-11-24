//! Tcode's single-choice form control: a styled composition of
//! `gpui_base::RadioGroup`/`Radio`, drawn as a
//! [`TabBar::segmented`](super::tabs::TabBar::segmented) track.
use crate::{
    icon::{Icon, IconName},
    material::focus_ring,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        Tooltip,
        tabs::{segment, segmented_track},
    },
};
use gpui::{
    App, Axis, ElementId, InteractiveElement as _, IntoElement, ParentElement as _, RenderOnce,
    SharedString, StatefulInteractiveElement as _, StyleRefinement, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{Radio, RadioGroup, StyledExt as _};
use std::rc::Rc;

type SelectHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// One option of a [`ChoiceGroup`].
pub struct Choice {
    id: ElementId,
    label: SharedString,
    selected: bool,
    disabled: bool,
    recommended: bool,
    on_select: Option<SelectHandler>,
}

impl Choice {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            selected: false,
            disabled: false,
            recommended: false,
            on_select: None,
        }
    }
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    /// Marks the option the provider or host recommends.
    pub fn recommended(mut self, recommended: bool) -> Self {
        self.recommended = recommended;
        self
    }
    /// Called when the user picks this option while another is selected.
    pub fn on_select(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }
}

/// A row of mutually exclusive options. It carries no label of its own; the
/// form places one above it.
///
/// Its own styles place the group (width, flex, margins); the track owns
/// everything inside it.
#[derive(IntoElement)]
pub struct ChoiceGroup {
    id: ElementId,
    compact: bool,
    aria_label: Option<SharedString>,
    choices: Vec<Choice>,
    style: StyleRefinement,
}

impl ChoiceGroup {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            compact: false,
            aria_label: None,
            choices: Vec::new(),
            style: StyleRefinement::default(),
        }
    }
    /// Touch geometry, as [`TabBar::compact`](super::tabs::TabBar::compact).
    pub fn compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }
    pub fn choice(mut self, choice: Choice) -> Self {
        self.choices.push(choice);
        self
    }
    pub fn choices(mut self, choices: impl IntoIterator<Item = Choice>) -> Self {
        self.choices.extend(choices);
        self
    }
}

impl Styled for ChoiceGroup {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for ChoiceGroup {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            compact,
            aria_label,
            choices,
            style,
        } = self;
        let count = choices.len();
        let radios = choices
            .into_iter()
            .enumerate()
            .map(|(ix, choice)| render_choice(choice, ix, count, compact, cx));
        let group = RadioGroup::new(id.clone())
            .axis(Axis::Horizontal)
            .when_some(aria_label, |this, label| this.aria_label(label))
            .children(radios);
        segmented_track(id.clone(), group, compact, cx)
            .debug_selector(|| id.to_string())
            .refine_style(&style)
    }
}

fn render_choice(choice: Choice, ix: usize, count: usize, compact: bool, cx: &App) -> Radio {
    let Choice {
        id,
        label,
        selected,
        disabled,
        recommended,
        on_select,
    } = choice;
    let theme = cx.theme();
    let ring = focus_ring(cx);
    let accessible: SharedString = if recommended {
        crate::tr!("choice.recommended_option", label = label.clone())
            .into_owned()
            .into()
    } else {
        label.clone()
    };
    let selector = id.to_string();
    let radio = Radio::new(id)
        .debug_selector(move || selector)
        .checked(selected)
        .disabled(disabled)
        .set_position(ix + 1, count)
        .accessibility_label(accessible)
        .focus_visible(move |style| style.shadow(vec![ring]))
        .flex()
        .items_center()
        .px(px(if compact { 14. } else { 12. }))
        .gap(px(if compact { 8. } else { 6. }))
        .text_body(compact, cx)
        .font_medium()
        .whitespace_nowrap()
        .text_color(theme.muted_foreground)
        .when(!disabled, |this| this.cursor_pointer())
        .styles(|styles| styles.disabled(|style| style.opacity(0.5)))
        .when_some(on_select.filter(|_| !disabled), |this, handler| {
            this.on_change(move |_, _, window, cx| handler(window, cx))
        })
        .child(div().flex_none().child(label))
        .when(recommended, |this| {
            this.child(Icon::new(IconName::Star).flex_none())
                .tooltip(|window, cx| {
                    Tooltip::new(crate::tr!("choice.recommended")).build(window, cx)
                })
        });
    segment(radio, selected, !disabled, compact, cx)
}
