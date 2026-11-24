//! Add Project: create or pick a directory on the *host* and, optionally, import the
//! external-agent threads it already has.
//!
//! Everything here is host state reached over the pipe — the recents scan, the
//! project root and the import run. The only native step is the directory
//! picker, which browses *this* machine; it is offered when this build has one
//! and the attachment is local, and never as a substitute for the host's own
//! judgment about a path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::overlay::{DialogActions, OverlayExt as _};
use crate::scroll::ScrollableElement as _;
use crate::sizing::fit_viewport;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::progress::Progress;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};
use crate::time::{humanize_ago, now_secs};
use tcode_protocol::{CommandResponse, ExternalImportState, ExternalThread, RecentDir, SourceTool};

const RECENT_LIMIT: usize = 15;
const RECENT_ROW_HEIGHT_ESTIMATE: f32 = 64.;
const RECENT_VIEWPORT_MAX_HEIGHT: f32 = 390.;
/// The recents list needs a definite height to scroll, and the dialog's flex
/// content height is not readable from its builder; reserve room for the
/// project inputs and footer so recents scroll instead of pushing the Open
/// button off a short window.
const RECENT_VIEWPORT_CHROME: f32 = 380.;

enum RecentState {
    Loading,
    Ready(Vec<RecentDir>),
    Failed(String),
}

/// Whether this build can put up a directory picker at all.
const NATIVE_DIRECTORY_PICKER: bool = cfg!(feature = "native-dialogs");

pub(super) struct AddProjectDialog {
    store: Entity<WorkspaceStore>,
    path_input: Entity<InputState>,
    recent: RecentState,
    /// The last failure to show under the path row. Host-authored where the
    /// host produced it, so the user reads the host's own reason.
    error: Option<String>,
    name_input: Entity<InputState>,
    pending_created_project: Option<String>,
    _subscriptions: Vec<gpui::Subscription>,
}

pub(super) fn open(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut App) {
    let dialog = cx.new(|cx| AddProjectDialog::new(store, window, cx));
    dialog.update(cx, |dialog, cx| dialog.scan(cx));
    let content = dialog.clone();
    let footer = dialog.clone();
    window.open_dialog(cx, move |builder, _, _| {
        let dialog_content = content.clone();
        let confirm = content.clone();
        builder
            .wide()
            .title(crate::tr!("sidebar.add_project").into_owned())
            .content(move |content_el, _, _| content_el.child(dialog_content.clone()))
            .footer(render_add_footer(&footer))
            .on_ok(move |_, window, cx| {
                confirm.update(cx, |dialog, cx| {
                    if dialog
                        .name_input
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                    {
                        dialog.create_new_project(window, cx);
                    } else {
                        dialog.open_typed_path(window, cx);
                    }
                });
                // The host result owns dismissal, including validation failures.
                false
            })
    });
}

