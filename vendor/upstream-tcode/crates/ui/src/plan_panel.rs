use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::spinner::Spinner;
use agent::{PlanStep, PlanStepStatus};
use gpui::{
    AnyElement, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    ScrollHandle, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use gpui_base::{InteractiveElementExt as _, h_flex, v_flex};

use crate::material;
use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};

pub struct PlanPanel {
    store: Entity<WorkspaceStore>,
    vscroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl PlanPanel {
    pub fn new(store: Entity<WorkspaceStore>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![observe_store_topics(
            &store,
            &[
                TopicKind::ActiveSession,
                TopicKind::SessionStatus,
                TopicKind::SessionPlan,
                TopicKind::SessionEvents,
            ],
            cx,
        )];
        Self {
            store,
            vscroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    fn render_steps(
        &self,
        steps: &[PlanStep],
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut steps_col = v_flex().w_full().min_w_0().gap_1();
        for (index, step) in steps.iter().enumerate() {
            steps_col = steps_col.child(self.render_step(index, step, compact, cx));
        }
        let steps_col = material::rail_detail(steps_col, cx);
        v_flex()
            .w_full()
            .gap_1()
            .child(
                div()
                    .pt_1()
                    .text_caption(cx)
                    .child(crate::tr!("tasks.steps")),
            )
            .child(steps_col)
            .into_any_element()
    }

    fn render_step(
        &self,
        index: usize,
        step: &PlanStep,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let primary = cx.theme().primary;

        let marker: AnyElement = match step.status {
            PlanStepStatus::Completed => Icon::new(IconName::CircleCheck)
                .text_color(muted)
                .into_any_element(),
            PlanStepStatus::InProgress => Spinner::new().color(primary).into_any_element(),
            PlanStepStatus::Pending => Icon::new(IconName::CircleDot)
                .text_color(muted)
                .into_any_element(),
        };

        let mut text = div().flex_1().min_w_0().child(step.step.clone());
        if step.status == PlanStepStatus::Completed {
            text = text.line_through().text_color(muted);
        }

        h_flex()
            .id(("plan-step", index))
            .debug_selector(move || format!("plan-step-{index}"))
            .w_full()
            .py_1()
            .gap_2()
            .items_start()
            .text_body(compact, cx)
            .child(
                // One body line tall, so the marker centres on the first line.
                h_flex()
                    .flex_none()
                    .h(if compact {
                        cx.theme().tokens.typography.md.line_height
                    } else {
                        cx.theme().tokens.typography.sm.line_height
                    })
                    .child(marker),
            )
            .child(text)
            .into_any_element()
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        material::empty_state(
            Icon::new(IconName::ListOrdered),
            crate::tr!("tasks.empty_title"),
            crate::tr!("tasks.empty_desc"),
            cx,
        )
        .into_any_element()
    }
}

impl Render for PlanPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let steps = self
            .store
            .read(cx)
            .session_plan()
            .map(|plan| plan.steps.clone())
            .unwrap_or_default();
        if steps.is_empty() {
            return v_flex().size_full().child(self.render_empty(cx));
        }

        // A compact page is inset from the window edges; the right column keeps
        // its denser padding.
        let compact = crate::window_seam::window_is_compact(window, cx);
        let inset = if compact {
            material::COMPACT_PAGE_INSET
        } else {
            material::CARD_INSET
        };
        let column = v_flex()
            .w_full()
            .min_w_0()
            .px(px(inset))
            .py_3()
            .gap_3()
            .child(self.render_steps(&steps, compact, cx));

        v_flex().size_full().child(crate::scroll::page_viewport(
            "plan-scroll-bounce",
            &self.vscroll,
            div()
                .id("plan-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .lock_scroll_axis()
                .track_scroll(&self.vscroll)
                .child(column),
        ))
    }
}
