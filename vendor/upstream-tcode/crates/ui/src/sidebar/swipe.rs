//! Swiping a phone thread row left settles it, or makes a settled one active
//! again: the long-press menu's Settle / Unsettle, one gesture away.
//!
//! gpui-base has no swipe-action row, so the row claims GPUI's touch pan
//! itself. GPUI locks a pan to one axis and reports it as `ScrollWheelEvent`s
//! at the pan's original contact; the row takes a horizontal one in the
//! capture phase, and a vertical one goes on to the list.

use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use super::arrange::DragAction;
use super::*;
use crate::edge_back::BackProgress;
use gpui::{DispatchPhase, HitboxBehavior, Pixels, Point, ScrollDelta, TouchPhase, canvas};

pub(super) struct RowSwipe {
    session_id: String,
    origin: Point<Pixels>,
    /// Travel and velocity count leftward.
    progress: BackProgress,
    last_moved_at: Instant,
    released: bool,
    /// Whether the thread was settled when the swipe committed. The row stays
    /// open until the list shows the thread moved, or the host refuses.
    committed: Option<bool>,
}

impl RowSwipe {
    pub(super) fn refused(&mut self) {
        self.committed = None;
    }
}

impl SessionsSidebar {
    pub(super) fn swipeable_row(
        &mut self,
        row: &CompactThreadRow,
        content: gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let id = row.meta.id.clone();
        let settled = row.meta.is_settled();
        let renaming = self
            .renaming
            .as_ref()
            .is_some_and(|rename| rename.session_id == id);
        // The long-press menu's rule for enabling Settle / Unsettle.
        let can_swipe = !renaming && (settled || (!row.working && !row.state.waiting()));
        let (target, dragging) = match self.row_swipe.as_mut().filter(|s| s.session_id == id) {
            Some(swipe) if !swipe.released => (swipe.progress.progress(), true),
            Some(swipe) => match swipe.committed {
                Some(was_settled) if was_settled == settled => (1., false),
                // It moved: show it closed in its new place without sliding.
                Some(_) => {
                    swipe.committed = None;
                    (0., true)
                }
                None => (0., false),
            },
            None => (0., false),
        };
        let progress = crate::edge_back::settle(
            ("compact-row-swipe", SharedString::from(id.clone())),
            target,
            dragging,
            window,
            cx,
        );
        let action = if settled {
            DragAction::Unsettle
        } else {
            DragAction::Settle
        };
        let (fill, on_fill) = if settled {
            (cx.theme().primary.opacity(0.12), cx.theme().primary)
        } else {
            (
                cx.theme().success.opacity(0.12),
                cx.theme().success_foreground,
            )
        };
        let sidebar = cx.entity().downgrade();
        div()
            .relative()
            .w_full()
            .overflow_hidden()
            .child(
                canvas(
                    |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    move |bounds, hitbox, window, _| {
                        window.on_mouse_event(
                            move |event: &gpui::ScrollWheelEvent, phase, window, cx| {
                                if phase != DispatchPhase::Capture {
                                    return;
                                }
                                let ScrollDelta::Pixels(delta) = event.delta else {
                                    return;
                                };
                                let Some(sidebar) = sidebar.upgrade() else {
                                    return;
                                };
                                sidebar.update(cx, |this, cx| {
                                    if event.touch_phase == TouchPhase::Started {
                                        // A finished swipe must not claim a later
                                        // pan that starts at the same point.
                                        if this.row_swipe.as_ref().is_some_and(|swipe| {
                                            swipe.released && swipe.committed.is_none()
                                        }) {
                                            this.row_swipe = None;
                                        }
                                        if !can_swipe
                                            || delta.x >= px(0.)
                                            || delta.y != px(0.)
                                            || !bounds.contains(&event.position)
                                            || !hitbox.should_handle_scroll(window)
                                        {
                                            return;
                                        }
                                        this.row_swipe = Some(RowSwipe {
                                            session_id: id.clone(),
                                            origin: event.position,
                                            progress: BackProgress {
                                                width: bounds.size.width,
                                                ..Default::default()
                                            },
                                            last_moved_at: cx.background_executor().now(),
                                            released: false,
                                            committed: None,
                                        });
                                    }
                                    let Some(swipe) = this.row_swipe.as_mut().filter(|swipe| {
                                        swipe.session_id == id && swipe.origin == event.position
                                    }) else {
                                        return;
                                    };
                                    cx.stop_propagation();
                                    if swipe.released {
                                        return;
                                    }
                                    let now = cx.background_executor().now();
                                    let was_past = swipe.progress.past_threshold();
                                    swipe.progress.travel -= delta.x;
                                    if swipe.progress.past_threshold() != was_past {
                                        crate::haptics::threshold();
                                    }
                                    if event.touch_phase == TouchPhase::Moved && delta.x != px(0.) {
                                        let elapsed =
                                            now.duration_since(swipe.last_moved_at).as_secs_f32();
                                        swipe.progress.velocity =
                                            -f32::from(delta.x) / elapsed.max(0.001);
                                        swipe.last_moved_at = now;
                                    }
                                    if matches!(
                                        event.touch_phase,
                                        TouchPhase::Ended | TouchPhase::Cancelled
                                    ) {
                                        if now.duration_since(swipe.last_moved_at)
                                            > Duration::from_millis(100)
                                        {
                                            swipe.progress.velocity = 0.;
                                        }
                                        swipe.released = true;
                                        if event.touch_phase == TouchPhase::Ended {
                                            crate::edge_back::catch_fling(swipe.origin, window, cx);
                                            if swipe.progress.commits() {
                                                swipe.committed = Some(settled);
                                                if settled {
                                                    this.perform_lifecycle(
                                                        vec![Command::UnsettleSession {
                                                            session_id: id.clone(),
                                                        }],
                                                        None,
                                                        window,
                                                        cx,
                                                    );
                                                } else {
                                                    this.settle_thread(&id, window, cx);
                                                }
                                            }
                                        }
                                    }
                                    cx.notify();
                                });
                            },
                        );
                    },
                )
                .absolute()
                .inset_0(),
            )
            .when(progress > 0., |row| {
                row.child(
                    h_flex()
                        .absolute()
                        .inset_0()
                        .justify_end()
                        .items_center()
                        .gap(cx.theme().tokens.spacing.xs)
                        .px(px(COMPACT_PAGE_INSET))
                        .bg(fill)
                        .text_color(on_fill)
                        .text_body(true, cx)
                        .child(Icon::new(action.icon()))
                        .child(action.label()),
                )
            })
            .child(
                div()
                    .relative()
                    .left(gpui::relative(-progress))
                    .bg(crate::material::content_surface(cx))
                    .child(content),
            )
            .into_any_element()
    }
}