impl AddProjectDialog {
    fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let placeholder = match store.read(cx).remote_host_name() {
            Some(host) => crate::tr!("sidebar.host_path_placeholder", host = host).into_owned(),
            None => crate::tr!("sidebar.path_placeholder").into_owned(),
        };
        let path_input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let name_input = cx
            .new(|cx| InputState::new(window, cx).placeholder(crate::tr!("sidebar.project_name")));
        let subscriptions = vec![
            cx.subscribe(&name_input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &store,
                window,
                |dialog, _, change: &crate::store::StoreChange, window, cx| {
                    if change.topic == TopicKind::Index {
                        dialog.open_created_project(window, cx);
                    }
                },
            ),
        ];
        Self {
            name_input,
            pending_created_project: None,
            _subscriptions: subscriptions,
            store,
            path_input,
            recent: RecentState::Loading,
            error: None,
        }
    }

    /// Whether the platform directory picker is meaningful here. It browses this
    /// machine, so a remote attachment must type a host path instead — the
    /// picker would silently produce a path the host cannot resolve.
    fn can_browse(&self, cx: &App) -> bool {
        NATIVE_DIRECTORY_PICKER && !self.store.read(cx).is_remote()
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let recent = store.update(cx, |store, cx| store.scan_external_history(cx));
        cx.spawn(async move |this, cx| {
            let recent = recent.await;
            let _ = this.update(cx, |dialog, cx| {
                dialog.recent = match recent {
                    Ok(recent) => RecentState::Ready(recent),
                    Err(error) => RecentState::Failed(error),
                };
                cx.notify();
            });
        })
        .detach();
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
                    dialog.create_draft(path, window, cx);
                });
            }
        })
        .detach();
    }

    /// Send the typed text to the host as-is. Whether it is absolute and whether
    /// it is a directory are facts about the host's filesystem and path rules,
    /// so the host decides and its answer is what the user reads. The path is
    /// never canonicalized or otherwise touched here.
    fn open_typed_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.path_input.read(cx).value().trim().to_owned();
        if typed.is_empty() {
            self.error = Some(crate::tr!("sidebar.path_required").into_owned());
            cx.notify();
            return;
        }
        self.create_draft(PathBuf::from(typed), window, cx);
    }

    fn create_draft(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let create = self
            .store
            .update(cx, |store, cx| store.create_project(path.clone(), cx));
        cx.spawn_in(window, async move |this, cx| {
            let project_id = match create.await {
                Ok(CommandResponse::ProjectId(Some(project_id))) => project_id,
                Ok(other) => {
                    let _ = this.update_in(cx, |dialog, _, cx| {
                        dialog.fail(format!("unexpected create-project response: {other:?}"), cx);
                    });
                    return;
                }
                Err(error) => {
                    let _ = this.update_in(cx, |dialog, _, cx| {
                        dialog.fail(error.message, cx);
                    });
                    return;
                }
            };
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.store.update(cx, |store, cx| {
                    store.start_draft(project_id, path, cx);
                });
                window.close_dialog(cx);
            });
        })
        .detach();
    }

    fn create_new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let name = self.name_input.read(cx).value().to_string();
        let create = self
            .store
            .update(cx, |store, cx| store.create_new_project(name, cx));
        cx.spawn_in(window, async move |this, cx| {
            let response = create.await;
            let _ = this.update_in(cx, |dialog, window, cx| match response {
                Ok(CommandResponse::ProjectId(Some(id))) => {
                    dialog.pending_created_project = Some(id);
                    dialog.open_created_project(window, cx);
                }
                Ok(_) => dialog.fail(
                    crate::tr!("sidebar.create_project_no_response").into_owned(),
                    cx,
                ),
                Err(error) => dialog.fail(error.message, cx),
            });
        })
        .detach();
        cx.notify();
    }

    fn open_created_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.pending_created_project.as_ref() else {
            return;
        };
        let Some(root) = self.store.read(cx).project_root(id) else {
            return;
        };
        let id = self.pending_created_project.take().unwrap();
        self.store
            .update(cx, |store, cx| store.start_draft(id, root, cx));
        window.close_dialog(cx);
    }

    fn fail(&mut self, reason: String, cx: &mut Context<Self>) {
        self.error = Some(crate::tr!("sidebar.path_rejected", reason = reason).into_owned());
        cx.notify();
    }

    fn choose_recent(&mut self, recent: RecentDir, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let path = recent.path.clone();
        let create = self
            .store
            .update(cx, |store, cx| store.create_project(path, cx));
        let threads = recent.threads;
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let project_id = match create.await {
                Ok(CommandResponse::ProjectId(Some(project_id))) => project_id,
                Ok(other) => {
                    let _ = this.update_in(cx, |dialog, _, cx| {
                        dialog.fail(format!("unexpected create-project response: {other:?}"), cx);
                    });
                    return;
                }
                Err(error) => {
                    let _ = this.update_in(cx, |dialog, _, cx| dialog.fail(error.message, cx));
                    return;
                }
            };
            // Subscribe before starting: an import short enough to finish
            // before the start reply lands is only recoverable through the
            // retained status snapshot.
            let import = store.update(cx, |store, cx| {
                store.watch_external_import(&project_id);
                store.start_external_import(&project_id, threads, cx)
            });
            let started = import.await;
            if !matches!(started, Ok(CommandResponse::ExternalImportStarted(true))) {
                store.update(cx, |store, _cx| {
                    store.unwatch_external_import(&project_id);
                });
                let reason = match started {
                    Err(error) => error.message,
                    _ => crate::tr!("sidebar.import_refused").into_owned(),
                };
                let _ = this.update_in(cx, |dialog, _, cx| {
                    dialog.error =
                        Some(crate::tr!("sidebar.import_failed", reason = reason).into_owned());
                    cx.notify();
                });
                return;
            }
            let _ = this.update_in(cx, |dialog, window, cx| {
                window.close_dialog(cx);
                let store = dialog.store.clone();
                let progress = cx.new(|cx| ImportProgress::new(store, project_id, cx));
                let content = progress.clone();
                window.open_dialog(cx, move |builder, _, _| {
                    let progress_content = content.clone();
                    builder
                        .title(crate::tr!("sidebar.importing").into_owned())
                        .close_button(false)
                        .overlay_closable(false)
                        .keyboard(false)
                        .content(move |content_el, _, _| content_el.child(progress_content.clone()))
                });
            });
        })
        .detach();
    }

    fn render_recent(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        match &self.recent {
            RecentState::Loading => v_flex()
                .child(crate::material::empty_row(
                    crate::tr!("sidebar.recent_loading"),
                    cx,
                ))
                .child(Progress::new("recent-directories-loading").loading(true))
                .into_any_element(),
            RecentState::Failed(error) => crate::material::empty_row(
                crate::tr!("sidebar.recent_failed", reason = error.clone()).into_owned(),
                cx,
            )
            .text_color(cx.theme().danger)
            .into_any_element(),
            RecentState::Ready(recent) if recent.is_empty() => {
                crate::material::empty_row(crate::tr!("sidebar.recent_empty"), cx)
                    .into_any_element()
            }
            RecentState::Ready(recent) => {
                // Keep the viewport and the flex column separate. Putting the
                // max height on the column itself lets flexbox shrink every row
                // until there is no overflow left for the wheel to scroll.
                let mut rows = v_flex().w_full().gap_1();
                for (index, recent) in recent.iter().take(RECENT_LIMIT).enumerate() {
                    let selected = recent.clone();
                    let name = directory_name(&recent.path);
                    let accessible_name =
                        crate::tr!("sidebar.open_recent", name = name.clone()).into_owned();
                    let path = recent.path.display().to_string();
                    let ago = humanize_ago(now_secs().saturating_sub(recent.last_active_ms / 1000));
                    let counts = tool_counts(&recent.threads);
                    rows = rows.child(
                        crate::material::panel_row(
                            format!("recent-directory-{index}"),
                            accessible_name.into(),
                            false,
                            cx,
                        )
                        .flex_none()
                        .cursor_pointer()
                        .on_click(cx.listener(move |dialog, _, window, cx| {
                            dialog.choose_recent(selected.clone(), window, cx);
                        }))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    h_flex()
                                        .w_full()
                                        .justify_between()
                                        .gap_3()
                                        .child(
                                            div()
                                                .min_w_0()
                                                .text_body(false, cx)
                                                .font_bold()
                                                .overflow_hidden()
                                                .child(crate::widgets::fade_text(name)),
                                        )
                                        .child(
                                            div().flex_none().text_secondary(false, cx).child(ago),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_secondary(false, cx)
                                        .overflow_hidden()
                                        .child(crate::widgets::fade_text(path)),
                                )
                                .child(div().text_secondary(false, cx).child(counts)),
                        ),
                    );
                }
                let visible_rows = recent.len().min(RECENT_LIMIT) as f32;
                let viewport_height = fit_viewport(
                    (visible_rows * RECENT_ROW_HEIGHT_ESTIMATE).min(RECENT_VIEWPORT_MAX_HEIGHT),
                    window.viewport_size().height - px(RECENT_VIEWPORT_CHROME),
                );
                div()
                    .id("recent-directory-list")
                    .w_full()
                    .h(viewport_height)
                    .overflow_y_scrollbar()
                    .child(div().size_full().child(rows))
                    .into_any_element()
            }
        }
    }
}

