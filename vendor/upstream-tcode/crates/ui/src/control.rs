//! How a `tcode_control` grant reads and is edited: one editor for Settings →
//! Control's default and project grants.

use std::rc::Rc;

use gpui::{
    AnyElement, App, IntoElement, ParentElement as _, SharedString, Styled as _, Window, div,
};
use gpui_base::v_flex;
use tcode_core::control::{ControlCategory, ControlGrant, ControlScope, ControlTier};
use tcode_core::project::Project;

use crate::typography::TextRole as _;
use crate::widgets::checkbox::Checkbox;
use crate::widgets::choice::{Choice, ChoiceGroup};
use crate::widgets::switch::Switch;

pub(crate) type GrantChange = Rc<dyn Fn(ControlGrant, &mut Window, &mut App)>;

pub(crate) fn tier_label(tier: ControlTier) -> String {
    match tier {
        ControlTier::Observe => crate::tr!("control.tier.observe"),
        ControlTier::Organize => crate::tr!("control.tier.organize"),
        ControlTier::Operate => crate::tr!("control.tier.operate"),
    }
    .into_owned()
}

fn tier_description(tier: ControlTier) -> String {
    match tier {
        ControlTier::Observe => crate::tr!("control.tier.observe_description"),
        ControlTier::Organize => crate::tr!("control.tier.organize_description"),
        ControlTier::Operate => crate::tr!("control.tier.operate_description"),
    }
    .into_owned()
}

pub(crate) fn category_label(category: ControlCategory) -> String {
    match category {
        ControlCategory::AnswerInput => crate::tr!("control.category.answer_input"),
        ControlCategory::Terminal => crate::tr!("control.category.terminal"),
        ControlCategory::Delete => crate::tr!("control.category.delete"),
        ControlCategory::GitRemote => crate::tr!("control.category.git_remote"),
        ControlCategory::PrWrite => crate::tr!("control.category.pr_write"),
        ControlCategory::HistoryEdit => crate::tr!("control.category.history_edit"),
        ControlCategory::SettingsWrite => crate::tr!("control.category.settings_write"),
    }
    .into_owned()
}

fn category_description(category: ControlCategory) -> String {
    match category {
        ControlCategory::AnswerInput => crate::tr!("control.category.answer_input_description"),
        ControlCategory::Terminal => crate::tr!("control.category.terminal_description"),
        ControlCategory::Delete => crate::tr!("control.category.delete_description"),
        ControlCategory::GitRemote => crate::tr!("control.category.git_remote_description"),
        ControlCategory::PrWrite => crate::tr!("control.category.pr_write_description"),
        ControlCategory::HistoryEdit => crate::tr!("control.category.history_edit_description"),
        ControlCategory::SettingsWrite => {
            crate::tr!("control.category.settings_write_description")
        }
    }
    .into_owned()
}

/// "project", "2 projects" or "host".
pub(crate) fn scope_label(scope: &ControlScope) -> String {
    match scope {
        ControlScope::Project => crate::tr!("control.scope.project_short"),
        ControlScope::Projects(ids) => {
            crate::tr!("control.scope.projects_short", count = ids.len())
        }
        ControlScope::Host => crate::tr!("control.scope.host_short"),
    }
    .into_owned()
}

/// "project · observe+organize+operate", as the lists and grant requests name a grant.
pub(crate) fn grant_summary(grant: &ControlGrant) -> String {
    let parts: Vec<String> = grant
        .tiers
        .iter()
        .map(|tier| tier_label(*tier))
        .chain(
            grant
                .categories
                .iter()
                .map(|category| category_label(*category)),
        )
        .collect();
    let allowed = if parts.is_empty() {
        crate::tr!("control.nothing").into_owned()
    } else {
        parts.join("+")
    };
    format!("{} · {allowed}", scope_label(&grant.scope))
}

