use crate::theme::ActiveTheme as _;
use gpui::{
    App, BoxShadow, ElementId, Entity, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, RenderOnce, SharedString, StatefulInteractiveElement as _, StyleRefinement,
    Styled, Window, div, hsla, point, prelude::FluentBuilder as _, px, relative,
};
use gpui_base::motion::{Transition, transition};
use gpui_base::slider::SliderState;
use gpui_base::{SliderIndicator, SliderThumb, SliderTrack, StyledExt as _};
use std::time::Duration;

const TRACK_HEIGHT: f32 = 24.;
const THUMB_SIZE: f32 = 28.;
const THUMB_PRESSED_SIZE: f32 = 32.;
const TICK_SIZE: f32 = 4.;
/// Thumb centers stop this far inside the track's ends, so the thumb at either
/// end sits flush with the rounded cap.
const END_INSET: f32 = TRACK_HEIGHT / 2. + 1.;

/// A stepped horizontal slider in Codex's model-picker style: a 24px pill track
/// filled up to the thumb, a dot at every step, and a 28px white thumb that
/// glides between steps. The fill and thumb follow the stepped value rather
/// than the pointer.
#[derive(IntoElement)]
pub struct Slider {
    state: Entity<SliderState>,
    style: StyleRefinement,
    fill: Option<Hsla>,
    disabled: bool,
    accessibility_label: Option<SharedString>,
}

impl Slider {
    pub fn new(state: &Entity<SliderState>) -> Self {
        Self {
            state: state.clone(),
            style: StyleRefinement::default(),
            fill: None,
            disabled: false,
            accessibility_label: None,
        }
    }
    pub fn accessibility_label(mut self, label: impl Into<SharedString>) -> Self {
        self.accessibility_label = Some(label.into());
        self
    }
    /// The filled portion's color; the theme's primary by default.
    pub fn fill(mut self, color: impl Into<Hsla>) -> Self {
        self.fill = Some(color.into());
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl Styled for Slider {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Slider {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = self.state.read(cx);
        let (min, max, step) = (state.min_value(), state.max_value(), state.step_value());
        let span = max - min;
        let fraction = |value: f32| {
            if span > 0. {
                ((value - min) / span).clamp(0., 1.)
            } else {
                0.
            }
        };
        let target = fraction(state.value().end());
        let steps = if step > 0. {
            (span / step).round() as usize
        } else {
            0
        };
        let entity_id = self.state.entity_id();
        let glide = Transition::new(Duration::from_millis(300)).ease(ease_out_quint);
        let motion_id = |name: &'static str| ElementId::from((name, entity_id));
        let filled = transition(motion_id("slider-fill"), target, glide, window, cx);
        // Raised while hovered, and kept through a drag that leaves the thumb.
        let raised = window.use_keyed_state(("slider-thumb-raised", entity_id), cx, |_, _| false);
        let thumb_size = transition(
            motion_id("slider-thumb-size"),
            if *raised.read(cx) && !self.disabled {
                THUMB_PRESSED_SIZE
            } else {
                THUMB_SIZE
            },
            Transition::new(Duration::from_millis(150)).ease(ease_out_quint),
            window,
            cx,
        );

        let theme = cx.theme();
        let fill = self.fill.unwrap_or(theme.primary);
        let foreground = theme.foreground;
        let ticks = (0..=steps).map(|index| {
            let at = fraction(min + index as f32 * step);
            div()
                .absolute()
                .top(px((TRACK_HEIGHT - TICK_SIZE) / 2.))
                .left(relative(at))
                .ml(px(-TICK_SIZE / 2.))
                .size(px(TICK_SIZE))
                .rounded_full()
                .bg(if at <= target {
                    hsla(0., 0., 1., 0.3)
                } else {
                    foreground.opacity(0.35)
                })
        });
        let rail = || {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(END_INSET))
                .right(px(END_INSET))
        };

        gpui_base::Slider::new(&self.state)
            .when_some(self.accessibility_label, |this, label| {
                this.accessibility_label(label)
            })
            .disabled(self.disabled)
            .relative()
            .w_full()
            .h(px(THUMB_SIZE))
            .refine_style(&self.style)
            .when(self.disabled, |this| this.opacity(0.6))
            .child(
                SliderTrack::new(&self.state)
                    .disabled(self.disabled)
                    .relative()
                    .size_full()
                    .flex()
                    .items_center()
                    .when(!self.disabled, |this| this.cursor_pointer())
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(TRACK_HEIGHT))
                            .rounded_full()
                            .overflow_hidden()
                            .bg(foreground.opacity(0.1))
                            .child(
                                rail().child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .bottom_0()
                                        .left(px(-END_INSET))
                                        .right(relative(1. - filled))
                                        // The track's overflow clip is
                                        // rectangular, so the fill rounds its
                                        // own end cap.
                                        .rounded_l_full()
                                        .bg(fill),
                                ),
                            )
                            .child(rail().children(ticks)),
                    )
                    .child(
                        SliderIndicator::new(&self.state)
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(END_INSET))
                            .right(px(END_INSET))
                            .child(
                                SliderThumb::new(&self.state)
                                    .disabled(self.disabled)
                                    .absolute()
                                    .top(px((THUMB_SIZE - thumb_size) / 2.))
                                    .left(relative(filled))
                                    .ml(px(-thumb_size / 2.))
                                    .size(px(thumb_size))
                                    .rounded_full()
                                    .border(px(0.5))
                                    .border_color(foreground.opacity(0.15))
                                    .bg(gpui::white())
                                    .shadow(vec![BoxShadow {
                                        color: hsla(0., 0., 0., 0.1),
                                        offset: point(px(0.), px(0.)),
                                        blur_radius: px(2.),
                                        spread_radius: px(0.),
                                        inset: false,
                                    }])
                                    .when(!self.disabled, |this| {
                                        let (hover, release) = (raised.clone(), raised.clone());
                                        this.on_hover(move |hovered, _, cx| {
                                            hover.update(cx, |raised, cx| {
                                                *raised = *hovered;
                                                cx.notify();
                                            })
                                        })
                                        .on_mouse_up_out(
                                            gpui::MouseButton::Left,
                                            move |_, _, cx| {
                                                release.update(cx, |raised, cx| {
                                                    *raised = false;
                                                    cx.notify();
                                                })
                                            },
                                        )
                                    }),
                            ),
                    ),
            )
    }
}

fn ease_out_quint(t: f32) -> f32 {
    1. - (1. - t).powi(5)
}
