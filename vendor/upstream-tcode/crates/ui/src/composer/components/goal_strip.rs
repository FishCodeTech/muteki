use super::super::*;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};
use agent::{Goal, GoalStatus};

fn status_label(status: GoalStatus) -> String {
    match status {
        GoalStatus::Active => crate::tr!("composer.goal_active"),
        GoalStatus::Paused => crate::tr!("composer.goal_paused"),
        GoalStatus::Blocked => crate::tr!("composer.goal_blocked"),
        GoalStatus::UsageLimited => crate::tr!("composer.goal_usage_limited"),
        GoalStatus::BudgetLimited => crate::tr!("composer.goal_budget_limited"),
        GoalStatus::Complete => crate::tr!("composer.goal_complete"),
    }
    .into_owned()
}

/// Time, tokens and checks, whichever the harness reports.
fn usage_line(goal: &Goal) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(secs) = goal.time_used_secs.filter(|secs| *secs > 0) {
        parts.push(crate::chat::model::format_duration(secs));
    }
    match (goal.tokens_used, goal.token_budget) {
        (used, Some(budget)) => parts.push(
            crate::tr!(
                "composer.goal_tokens_of_budget",
                used = crate::context_meter::format_tokens(Some(used.unwrap_or(0))),
                budget = crate::context_meter::format_tokens(Some(budget))
            )
            .into_owned(),
        ),
        (Some(used), None) if used > 0 => parts.push(
            crate::tr!(
                "composer.goal_tokens",
                used = crate::context_meter::format_tokens(Some(used))
            )
            .into_owned(),
        ),
        _ => {}
    }
    if let Some(checks) = goal.checks.filter(|checks| *checks > 0) {
        parts.push(crate::tr!("composer.goal_checks", count = checks).into_owned());
    }
    parts.join(" · ")
}

impl Composer {
    /// The harness's standing goal: what it works toward, how far it got, and
    /// the pause, edit and clear controls the harness offers.
    pub(in super::super) fn render_goal_strip(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.workspace_store.read(cx).composer_state();
        let goal = state.goal.clone()?;
        let control = state.provider.and_then(|provider| provider.caps().goals);
        let enabled = self.interactive(cx) && goal_offered(&state).is_some();
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (icon, color) = match goal.status {
            GoalStatus::Active => (IconName::CircleDot, theme.primary),
            GoalStatus::Paused => (IconName::Pause, muted),
            GoalStatus::Complete => (IconName::CircleCheck, theme.success),
            GoalStatus::Blocked | GoalStatus::UsageLimited | GoalStatus::BudgetLimited => {
                (IconName::TriangleAlert, theme.warning)
            }
        };
        let pause = match goal.status {
            GoalStatus::Active => {
                Some((IconName::Pause, GoalStatus::Paused, "composer.goal_pause"))
            }
            GoalStatus::Paused | GoalStatus::Blocked | GoalStatus::UsageLimited => {
                Some((IconName::Play, GoalStatus::Active, "composer.goal_resume"))
            }
            GoalStatus::BudgetLimited | GoalStatus::Complete => None,
        }
        .filter(|_| control.is_some_and(|control| control.pause));
        let usage = usage_line(&goal);
        let last_check = goal
            .last_check
            .clone()
            .filter(|_| goal.status != GoalStatus::Complete);
        let objective = goal.objective.clone();
        let touch = self.compact;

        let header = h_flex()
            .w_full()
            .gap_1()
            .items_center()
            .text_secondary(touch, cx)
            .child(Icon::new(icon).text_color(color))
            .child(div().flex_none().child(crate::tr!(
                "composer.goal_header",
                status = status_label(goal.status)
            )))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(usage)),
            )
            .when_some(pause, |row, (icon, status, label)| {
                row.child(
                    Button::new("goal-pause")
                        .debug_selector(|| "goal-pause".into())
                        .ghost()
                        .xsmall()
                        .touch(touch)
                        .icon(icon)
                        .disabled(!enabled)
                        .tooltip(crate::tr!(label))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store
                                .update(cx, |store, _cx| store.set_goal(None, Some(status)));
                        })),
                )
            })
            .child(
                Button::new("goal-edit")
                    .debug_selector(|| "goal-edit".into())
                    .ghost()
                    .xsmall()
                    .touch(touch)
                    .icon(IconName::Pencil)
                    .disabled(!enabled)
                    .tooltip(crate::tr!("composer.goal_edit"))
                    .on_click(cx.listener({
                        let objective = objective.clone();
                        move |this, _, window, cx| this.edit_goal(&objective, window, cx)
                    })),
            )
            .child(
                Button::new("goal-clear")
                    .debug_selector(|| "goal-clear".into())
                    .ghost()
                    .xsmall()
                    .touch(touch)
                    .icon(IconName::Close)
                    .disabled(!enabled)
                    .tooltip(crate::tr!("composer.goal_clear"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_store
                            .update(cx, |store, _cx| store.clear_goal());
                    })),
            );

        Some(
            v_flex()
                .id("goal-strip")
                .w_full()
                .gap_0p5()
                .px_1()
                .child(header)
                .child(
                    div()
                        .w_full()
                        .line_clamp(3)
                        .text_body(touch, cx)
                        .text_color(theme.foreground)
                        .child(objective.clone()),
                )
                .when_some(last_check, |strip, reason| {
                    strip.child(
                        div()
                            .w_full()
                            .line_clamp(2)
                            .text_secondary(touch, cx)
                            .child(crate::tr!("composer.goal_last_check", reason = reason)),
                    )
                })
                .context_menu(move |menu, _, _| {
                    menu.menu(
                        crate::tr!("chat.copy_text").into_owned(),
                        Box::new(CopyText(objective.clone())),
                    )
                })
                .into_any_element(),
        )
    }

    /// Put `/goal <objective>` in the composer with the objective selected.
    pub(in super::super) fn edit_goal(
        &mut self,
        objective: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = format!("/goal {objective}");
        let selection = "/goal ".len()..text.len();
        self.input.update(cx, |state, cx| {
            state.replace_all(text, window, cx);
            state.set_selected_range(selection, cx);
            if !crate::window_seam::is_mobile(cx) {
                state.focus(window, cx);
            }
        });
        self.recompute_trigger(cx);
        cx.notify();
    }
}