/// A `ControlAction`'s operation in words; an operation this client does not
/// know keeps the key the host recorded.
pub(crate) fn action_label(key: &str) -> String {
    use tcode_core::control::action;
    let key = match key {
        action::SETTLE => "control.action.settle",
        action::SETTLED_ITSELF => "control.action.settled_itself",
        action::UNSETTLE => "control.action.unsettle",
        action::ARCHIVE => "control.action.archive",
        action::UNARCHIVE => "control.action.unarchive",
        action::PIN => "control.action.pin",
        action::UNPIN => "control.action.unpin",
        action::RENAME => "control.action.rename",
        action::MARK_READ => "control.action.mark_read",
        action::MARK_UNREAD => "control.action.mark_unread",
        action::FOLLOW_UP => "control.action.follow_up",
        action::FOLLOW_UP_OFF => "control.action.follow_up_off",
        action::PARK => "control.action.park",
        action::PARK_OFF => "control.action.park_off",
        action::KEEP => "control.action.keep",
        action::KEEP_OFF => "control.action.keep_off",
        action::SETTLE_SELF => "control.action.settle_self",
        action::RECORD_REVIEW => "control.action.record_review",
        action::REQUEST_GRANT => "control.action.request_grant",
        action::NOTIFY_WHEN => "control.action.notify_when",
        action::CANCEL_NOTIFY => "control.action.cancel_notify",
        action::RESPOND_INPUT => "control.action.respond_input",
        action::TERMINAL_OPEN => "control.action.terminal_open",
        action::TERMINAL_INPUT => "control.action.terminal_input",
        action::TERMINAL_CLOSE => "control.action.terminal_close",
        action::DELETE_THREADS => "control.action.delete_threads",
        action::REWIND => "control.action.rewind",
        action::PATCH_SETTINGS => "control.action.patch_settings",
        action::START_THREAD => "control.action.start_thread",
        action::SEND_QUEUED => "control.action.send_queued",
        action::SEND_STEERED => "control.action.send_steered",
        action::SEND_SCHEDULED => "control.action.send_scheduled",
        action::INTERRUPT => "control.action.interrupt",
        action::RETRY => "control.action.retry",
        action::COMPACT => "control.action.compact",
        action::SET_MODEL => "control.action.set_model",
        action::SET_OPTION => "control.action.set_option",
        action::FORK => "control.action.fork",
        action::STEER_QUEUED => "control.action.steer_queued",
        action::DROP_QUEUED => "control.action.drop_queued",
        action::PR_LINK => "control.action.pr_link",
        action::PR_UNLINK => "control.action.pr_unlink",
        action::PR_WATCH => "control.action.pr_watch",
        action::PR_UNWATCH => "control.action.pr_unwatch",
        action::PR_REFRESH => "control.action.pr_refresh",
        action::PR_COMMENT => "control.action.pr_comment",
        action::PR_REPLY => "control.action.pr_reply",
        action::PR_RESOLVE => "control.action.pr_resolve",
        action::GIT_CHECKOUT => "control.action.git_checkout",
        action::GIT_MERGE_WORKTREE => "control.action.git_merge_worktree",
        action::GIT_COMMIT => "control.action.git_commit",
        action::GIT_COMMIT_PUSH => "control.action.git_commit_push",
        action::GIT_PUSH => "control.action.git_push",
        action::GIT_PULL => "control.action.git_pull",
        action::GIT_PUBLISH_BRANCH => "control.action.git_publish_branch",
        action::GIT_INITIALIZE => "control.action.git_initialize",
        action::CREATE_PROJECT => "control.action.create_project",
        action::MOVE => "control.action.move",
        action::REFERENCE => "control.action.reference",
        _ => return key.to_owned(),
    };
    crate::tr!(key).into_owned()
}

