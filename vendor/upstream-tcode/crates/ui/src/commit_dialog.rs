//! Commit dialog with a changed-files list, include/exclude checkboxes, the
//! current branch, a default-branch safeguard banner, and a commit-message
//! textarea pre-filled by AI generation (with a regenerate button).

use std::collections::HashSet;

use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::checkbox::Checkbox;
use crate::widgets::input::{Textarea, TextareaState};
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, Task, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use tcode_core::git::{GitAction, GitFileEntry, feature_branch_name, included_paths};

use crate::store::WorkspaceStore;

pub struct CommitDialog {
    store: Entity<WorkspaceStore>,
    message: Entity<TextareaState>,
    files: Vec<GitFileEntry>,
    /// User-*excluded* (unchecked) paths — kept out of the commit via pathspec.
    excluded: HashSet<String>,
    branch: Option<String>,
    on_default_branch: bool,
    /// Safeguard: create a `tcode/<slug>` feature branch and commit there.
    create_feature_branch: bool,
    action: GitAction,
    generating: bool,
    _gen_task: Option<Task<()>>,
}

impl CommitDialog {
    pub fn new(
        store: Entity<WorkspaceStore>,
        action: GitAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = store.read(cx).commit_dialog_state();
        let files = state.files;
        let branch = state.branch;
        let on_default_branch = state.on_default_branch;
        let message = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 10)
                .placeholder(crate::tr!("git.commit.message_placeholder"))
        });
        let mut this = Self {
            store,
            message,
            files,
            excluded: HashSet::new(),
            branch,
            on_default_branch,
            create_feature_branch: false,
            action,
            generating: false,
            _gen_task: None,
        };
        this.regenerate(window, cx);
        this
    }

    /// The checked-file subset staged for the commit (`None` = all files).
    fn included(&self) -> Option<Vec<String>> {
        included_paths(&self.files, &self.excluded)
    }

    fn toggle_file(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.excluded.contains(path) {
            self.excluded.remove(path);
        } else {
            self.excluded.insert(path.to_string());
        }
        cx.notify();
    }

    /// (Re)generate the commit message via the current provider (headless).
    fn regenerate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.generating {
            return;
        }
        self.generating = true;
        let included = self.included();
        let store = self.store.clone();
        let task = store.update(cx, |store, cx| store.generate_commit_message(included, cx));
        let message = self.message.clone();
        self._gen_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.generating = false;
                match result {
                    Ok(text) => {
                        message.update(cx, |state, cx| state.set_value(text, window, cx));
                    }
                    Err(err) => log::warn!("commit message generation failed: {err}"),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Confirm the commit; an empty message starts regeneration instead.
    /// Returns whether the dialog should close.
    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let message = self.message.read(cx).value().trim().to_string();
        if message.is_empty() {
            self.regenerate(window, cx);
            return false;
        }
        let included = self.included();
        let feature_branch = if self.on_default_branch && self.create_feature_branch {
            Some(feature_branch_name(
                message.lines().next().unwrap_or("update"),
            ))
        } else {
            None
        };
        let action = self.action;
        self.store.update(cx, |store, _cx| {
            store.run_git_action(action, Some(message), included, feature_branch);
        });
        true
    }

    pub fn confirm_label(&self, cx: &App) -> String {
        if self.on_default_branch && self.create_feature_branch {
            let branch = feature_branch_name(
                self.message
                    .read(cx)
                    .value()
                    .lines()
                    .next()
                    .unwrap_or("update"),
            );
            return crate::tr!("git.commit.confirm_feature", branch = branch).into_owned();
        }
        match self.action {
            GitAction::CommitPush => crate::tr!("git.commit.confirm_push").into_owned(),
            _ => crate::tr!("git.commit.confirm").into_owned(),
        }
    }

    fn render_file_row(
        &self,
        index: usize,
        file: &GitFileEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let included = !self.excluded.contains(&file.path);
        let path = file.path.clone();
        let path_for_toggle = file.path.clone();
        h_flex()
            .debug_selector(move || format!("commit-file-{index}"))
            .w_full()
            .py_1()
            .px_1()
            .gap_2()
            .items_center()
            .child(
                Checkbox::new(("commit-file", index))
                    .checked(included)
                    .on_click(cx.listener(move |dialog, _checked: &bool, _window, cx| {
                        dialog.toggle_file(&path_for_toggle, cx);
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_code(cx)
                    .child(crate::widgets::fade_text(path)),
            )
            .when(file.insertions > 0, |this| {
                this.child(
                    div()
                        .flex_none()
                        .text_secondary(false, cx)
                        .text_color(cx.theme().success)
                        .child(format!("+{}", file.insertions)),
                )
            })
            .when(file.deletions > 0, |this| {
                this.child(
                    div()
                        .flex_none()
                        .text_secondary(false, cx)
                        .text_color(cx.theme().danger)
                        .child(format!("-{}", file.deletions)),
                )
            })
    }
}

impl Render for CommitDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;

        let branch_label = self
            .branch
            .clone()
            .unwrap_or_else(|| crate::tr!("git.commit.detached").into_owned());
        let branch_row = h_flex()
            .w_full()
            .gap_1p5()
            .items_center()
            .text_secondary(false, cx)
            .child(Icon::empty().path("icons/git-branch.svg"))
            .child(crate::tr!("git.commit.branch"))
            .child(
                div()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(cx.theme().foreground)
                    .child(branch_label),
            );

        let mut body = v_flex().w_full().gap_3().child(branch_row);

        if self.on_default_branch {
            let create = self.create_feature_branch;
            body = body.child(
                crate::material::notice(cx.theme().warning, cx).child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(cx.theme().tokens.spacing.sm)
                        .child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .text_title(cx)
                                .text_color(cx.theme().warning)
                                .child(Icon::new(IconName::TriangleAlert))
                                .child(crate::tr!("git.commit.default_warning_title")),
                        )
                        .child(
                            div()
                                .text_body(false, cx)
                                .text_color(muted)
                                .child(crate::tr!("git.commit.default_warning_body")),
                        )
                        .child(
                            Checkbox::new("commit-feature-branch")
                                .checked(create)
                                .label(crate::tr!("git.commit.create_feature_branch").into_owned())
                                .on_click(cx.listener(|dialog, checked: &bool, _window, cx| {
                                    dialog.create_feature_branch = *checked;
                                    cx.notify();
                                })),
                        ),
                ),
            );
        }

        let files_header =
            h_flex()
                .w_full()
                .justify_between()
                .items_center()
                .child(div().text_caption(cx).child(crate::tr!(
                    "git.commit.files_count",
                    count = self.files.len()
                )));
        let file_list = if self.files.is_empty() {
            crate::material::empty_row(crate::tr!("git.commit.no_changes"), cx).into_any_element()
        } else {
            crate::scroll::VirtualList::uniform(
                "commit-files",
                self.files.len(),
                cx.processor(move |dialog, range: std::ops::Range<usize>, _window, cx| {
                    range
                        .map(|index| {
                            div().pb_0p5().child(dialog.render_file_row(
                                index,
                                &dialog.files[index],
                                cx,
                            ))
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .w_full()
            // The list scrolls inside the dialog after a handful of rows so the
            // message field stays on screen; no owner sizes a list in a form.
            .max_h(px(180.))
            .into_any_element()
        };
        body = body.child(
            v_flex().w_full().gap_1().child(files_header).child(
                div()
                    .w_full()
                    .rounded(crate::material::radius_input(cx))
                    .border_1()
                    .border_color(cx.theme().border)
                    .pt_1()
                    .px_1()
                    // The last file row carries its gap.
                    .pb(if self.files.is_empty() {
                        cx.theme().tokens.spacing.xs
                    } else {
                        cx.theme().tokens.spacing.xxs
                    })
                    .child(file_list),
            ),
        );

        let message_header = h_flex()
            .w_full()
            .justify_between()
            .items_center()
            .child(
                div()
                    .text_caption(cx)
                    .child(crate::tr!("git.commit.message_label")),
            )
            .child(
                Button::new("commit-regenerate")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Undo)
                    .label(if self.generating {
                        crate::tr!("git.commit.generating")
                    } else {
                        crate::tr!("git.commit.regenerate")
                    })
                    .disabled(self.generating)
                    .on_click(cx.listener(|dialog, _, window, cx| {
                        dialog.regenerate(window, cx);
                    })),
            );
        body = body.child(
            v_flex()
                .w_full()
                .gap_1()
                .child(message_header)
                .child(Textarea::new(&self.message)),
        );

        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use gpui::{ScrollDelta, ScrollWheelEvent, TestAppContext, point};
    use tcode_core::git::GitStatus;
    use tcode_protocol::{
        EventEnvelope, GitStatusStatus, HostMessage, ServerEvent, Topic, encode_line,
    };

    #[gpui::test]
    fn a_long_change_list_lays_out_only_the_visible_files(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let status = GitStatus {
            is_repo: true,
            has_working_tree_changes: true,
            changed_files: (0..300)
                .map(|index| GitFileEntry {
                    path: format!("src/file-{index}.rs"),
                    insertions: 1,
                    deletions: 0,
                })
                .collect(),
            ..Default::default()
        };
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::GitStatus {
                        session_id: "session".into(),
                    },
                    event: ServerEvent::GitStatusReplaced(GitStatusStatus {
                        status: Some(status),
                        busy: false,
                    }),
                }))
                .unwrap(),
            )
            .unwrap();
        let deferred: Vec<String> = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|| executor.timer(Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
        store.update(cx, |store, _| store.select_session("session".into()));
        for line in deferred {
            incoming.try_send(line).unwrap();
        }
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (_dialog, cx) = cx.add_window_view(|window, cx| {
            CommitDialog::new(store.clone(), GitAction::Commit, window, cx)
        });
        cx.simulate_resize(gpui::size(px(640.), px(800.)));
        cx.update(|window, cx| _ = window.draw(cx));
        let first = cx.debug_bounds("commit-file-0").expect("first file row");
        assert!(
            cx.debug_bounds("commit-file-250").is_none(),
            "the dialog must not lay out all 300 file rows"
        );

        cx.simulate_event(ScrollWheelEvent {
            position: first.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(-100_000.))),
            ..Default::default()
        });
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("commit-file-299").is_some());
        assert!(cx.debug_bounds("commit-file-0").is_none());
    }
}
