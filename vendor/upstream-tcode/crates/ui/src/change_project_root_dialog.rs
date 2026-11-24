//! Change Project Path: point a project at another directory on the *host*,
//! either moving its directory there or re-pointing after a move made outside
//! Tcode. The host validates the path and carries the threads along; the only
//! native step is the directory picker, offered on a local attachment only.

use std::path::PathBuf;

use crate::overlay::{DialogActions, OverlayExt as _};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::checkbox::Checkbox;
use crate::widgets::input::{Input, InputState};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render,
    Styled as _, Window, div, prelude::FluentBuilder as _,
};
use gpui_base::{h_flex, v_flex};

use crate::store::WorkspaceStore;
use tcode_protocol::CommandResponse;

/// Whether this build can put up a directory picker at all.
const NATIVE_DIRECTORY_PICKER: bool = cfg!(feature = "native-dialogs");

pub(super) struct ChangeProjectRootDialog {
    store: Entity<WorkspaceStore>,
    project_id: String,
    current_root: PathBuf,
    path_input: Entity<InputState>,
    move_files: bool,
    /// A change is in flight on the host; a move may take a while.
    busy: bool,
    /// The last failure, host-authored where the host produced it.
    error: Option<String>,
}

pub(super) fn open(
    store: Entity<WorkspaceStore>,
    project_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some((project_name, _)) = store.read(cx).project_summary(&project_id) else {
        return;
    };
    let Some(current_root) = store.read(cx).project_root(&project_id) else {
        return;
    };
    let dialog =
        cx.new(|cx| ChangeProjectRootDialog::new(store, project_id, current_root, window, cx));
    let content = dialog.clone();
    let footer = dialog.clone();
    window.open_dialog(cx, move |builder, _, _| {
        let dialog_content = content.clone();
        let confirm = content.clone();
        builder
            .wide()
            .title(
                crate::tr!(
                    "sidebar.change_project_root_title",
                    project = project_name.clone()
                )
                .into_owned(),
            )
            .content(move |content_el, _, _| content_el.child(dialog_content.clone()))
            .footer(render_footer(&footer))
            .on_ok(move |_, window, cx| {
                confirm.update(cx, |dialog, cx| dialog.submit(window, cx));
                // The host result owns dismissal, including validation failures.
                false
            })
    });
}

/// A thread could not start because `cwd` is gone: offer to point the
/// project at where it went, or to remove it from Tcode.
pub(super) fn open_missing_directory(
    store: Entity<WorkspaceStore>,
    project_id: String,
    cwd: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    let Some((project_name, _)) = store.read(cx).project_summary(&project_id) else {
        return;
    };
    let cwd = cwd.to_string_lossy().into_owned();
    window.open_dialog(cx, move |builder, _window, _cx| {
        let change_store = store.clone();
        let change_id = project_id.clone();
        let remove_store = store.clone();
        let remove_id = project_id.clone();
        builder
            .title(crate::tr!("sidebar.missing_directory_title").into_owned())
            .content({
                let project_name = project_name.clone();
                let cwd = cwd.clone();
                move |content_el, _, cx| {
                    content_el.child(
                        div().text_secondary(false, cx).child(
                            crate::tr!(
                                "sidebar.missing_directory_description",
                                project = project_name.clone(),
                                path = cwd.clone()
                            )
                            .into_owned(),
                        ),
                    )
                }
            })
            .footer(
                DialogActions::new()
                    .child(
                        Button::new("missing-directory-cancel")
                            .label(crate::tr!("sidebar.cancel"))
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("missing-directory-remove")
                            .danger()
                            .label(crate::tr!("sidebar.remove_project"))
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                crate::sidebar::confirm_remove_project(
                                    remove_store.clone(),
                                    remove_id.clone(),
                                    window,
                                    cx,
                                );
                            }),
                    )
                    .child(
                        Button::new("missing-directory-change")
                            .primary()
                            .label(crate::tr!("sidebar.change_project_root"))
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                open(change_store.clone(), change_id.clone(), window, cx);
                            }),
                    )
                    .into_any_element(),
            )
    });
}