/// Scope, tiers and categories of one grant. Every edit reports the whole new
/// grant through `on_change`; the caller decides where it is stored.
pub(crate) fn grant_editor(
    id: impl Into<SharedString>,
    grant: &ControlGrant,
    projects: &[Project],
    compact: bool,
    on_change: GrantChange,
    cx: &App,
) -> gpui::Div {
    let id: SharedString = id.into();
    let caption = |text: String| {
        div()
            .pt_1()
            .pl(gpui::px(crate::material::CARD_INSET))
            .text_caption(cx)
            .child(text)
    };
    let selected_projects = match &grant.scope {
        ControlScope::Projects(ids) => ids.clone(),
        _ => Vec::new(),
    };
    let scopes = [
        (ControlScope::Project, crate::tr!("control.scope.project")),
        (
            ControlScope::Projects(selected_projects.clone()),
            crate::tr!("control.scope.projects"),
        ),
        (ControlScope::Host, crate::tr!("control.scope.host")),
    ];
    let scope_choices = scopes
        .into_iter()
        .enumerate()
        .map(|(index, (scope, label))| {
            let selected = std::mem::discriminant(&scope) == std::mem::discriminant(&grant.scope);
            let grant = grant.clone();
            let on_change = on_change.clone();
            Choice::new(SharedString::from(format!("{id}-scope-{index}")), label)
                .selected(selected)
                .on_select(move |window, cx| {
                    let mut next = grant.clone();
                    next.scope = scope.clone();
                    on_change(next, window, cx);
                })
        });
    let mut editor = v_flex()
        .w_full()
        .gap_2()
        .child(caption(crate::tr!("control.scope.title").into_owned()))
        .child(
            ChoiceGroup::new(SharedString::from(format!("{id}-scope")))
                .compact(compact)
                .aria_label(crate::tr!("control.scope.title"))
                .choices(scope_choices),
        );
    if matches!(grant.scope, ControlScope::Projects(_)) {
        let mut list = v_flex().gap_1();
        if projects.is_empty() {
            list = list.child(
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("control.scope.no_projects")),
            );
        }
        for project in projects {
            let checked = selected_projects.contains(&project.id);
            let grant = grant.clone();
            let on_change = on_change.clone();
            let project_id = project.id.clone();
            list = list.child(
                Checkbox::new(SharedString::from(format!("{id}-project-{}", project.id)))
                    .label(project.name.clone())
                    .checked(checked)
                    .on_click(move |checked, window, cx| {
                        let mut next = grant.clone();
                        if let ControlScope::Projects(ids) = &mut next.scope {
                            ids.retain(|id| id != &project_id);
                            if *checked {
                                ids.push(project_id.clone());
                            }
                        }
                        on_change(next, window, cx);
                    }),
            );
        }
        editor = editor.child(list);
    }
    let tier_rows = ControlTier::ALL
        .into_iter()
        .map(|tier| {
            let grant = grant.clone();
            let on_change = on_change.clone();
            switch_row(
                SharedString::from(format!("{id}-tier-{}", tier.name())),
                tier_label(tier),
                tier_description(tier),
                grant.allows_tier(tier),
                compact,
                Rc::new(move |on, window, cx| {
                    let mut next = grant.clone();
                    if on {
                        next.tiers.insert(tier);
                    } else {
                        next.tiers.remove(&tier);
                    }
                    on_change(next, window, cx);
                }),
                cx,
            )
        })
        .collect();
    let category_rows = ControlCategory::ALL
        .into_iter()
        .map(|category| {
            let grant = grant.clone();
            let on_change = on_change.clone();
            switch_row(
                SharedString::from(format!("{id}-category-{}", category.name())),
                category_label(category),
                category_description(category),
                grant.allows_category(category),
                compact,
                Rc::new(move |on, window, cx| {
                    let mut next = grant.clone();
                    if on {
                        next.categories.insert(category);
                    } else {
                        next.categories.remove(&category);
                    }
                    on_change(next, window, cx);
                }),
                cx,
            )
        })
        .collect();
    editor
        .child(caption(crate::tr!("control.tiers").into_owned()))
        .child(crate::material::grouped(tier_rows, cx))
        .child(caption(crate::tr!("control.categories").into_owned()))
        .child(crate::material::grouped(category_rows, cx))
}

type Toggle = Rc<dyn Fn(bool, &mut Window, &mut App)>;

fn switch_row(
    id: SharedString,
    title: String,
    description: String,
    checked: bool,
    compact: bool,
    on_toggle: Toggle,
    cx: &App,
) -> AnyElement {
    crate::material::group_row(cx)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_body(compact, cx).child(title.clone()))
                .child(div().text_secondary(compact, cx).child(description)),
        )
        .child(
            Switch::new(id)
                .checked(checked)
                .aria_label(title)
                .on_click(move |next, window, cx| on_toggle(*next, window, cx)),
        )
        .into_any_element()
}