impl Render for AddProjectDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let host = self.store.read(cx).remote_host_name().map(str::to_owned);
        let recent_label = match &host {
            Some(host) => crate::tr!("sidebar.recent_activity_host", host = host).into_owned(),
            None => crate::tr!("sidebar.recent_activity").into_owned(),
        };
        let path_hint = host
            .as_ref()
            .map(|host| crate::tr!("sidebar.host_path_hint", host = host).into_owned());
        let can_browse = self.can_browse(cx);
        let destination = format!(
            "~/TcodeProjects/{}",
            self.name_input.read(cx).value().trim()
        );
        let destination_hint = match &host {
            Some(host) => crate::tr!(
                "sidebar.new_project_destination_host",
                host = host,
                path = destination
            )
            .into_owned(),
            None => crate::tr!("sidebar.new_project_destination", path = destination).into_owned(),
        };
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_caption(cx)
                            .child(crate::tr!("sidebar.create_new_project")),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(Input::new(&self.name_input).flex_1())
                            .child(
                                Button::new("create-new-project")
                                    .label(crate::tr!("sidebar.create"))
                                    .on_click(cx.listener(|dialog, _, window, cx| {
                                        dialog.create_new_project(window, cx)
                                    })),
                            ),
                    )
                    .child(div().text_secondary(false, cx).child(destination_hint)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(Input::new(&self.path_input).flex_1())
                            .when(can_browse, |row| {
                                row.child(
                                    Button::new("browse-project-directory")
                                        .label(crate::tr!("sidebar.browse"))
                                        .on_click(cx.listener(|dialog, _, window, cx| {
                                            dialog.browse_clicked(window, cx);
                                        })),
                                )
                            }),
                    )
                    .when_some(path_hint, |column, hint| {
                        column.child(div().text_secondary(false, cx).child(hint))
                    })
                    .when_some(self.error.clone(), |column, error| {
                        column.child(
                            div()
                                .text_secondary(false, cx)
                                .text_color(cx.theme().danger)
                                .child(error),
                        )
                    }),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(div().text_caption(cx).child(recent_label))
                            .child(
                                div()
                                    .text_secondary(false, cx)
                                    .child(crate::tr!("sidebar.recent_import_hint")),
                            ),
                    )
                    .child(self.render_recent(window, cx)),
            )
    }
}

