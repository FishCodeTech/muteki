use std::borrow::Cow;
use std::time::Duration;

use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::tooltip::Tooltip;
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AppContext as _, Context, ElementId, Entity,
    Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render, RenderOnce,
    SharedString, StatefulInteractiveElement as _, Styled as _, StyledText, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::h_flex;

use super::super::model::{
    TurnTimeClause, format_elapsed_deciseconds, format_local_time, turn_time_breakdown,
    turn_time_clauses,
};
use crate::time::now_millis;
use tcode_core::session::TurnMeta;

/// A running turn's working indicator, drawn by a view whose clock ticks
/// only while a frame draws it: the view is the element's state, so a turn
/// scrolled out of the timeline's rows drops it, and its clock with it.
#[derive(IntoElement)]
struct WorkingIndicator {
    id: ElementId,
    started_at: Option<u64>,
}

impl RenderOnce for WorkingIndicator {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let started_at = self.started_at;
        window.with_global_id(self.id, |id, window| {
            window.with_element_state(id, |view: Option<Entity<WorkingClock>>, _| {
                let view = view
                    .filter(|view| view.read(cx).started_at == started_at)
                    .unwrap_or_else(|| cx.new(|cx| WorkingClock::new(started_at, cx)));
                (view.clone(), view)
            })
        })
    }
}

struct WorkingClock {
    started_at: Option<u64>,
    _tick: Task<()>,
}

impl WorkingClock {
    fn new(started_at: Option<u64>, cx: &mut Context<Self>) -> Self {
        // The elapsed time is shown to the decisecond.
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self {
            started_at,
            _tick: tick,
        }
    }
}

impl Render for WorkingClock {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        working_indicator("working".into(), self.started_at, cx)
    }
}

pub(crate) fn shimmer_label(id: SharedString, label: Cow<'static, str>, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let foreground = cx.theme().foreground;
    let char_starts = label
        .char_indices()
        .map(|(start, _)| start)
        .collect::<Vec<_>>();
    let char_count = char_starts.len().max(1) as f32;

    h_flex()
        .gap(px(0.))
        .text_body(false, cx)
        .children(char_starts.iter().enumerate().map(|(index, &start)| {
            let end = char_starts.get(index + 1).copied().unwrap_or(label.len());
            let glyph = SharedString::from(label[start..end].to_owned());
            let animation_id = SharedString::from(format!("{id}-glyph-{index}"));
            let position = (index as f32 + 0.5) / char_count;

            div().child(StyledText::new(glyph)).with_animation(
                animation_id,
                Animation::new(Duration::from_millis(1400)).repeat(),
                move |element, delta| {
                    // Enter and leave outside the text so every glyph returns to muted.
                    let center = -0.15 + delta * 1.3;
                    let intensity = (1. - (position - center).abs() / 0.15).clamp(0., 1.);
                    element.text_color(Hsla {
                        h: muted.h + (foreground.h - muted.h) * intensity,
                        s: muted.s + (foreground.s - muted.s) * intensity,
                        l: muted.l + (foreground.l - muted.l) * intensity,
                        a: muted.a + (foreground.a - muted.a) * intensity,
                    })
                },
            )
        }))
        .into_any_element()
}

pub(crate) fn working_indicator(id: SharedString, started_at: Option<u64>, cx: &App) -> AnyElement {
    const DELAYS_MS: [u64; 9] = [90, 180, 270, 0, 90, 180, 90, 180, 270];
    const CYCLE_MS: u64 = 650;

    let foreground = cx.theme().foreground;
    let mut pixels = div().grid().grid_cols(3).gap(px(1.5));
    for (cell, delay_ms) in DELAYS_MS.into_iter().enumerate() {
        let animation_id = SharedString::from(format!("{id}-pixel-{cell}"));
        pixels = pixels.child(
            div()
                .size(px(4.))
                .rounded(px(1.))
                .bg(foreground)
                .with_animation(
                    animation_id,
                    Animation::new(Duration::from_millis(CYCLE_MS)).repeat(),
                    move |element, delta| {
                        let phase = (delta + 1. - delay_ms as f32 / CYCLE_MS as f32) % 1.;
                        let triangle = 1. - (phase * 2. - 1.).abs();
                        let eased = triangle * triangle * (3. - 2. * triangle);
                        element.opacity(0.15 + eased * 0.85)
                    },
                ),
        );
    }

    let elapsed_ms = started_at.map_or(0, |start| now_millis().saturating_sub(start));
    h_flex()
        .gap(px(10.))
        .items_center()
        .text_color(cx.theme().muted_foreground)
        .child(pixels)
        .child(shimmer_label(
            SharedString::from(format!("{id}-label")),
            crate::tr!("chat.working"),
            cx,
        ))
        .child(
            div()
                .text_meta_mono(false, cx)
                // Flex has no text baselines (taffy sees opaque boxes), so the
                // smaller digits render top-aligned beside the label; drop
                // them onto the label's baseline. Measured against DM Sans at
                // 2x; both fonts ship with the app.
                .relative()
                .top(px(2.))
                .child(format_elapsed_deciseconds(elapsed_ms)),
        )
        .into_any_element()
}

