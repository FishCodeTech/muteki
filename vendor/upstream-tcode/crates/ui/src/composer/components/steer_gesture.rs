//! Holding Queue opens the touch chooser; sliding up reveals it early.
//! Only the finger's height decides the release: above the gap under the
//! Steer target steers, level with Queue queues, and below Queue cancels,
//! wherever the finger is horizontally. A tap queues without showing the
//! chooser.

use super::super::*;
use gpui::{Bounds, Pixels, Point, TouchDragEvent, TouchPhase, canvas, deferred};
use gpui_base::Positioner;

const CHOICE_GAP: Pixels = px(8.);

pub(in super::super) struct SteerGesture {
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    revealed: bool,
    _hold: Task<()>,
}

impl SteerGesture {
    pub(in super::super) fn armed(&self) -> bool {
        self.position.y < self.bounds.top() - CHOICE_GAP
    }

    fn queues(&self) -> bool {
        !self.armed() && self.position.y <= self.bounds.bottom()
    }
}

impl Composer {
    /// The touch handlers painted over the compact Queue button. The drag is
    /// claimed on the first touch, before it could become a tap, a long
    /// press or a scroll of the thread behind it. That claim ends the
    /// window's own long-press recognition for this touch, so the hold is
    /// timed here with gpui's long-press duration.
    pub(in super::super) fn render_steer_gesture(&self, cx: &mut Context<Self>) -> AnyElement {
        let composer = cx.entity();
        let touch = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                window.on_mouse_event(move |event: &TouchDragEvent, phase, window, cx| {
                    if !phase.bubble() {
                        return;
                    }
                    match event.phase {
                        TouchPhase::Started => {
                            if window.default_prevented() || !bounds.contains(&event.position) {
                                return;
                            }
                            window.prevent_default();
                            cx.stop_propagation();
                            composer.update(cx, |this, cx| {
                                this.begin_steer_gesture(bounds, event.position, cx);
                            });
                        }
                        TouchPhase::Moved => composer.update(cx, |this, cx| {
                            this.move_steer_gesture(event.position, cx);
                        }),
                        TouchPhase::Ended | TouchPhase::Cancelled => {
                            composer.update(cx, |this, cx| {
                                this.end_steer_gesture(event.phase, event.position, window, cx);
                            });
                        }
                    }
                });
            },
        )
        .absolute()
        .inset_0()
        .into_any_element();
        div()
            .absolute()
            .inset_0()
            .child(touch)
            .when_some(
                self.steer_gesture
                    .as_ref()
                    .filter(|gesture| gesture.revealed),
                |el, gesture| el.child(self.render_steer_choices(gesture, cx)),
            )
            .into_any_element()
    }

    fn render_steer_choices(&self, gesture: &SteerGesture, cx: &Context<Self>) -> AnyElement {
        let choices = v_flex()
            .gap(CHOICE_GAP)
            .children([true, false].map(|steer| {
                let id = if steer {
                    "steer-gesture-steer"
                } else {
                    "steer-gesture-queue"
                };
                let selected = if steer {
                    gesture.armed()
                } else {
                    gesture.queues()
                };
                let label = crate::tr!(if steer {
                    "mobile.steer"
                } else {
                    "mobile.queue"
                });
                let hint = crate::tr!(match (steer, selected) {
                    (true, true) => "mobile.release_to_steer",
                    (false, true) => "mobile.release_to_queue",
                    (true, false) => "mobile.steer",
                    (false, false) => "mobile.queue",
                });
                let content = div()
                    .relative()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .when(selected, |el| {
                                el.debug_selector(move || format!("{id}-selected"))
                            })
                            .size(px(40.))
                            .rounded_full()
                            .border_1()
                            .border_color(if selected {
                                cx.theme().primary
                            } else {
                                cx.theme().border
                            })
                            .bg(if selected {
                                cx.theme().primary
                            } else {
                                cx.theme().popover
                            })
                            .text_color(if selected {
                                cx.theme().primary_foreground
                            } else {
                                cx.theme().foreground
                            })
                            .shadow_lg()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::ArrowUp).size(px(18.))),
                    )
                    .child(
                        crate::material::overlay_contour(div(), cx)
                            .absolute()
                            .right(px(52.))
                            .px(cx.theme().tokens.spacing.sm)
                            .py(cx.theme().tokens.spacing.xs)
                            .rounded(crate::material::radius_chip(cx))
                            .whitespace_nowrap()
                            .text_token(&cx.theme().tokens.typography.xs)
                            .text_color(if selected {
                                cx.theme().primary
                            } else {
                                cx.theme().foreground
                            })
                            .child(hint),
                    )
                    .into_any_element();
                let content = if steer {
                    // Base supplies no default popup motion. Use the composer's
                    // existing 150ms entrance with Base's enter easing; GPUI owns
                    // frame scheduling and reduced motion.
                    div()
                        .size_full()
                        .child(content)
                        .with_animation(
                            "steer-choice-pop-in",
                            Animation::new(Duration::from_millis(150))
                                .with_easing(gpui_base::animation::ease_out_cubic),
                            |el, progress| {
                                el.relative()
                                    .top(px(52.) * (1. - progress))
                                    .opacity(progress)
                            },
                        )
                        .into_any_element()
                } else {
                    content
                };
                crate::material::accessible_clickable(div(), id, Role::Button, label, cx)
                    .debug_selector(move || id.into())
                    .relative()
                    .w(gesture.bounds.size.width)
                    .h(gesture.bounds.size.height)
                    .occlude()
                    .child(content)
                    .into_any_element()
            }));
        deferred(
            Positioner::corner(Anchor::BottomRight, gesture.bounds.bottom_right())
                .margin(px(0.))
                .child(choices),
        )
        .with_priority(gpui_base::POPUP_PRIORITY)
        .into_any_element()
    }

    fn begin_steer_gesture(
        &mut self,
        bounds: Bounds<Pixels>,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let hold = gpui::GestureTuning::default().long_press_duration;
        let hold = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(hold).await;
            _ = this.update(cx, |this, cx| {
                this.update_steer_gesture(cx, |gesture| gesture.revealed = true);
            });
        });
        self.steer_gesture = Some(SteerGesture {
            position,
            bounds,
            revealed: false,
            _hold: hold,
        });
        cx.notify();
    }

    fn move_steer_gesture(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.update_steer_gesture(cx, |gesture| {
            gesture.position = position;
            gesture.revealed |= position.y < gesture.bounds.top();
        });
    }

    /// The haptic plays the moment a steer is armed: the finger can lift.
    fn update_steer_gesture(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut SteerGesture),
    ) {
        let Some(gesture) = self.steer_gesture.as_mut() else {
            return;
        };
        let was_armed = gesture.armed();
        change(gesture);
        let armed = gesture.armed();
        if armed && !was_armed {
            crate::haptics::long_press();
        }
        cx.notify();
    }

    fn end_steer_gesture(
        &mut self,
        phase: TouchPhase,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut gesture) = self.steer_gesture.take() else {
            return;
        };
        cx.notify();
        if phase == TouchPhase::Cancelled {
            return;
        }
        gesture.position = position;
        let steer = gesture.armed();
        if steer || gesture.queues() {
            let input = self.input.clone();
            self.submit(&input, steer, window, cx);
        }
    }
}