impl AddProjectDialog {
    #[cfg(feature = "native-dialogs")]
    fn browse_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.browse(window, cx);
    }

    /// Unreachable: the button is only rendered when `can_browse`, which is
    /// false without the feature.
    #[cfg(not(feature = "native-dialogs"))]
    fn browse_clicked(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}
}

/// Renders the host's replicated import status. It owns no progress state of
/// its own, so a completion that arrived before this view existed still shows
/// up: the subscription snapshot carries the retained latest run.
struct ImportProgress {
    store: Entity<WorkspaceStore>,
    project_id: String,
    _subscription: gpui::Subscription,
}

impl ImportProgress {
    fn new(store: Entity<WorkspaceStore>, project_id: String, cx: &mut Context<Self>) -> Self {
        Self {
            _subscription: observe_store_topics(&store, &[TopicKind::ExternalImport], cx),
            store,
            project_id,
        }
    }
}

impl Render for ImportProgress {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self
            .store
            .read(cx)
            .external_import_status(&self.project_id)
            .map(|status| status.state.clone());
        // The "n of N" line describes a run still in flight; the summary below
        // replaces it once the host reports the outcome.
        let running = match &state {
            Some(ExternalImportState::Progress { done, total, tool }) => {
                Some((*done, *total, tool.clone()))
            }
            _ => None,
        };
        let summary = match state {
            Some(ExternalImportState::Finished { imported, skipped }) => Some((imported, skipped)),
            _ => None,
        };
        // A run with nothing to import is complete the moment it starts; a
        // status that has not arrived yet is not.
        let percent = match running {
            Some((_, 0, _)) | None if summary.is_none() => 0.0,
            Some((done, total, _)) if total > 0 => done as f32 * 100.0 / total as f32,
            _ => 100.0,
        };
        let project_id = self.project_id.clone();
        let store = self.store.clone();
        v_flex()
            .gap_3()
            .py_2()
            .child(Progress::new("external-import-progress").value(percent))
            .when_some(running, |column, (done, total, tool)| {
                column.child(div().text_secondary(false, cx).child(crate::tr!(
                    "sidebar.import_progress",
                    done = done,
                    total = total,
                    tool = tool
                )))
            })
            .when_some(summary, |column, (imported, skipped)| {
                column
                    .child(div().text_title(cx).child(crate::tr!(
                        "sidebar.import_summary",
                        imported = imported,
                        skipped = skipped
                    )))
                    .child(
                        h_flex().w_full().justify_end().child(
                            Button::new("external-import-ok")
                                .primary()
                                .label(crate::tr!("sidebar.import_ok"))
                                .on_click(move |_, window, cx| {
                                    store.update(cx, |store, _cx| {
                                        store.unwatch_external_import(&project_id);
                                    });
                                    window.close_dialog(cx);
                                }),
                        ),
                    )
            })
    }
}