/// A running turn's liveness, bare in the flow after every segment: the pixel
/// grid, the shimmer label, the timer, and the served-model warning. It belongs
/// to the turn, not to any disclosure, so no fold can hide it.
pub(crate) fn turn_working_indicator(
    turn: usize,
    started_at: Option<u64>,
    served_model: Option<String>,
    cx: &App,
) -> AnyElement {
    h_flex()
        .gap_2()
        .items_center()
        .text_body(false, cx)
        .text_color(cx.theme().muted_foreground)
        .child(WorkingIndicator {
            id: SharedString::from(format!("working-{turn}")).into(),
            started_at,
        })
        .when_some(served_model, |row, served| {
            let tooltip = crate::tr!("chat.served_model_tooltip").into_owned();
            row.child(
                h_flex()
                    .id(SharedString::from(format!("served-model-{turn}")))
                    .gap_1()
                    .items_center()
                    .text_color(cx.theme().warning)
                    .child(crate::icon::Icon::new(crate::icon::IconName::TriangleAlert))
                    .child(served)
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx)),
            )
        })
        .into_any_element()
}

/// A finished turn's quiet mono timing row.
pub(crate) fn turn_time_footer(
    id: SharedString,
    clauses: Vec<TurnTimeClause>,
    breakdown: Option<String>,
    cx: &App,
) -> AnyElement {
    let warning = cx.theme().warning;
    let last = clauses.len().saturating_sub(1);
    h_flex()
        .id(id)
        .w_full()
        .gap_1p5()
        .items_start()
        .text_meta_mono(false, cx)
        .debug_selector(|| "turn-time-row".into())
        .when_some(breakdown, |row, breakdown| {
            row.tooltip(move |window, cx| Tooltip::new(breakdown.clone()).build(window, cx))
        })
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .flex_wrap()
                .justify_start()
                .gap_x(px(4.))
                .gap_y(px(2.))
                .children(clauses.into_iter().enumerate().map(|(index, clause)| {
                    let selector = clause.selector;
                    div()
                        .min_w_0()
                        .debug_selector(move || selector.into())
                        .when(clause.warning, |element| element.text_color(warning))
                        .child(if index == last {
                            clause.text
                        } else {
                            format!("{} ·", clause.text)
                        })
                })),
        )
        .into_any_element()
}

pub(crate) fn finished_turn_time(
    timestamp: u64,
    turn: &TurnMeta,
    requested_model: Option<&str>,
    cx: &App,
) -> AnyElement {
    turn_time_footer(
        SharedString::from(format!("turn-time-{timestamp}")),
        turn_time_clauses(
            format_local_time(timestamp),
            turn.timing,
            turn.cost_usd,
            turn.served_model.as_deref(),
            requested_model,
        ),
        turn_time_breakdown(turn.timing),
        cx,
    )
}

#[cfg(test)]
mod tests {
    use super::turn_time_footer;
    use crate::chat::model::{TurnTimeClause, turn_time_clauses};
    use gpui::TestAppContext;
    use tcode_core::session::TurnTiming;

    struct TurnTimeFooterProbe {
        clauses: Vec<TurnTimeClause>,
    }

    impl gpui::Render for TurnTimeFooterProbe {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            // The turn column stacks the footer under the answer, so the row is
            // content-height there; reproduce that rather than letting the window
            // stretch it and mask a wrap.
            use gpui::{ParentElement as _, Styled as _};
            gpui_base::v_flex().size_full().child(turn_time_footer(
                "turn-time-probe".into(),
                self.clauses.clone(),
                None,
                cx,
            ))
        }
    }

    #[gpui::test]
    fn the_time_row_keeps_its_compact_clauses_inside_narrow_widths(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        let clauses = turn_time_clauses(
            "10:18 AM".into(),
            Some(TurnTiming::new(99_000, 0)),
            None,
            None,
            None,
        );
        // The footer's clauses, in render order (mirrors `turn_time_footer`).
        let selectors = ["turn-time-clock", "turn-time-total"];
        assert_eq!(clauses.len(), selectors.len());

        cx.update(crate::theme::init);
        let (_, cx) = cx.add_window_view(|_, _| TurnTimeFooterProbe { clauses });
        let cx: &mut VisualTestContext = cx;
        let draw = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| {
                _ = window.draw(cx);
            });
        };

        // A comfortable chat width: the restrained single-line baseline.
        cx.simulate_resize(size(px(900.), px(120.)));
        draw(cx);
        let first = cx.debug_bounds(selectors[0]).expect("clock bounds");
        for selector in &selectors[1..] {
            let clause = cx.debug_bounds(selector).expect("clause bounds");
            assert_eq!(
                clause.top(),
                first.top(),
                "{selector} left the single line at 900px: {clause:?}"
            );
        }

        // Opening the diff panel squeezes the chat column; sweep well below any
        // practical width. Every clause must stay inside the row, and the row
        // must preserve the intentionally left-aligned footer.
        for width in 260..=900 {
            cx.simulate_resize(size(px(width as f32), px(120.)));
            draw(cx);

            let row = cx.debug_bounds("turn-time-row").expect("row bounds");
            for selector in &selectors {
                let clause = cx.debug_bounds(selector).expect("clause bounds");
                assert!(
                    clause.left() >= row.left() && clause.right() <= row.right(),
                    "{selector} escaped the row at {width}px: row={row:?}, clause={clause:?}"
                );
                assert!(
                    clause.size.width > px(0.) && clause.size.height > px(0.),
                    "{selector} was squeezed away at {width}px: {clause:?}"
                );
            }
            let first_clause = cx.debug_bounds(selectors[0]).expect("clause bounds");
            assert!(
                (first_clause.left() - row.left()).abs() <= px(0.5),
                "the footer left the leading edge at {width}px: \
                 row={row:?}, clause={first_clause:?}"
            );
        }
    }
}