impl ChangeProjectRootDialog {
    fn new(
        store: Entity<WorkspaceStore>,
        project_id: String,
        current_root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = match store.read(cx).remote_host_name() {
            Some(host) => crate::tr!("sidebar.host_path_placeholder", host = host).into_owned(),
            None => crate::tr!("sidebar.path_placeholder").into_owned(),
        };
        let path_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder(placeholder);
            input.set_value(current_root.to_string_lossy().into_owned(), window, cx);
            input
        });
        Self {
            store,
            project_id,
            current_root,
            path_input,
            move_files: false,
            busy: false,
            error: None,
        }
    }

    /// The platform picker browses this machine, so a remote attachment types
    /// a host path instead.
    fn can_browse(&self, cx: &App) -> bool {
        NATIVE_DIRECTORY_PICKER && !self.store.read(cx).is_remote()
    }

    #[cfg(feature = "native-dialogs")]
    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(crate::tr!("sidebar.select_project").into_owned().into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await
                && let Some(path) = paths.pop()
            {
                let _ = this.update_in(cx, |dialog, window, cx| {
                    dialog.path_input.update(cx, |input, cx| {
                        input.set_value(path.to_string_lossy().into_owned(), window, cx)
                    });
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Unreachable: the button is only rendered when `can_browse`, which is
    /// false without the feature.
    #[cfg(not(feature = "native-dialogs"))]
    fn browse(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    /// Send the typed path to the host as-is; the host decides whether it is
    /// absolute, whether it exists, and what to say when it is refused.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let typed = self.path_input.read(cx).value().trim().to_owned();
        if typed.is_empty() {
            self.error = Some(crate::tr!("sidebar.path_required").into_owned());
            cx.notify();
            return;
        }
        self.error = None;
        self.busy = true;
        cx.notify();
        let project_id = self.project_id.clone();
        let move_files = self.move_files;
        let change = self.store.update(cx, |store, cx| {
            store.set_project_root(project_id, PathBuf::from(typed), move_files, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let response = change.await;
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.busy = false;
                match response {
                    Ok(CommandResponse::Unit) => window.close_dialog(cx),
                    Ok(other) => dialog.fail(format!("unexpected response: {other:?}"), cx),
                    Err(error) => dialog.fail(error.message, cx),
                }
            });
        })
        .detach();
    }

    fn fail(&mut self, reason: String, cx: &mut Context<Self>) {
        self.error = Some(crate::tr!("sidebar.path_rejected", reason = reason).into_owned());
        cx.notify();
    }
}

impl Render for ChangeProjectRootDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_browse = self.can_browse(cx);
        let path_hint = self
            .store
            .read(cx)
            .remote_host_name()
            .map(|host| crate::tr!("sidebar.host_path_hint", host = host).into_owned());
        v_flex()
            .gap_4()
            .child(
                div()
                    .text_secondary(false, cx)
                    .child(crate::tr!("sidebar.change_project_root_description")),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div().text_secondary(false, cx).child(
                            crate::tr!(
                                "sidebar.change_project_root_current",
                                path = self.current_root.to_string_lossy().into_owned()
                            )
                            .into_owned(),
                        ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(Input::new(&self.path_input).flex_1())
                            .when(can_browse, |row| {
                                row.child(
                                    Button::new("browse-project-root")
                                        .label(crate::tr!("sidebar.browse"))
                                        .on_click(cx.listener(|dialog, _, window, cx| {
                                            dialog.browse(window, cx);
                                        })),
                                )
                            }),
                    )
                    .when_some(path_hint, |column, hint| {
                        column.child(div().text_secondary(false, cx).child(hint))
                    }),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Checkbox::new("move-project-files")
                            .checked(self.move_files)
                            .disabled(self.busy)
                            .label(crate::tr!("sidebar.change_project_root_move").into_owned())
                            .on_click(cx.listener(|dialog, checked: &bool, _window, cx| {
                                dialog.move_files = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_secondary(false, cx)
                            .child(crate::tr!("sidebar.change_project_root_move_help")),
                    ),
            )
            .when(self.busy, |column| {
                column.child(
                    div()
                        .text_secondary(false, cx)
                        .child(crate::tr!("sidebar.change_project_root_working")),
                )
            })
            .when_some(self.error.clone(), |column, error| {
                column.child(
                    div()
                        .text_secondary(false, cx)
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
    }
}

fn render_footer(dialog: &Entity<ChangeProjectRootDialog>) -> AnyElement {
    let submit = dialog.clone();
    DialogActions::new()
        .child(
            Button::new("change-project-root-cancel")
                .label(crate::tr!("sidebar.cancel"))
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                }),
        )
        .child(
            Button::new("change-project-root-apply")
                .primary()
                .label(crate::tr!("sidebar.change_project_root_action"))
                .on_click(move |_, window, cx| {
                    submit.update(cx, |dialog, cx| dialog.submit(window, cx));
                }),
        )
        .into_any_element()
}
