//! Move to project: pick another of the host's projects for a thread. The host
//! moves it and restarts its provider session there with a handoff of the
//! conversation so far.

use crate::overlay::{DialogActions, OverlayExt as _};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_base::v_flex;
use tcode_protocol::Command;

use crate::store::WorkspaceStore;

pub(crate) struct MoveThreadDialog {
    store: Entity<WorkspaceStore>,
    session_id: String,
    selected: Option<String>,
    busy: bool,
    error: Option<String>,
}

/// Open the picker for `session_id`; its current project is not offered.
pub(crate) fn open(
    store: Entity<WorkspaceStore>,
    session_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let dialog = cx.new(|_| MoveThreadDialog {
        store,
        session_id,
        selected: None,
        busy: false,
        error: None,
    });
    window.open_dialog(cx, move |builder, _, cx| {
        let content = dialog.clone();
        builder
            .title(crate::tr!("sidebar.move_thread_title").into_owned())
            .content(move |body, _, _| body.child(content.clone()))
            .footer(render_footer(&dialog, cx))
    });
}

impl MoveThreadDialog {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.selected.clone().filter(|_| !self.busy) else {
            return;
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        window.refresh();
        let task = self.store.update(cx, |store, cx| {
            store.command(
                Command::MoveSession {
                    session_id: self.session_id.clone(),
                    project_id,
                    cwd: None,
                },
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.busy = false;
                match result {
                    Ok(_) => window.close_dialog(cx),
                    Err(error) => {
                        window.refresh();
                        dialog.error = Some(
                            crate::tr!("sidebar.move_thread_failed", reason = error.message)
                                .into_owned(),
                        );
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
}

impl Render for MoveThreadDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let current = store
            .sidebar_sessions()
            .into_iter()
            .find(|meta| meta.id == self.session_id)
            .and_then(|meta| meta.project_id);
        let projects: Vec<_> = store
            .projects()
            .into_iter()
            .filter(|project| Some(&project.id) != current.as_ref())
            .collect();
        let empty = projects.is_empty();
        let mut list = v_flex()
            .id("move-thread-projects")
            .role(gpui::Role::ListBox)
            .aria_label(crate::tr!("sidebar.move_thread_title").into_owned())
            .w_full()
            .max_h(gpui::px(320.))
            .overflow_y_scroll()
            .gap_0p5();
        for (index, project) in projects.into_iter().enumerate() {
            let selected = self.selected.as_deref() == Some(project.id.as_str());
            let id = project.id.clone();
            let root = project.root.to_string_lossy().into_owned();
            list = list.child(
                crate::material::menu_item_frame(false, cx)
                    .id(("move-thread-project", index))
                    .role(gpui::Role::ListBoxOption)
                    .aria_label(format!("{}: {root}", project.name))
                    .aria_selected(selected)
                    .w_full()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(cx.theme().list_active))
                    .hover(|style| style.bg(cx.theme().muted))
                    .on_click(cx.listener(move |dialog, _, window, cx| {
                        dialog.selected = Some(id.clone());
                        dialog.error = None;
                        cx.notify();
                        // The footer's Move button is drawn by the dialog frame.
                        window.refresh();
                    }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_body(false, cx).child(project.name.clone()))
                            .child(
                                div()
                                    .text_secondary(false, cx)
                                    .overflow_hidden()
                                    .child(crate::widgets::fade_text(root)),
                            ),
                    ),
            );
        }
        v_flex()
            .gap_3()
            .when(empty, |column| {
                column.child(
                    div()
                        .text_secondary(false, cx)
                        .child(crate::tr!("sidebar.move_thread_empty")),
                )
            })
            .when(!empty, |column| column.child(list))
            .child(
                div()
                    .text_secondary(false, cx)
                    .child(crate::tr!("sidebar.move_thread_confirm")),
            )
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

fn render_footer(dialog: &Entity<MoveThreadDialog>, cx: &App) -> AnyElement {
    let ready = {
        let dialog = dialog.read(cx);
        dialog.selected.is_some() && !dialog.busy
    };
    let submit = dialog.clone();
    DialogActions::new()
        .child(
            Button::new("move-thread-cancel")
                .label(crate::tr!("sidebar.cancel"))
                .on_click(|_, window, cx| window.close_dialog(cx)),
        )
        .child(
            Button::new("move-thread-apply")
                .primary()
                .disabled(!ready)
                .label(crate::tr!("sidebar.move_thread_action"))
                .on_click(move |_, window, cx| {
                    submit.update(cx, |dialog, cx| dialog.submit(window, cx));
                }),
        )
        .into_any_element()
}