fn render_add_footer(dialog: &Entity<AddProjectDialog>) -> AnyElement {
    let open = dialog.clone();
    DialogActions::new()
        .child(
            Button::new("add-project-cancel")
                .label(crate::tr!("sidebar.cancel"))
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                }),
        )
        .child(
            Button::new("add-project-open")
                .primary()
                .label(crate::tr!("sidebar.open"))
                .on_click(move |_, window, cx| {
                    open.update(cx, |dialog, cx| dialog.open_typed_path(window, cx));
                }),
        )
        .into_any_element()
}

fn directory_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

fn tool_counts(threads: &[ExternalThread]) -> String {
    let mut counts = HashMap::new();
    for thread in threads {
        *counts.entry(thread.source).or_insert(0_usize) += 1;
    }
    [
        SourceTool::ClaudeCode,
        SourceTool::ClaudeDesktop,
        SourceTool::T3Code,
        SourceTool::CodexCli,
        SourceTool::CodexDesktop,
    ]
    .into_iter()
    .filter_map(|source| {
        counts.get(&source).map(|&count| {
            let tool = source.display_name();
            if count == 1 {
                crate::tr!("sidebar.recent_tool_threads_one", tool = tool).into_owned()
            } else {
                crate::tr!("sidebar.recent_tool_threads", tool = tool, count = count).into_owned()
            }
        })
    })
    .collect::<Vec<_>>()
    .join(" · ")
}

#[cfg(test)]
mod tests {
    use std::future::Future as _;

    use gpui::{TestAppContext, VisualTestContext};
    use tcode_runtime::pipe::{HostServices, spawn_host};
    use tcode_services::store::SessionStore;

    use super::*;
    use crate::store::WorkspaceAttachment;

    /// A remote client types a path that means nothing on its own machine. The
    /// answer — accept or reject, and why — comes from the host's filesystem,
    /// and the client shows the host's words rather than judging the string
    /// against its own path rules.
    #[gpui::test]
    fn a_remote_project_root_is_judged_by_the_host(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("add-project-remote");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let host = spawn_host(
            SessionStore::open_at(root.join("data")).unwrap(),
            HostServices::default(),
        )
        .expect("spawn add-project test host");
        let link = tcode_client::HostLink::new(host.to_host.clone(), host.from_host.clone());
        let mut pump = std::pin::pin!(link.pump());
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link.clone(),
                WorkspaceAttachment::Remote {
                    host_id: "build-box".into(),
                    host_name: "build-box".into(),
                },
                None,
                None,
                false,
                cx,
            )
        });
        fn wait_until(
            cx: &mut TestAppContext,
            store: &Entity<WorkspaceStore>,
            mut pump: std::pin::Pin<&mut impl std::future::Future<Output = ()>>,
            description: &str,
            mut ready: impl FnMut(&TestAppContext) -> bool,
        ) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
            loop {
                // Host replies arrive on an OS thread. Decode them here so ACKs
                // wake GPUI tasks on the test scheduler's own thread.
                let _ = pump.as_mut().poll(&mut task_cx);
                store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                cx.run_until_parked();
                if ready(cx) {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "waiting for {description}"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        wait_until(cx, &store, pump.as_mut(), "host baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        // The dialog closes itself on success, which routes through the
        // window's overlay root — so mount it the way the app does.
        let built = std::rc::Rc::new(std::cell::RefCell::new(None));
        let capture = built.clone();
        let store_for_view = store.clone();
        let (_root, cx) = cx.add_window_view(move |window, cx| {
            let dialog = cx.new(|cx| AddProjectDialog::new(store_for_view.clone(), window, cx));
            *capture.borrow_mut() = Some(dialog.clone());
            gpui_base::Root::new(dialog, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        let dialog = built.borrow().clone().expect("dialog was built");

        let type_and_open = |cx: &mut VisualTestContext, path: &str| {
            let path = path.to_owned();
            cx.update(|window, cx| {
                dialog.update(cx, |dialog, cx| {
                    dialog
                        .path_input
                        .update(cx, |input, cx| input.set_value(path.clone(), window, cx));
                    dialog.open_typed_path(window, cx);
                });
            });
        };

        // A Windows-style path on a Unix host (or a nonexistent one on Windows):
        // rejected there, not here.
        type_and_open(cx, r"C:\Users\dev\src");
        wait_until(cx, &store, pump.as_mut(), "host path refusal", |cx| {
            dialog.read_with(cx, |dialog, _| dialog.error.is_some())
        });
        let error = dialog.read_with(cx, |dialog, _| dialog.error.clone());
        assert!(
            error.is_some_and(|error| error.contains(r"C:\Users\dev\src")),
            "the dialog must show the host's reason for refusing the path"
        );
        assert_eq!(
            smol::block_on(host.update_state_for_test(|state, _| state.projects.len()))
                .expect("read projects"),
            0,
            "a refused root must not create a project"
        );

        // A directory that exists on the host is accepted even though this
        // client never looked at it.
        type_and_open(cx, workspace.to_str().unwrap());
        wait_until(cx, &store, pump.as_mut(), "host project draft", |cx| {
            store.read_with(cx, |store, _| store.active_session_id().is_some())
        });
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.error.clone()), None);
        assert_eq!(
            smol::block_on(host.update_state_for_test(|state, _| {
                state
                    .projects
                    .iter()
                    .map(|project| project.root.clone())
                    .collect::<Vec<_>>()
            }))
            .expect("read projects"),
            vec![workspace.clone()]
        );

        let mut shutdown = std::pin::pin!(link.shutdown());
        wait_until(cx, &store, pump.as_mut(), "host shutdown", |_| {
            let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
            match shutdown.as_mut().poll(&mut task_cx) {
                std::task::Poll::Ready(result) => {
                    result.expect("stop host");
                    true
                }
                std::task::Poll::Pending => false,
            }
        });
        host.stopped.recv_blocking().expect("host stopped");
        let _ = std::fs::remove_dir_all(root);
    }
}
