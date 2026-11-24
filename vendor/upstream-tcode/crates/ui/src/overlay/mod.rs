mod dialog;
mod notification;

pub(crate) use dialog::DIALOG_LIST_HEIGHT;
pub use dialog::{AlertDialog, Dialog, DialogActions, DialogButtons, DialogContent};
pub use notification::{Notification, NotificationType};

use std::rc::Rc;

use gpui::{
    App, AppContext as _, Context, Div, ElementId, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Refineable as _, Render, Stateful, StyleRefinement, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};

use crate::theme::ActiveTheme as _;
use dialog::ActiveDialog;
use notification::NotificationList;

/// The release guard for popups gpui-base dismisses on a press outside them.
///
/// gpui-base closes a popover, menu or sheet on the outside press itself,
/// and GPUI then delivers that press's release, and the click it completes,
/// to whatever the popup uncovered. Arming the guard from the dismissal
/// swallows that one release; the next press disarms it, so a dismissal by
/// Escape or by choosing an item costs nothing. Mount `release_listener`
/// with the trigger, not the transient surface, which is gone by the release.
#[derive(Clone, Default)]
pub(crate) struct OutsideDismissal(Rc<std::cell::Cell<bool>>);

impl OutsideDismissal {
    pub(crate) fn new(id: ElementId, window: &mut Window, cx: &mut App) -> Self {
        window
            .use_keyed_state((id, "outside-dismissal"), cx, |_, _| Self::default())
            .read(cx)
            .clone()
    }

    /// The popup was dismissed; the release of the press that did it, if
    /// one is in progress, is swallowed.
    pub(crate) fn dismissed(&self) {
        self.0.set(true);
    }

    /// Dismissed by `event`, which is swallowed as well: a press that closes
    /// a menu or a picker does nothing else.
    pub(crate) fn consume(&self, window: &mut Window, cx: &mut App) {
        self.dismissed();
        window.prevent_default();
        cx.stop_propagation();
    }

    pub(crate) fn release_listener(&self) -> impl IntoElement {
        let armed = self.0.clone();
        gpui::canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let disarm = armed.clone();
                window.on_mouse_event(move |_: &gpui::MouseDownEvent, phase, _, _| {
                    if phase.capture() {
                        disarm.set(false);
                    }
                });
                window.on_mouse_event(move |_: &gpui::MouseUpEvent, phase, window, cx| {
                    if phase.capture() && armed.replace(false) {
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                });
            },
        )
        .absolute()
        .size_0()
    }
}

/// tcode's per-window presentation on the Base [`gpui_base::Root`]: the
/// modal and toast layers.
pub(crate) struct Overlays {
    dialogs: Vec<ActiveDialog>,
    sheets: Vec<gpui::WeakEntity<SheetBack>>,
    notifications: Entity<NotificationList>,
}

/// Register the overlays every window root mounts. Call before opening windows.
pub(crate) fn init(cx: &mut App) {
    gpui_base::Root::register_plugin(cx, Overlays::new);
}

impl Overlays {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            dialogs: Vec::new(),
            sheets: Vec::new(),
            notifications: cx.new(|cx| NotificationList::new(window, cx)),
        }
    }

    fn update<R>(
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) -> R,
    ) -> R {
        let overlays = gpui_base::Root::read(window, cx)
            .plugin::<Self>()
            .expect("tcode_ui::overlay::init must run before the window opens");
        overlays.update(cx, |overlays, cx| f(overlays, window, cx))
    }

    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.dialogs.pop() {
            if dialog.focus_handle.contains_focused(window, cx)
                && let Some(previous) = dialog
                    .previous_focus_handle
                    .and_then(|focus| focus.upgrade())
            {
                previous.focus(window, cx);
            }
            cx.notify();
        }
    }

    fn close_all_dialogs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self
            .dialogs
            .first()
            .and_then(|dialog| dialog.previous_focus_handle.clone())
            .and_then(|focus| focus.upgrade());
        self.dialogs.clear();
        if let Some(previous) = previous {
            previous.focus(window, cx);
        }
        cx.notify();
    }
}

impl gpui_base::RootPlugin for Overlays {
    fn style(&self, surface: &mut Stateful<Div>, _window: &mut Window, cx: &mut App) {
        surface.style().refine(
            &StyleRefinement::default()
                .bg(crate::material::canvas(cx))
                .text_color(cx.theme().foreground)
                .font_family(cx.theme().font_family.clone()),
        );
    }
}

impl Render for Overlays {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dialog_count = self.dialogs.len();
        let dialogs = self
            .dialogs
            .iter()
            .enumerate()
            .map(|(index, active)| {
                let dialog = (active.builder)(Dialog::new(cx), window, cx);
                dialog
                    .layer(index, index + 1 == dialog_count)
                    .focus_handle(active.focus_handle.clone())
            })
            .collect::<Vec<_>>();

        // Dialogs and toasts share the shell's one safe content rectangle: a
        // dialog centred in the raw window would sit under a notch, and a toast
        // pinned to the corner would sit under the status bar.
        let compact = crate::window_seam::window_is_compact(window, cx);
        let seam = crate::window_seam::content_insets(window);
        let inset = px(crate::material::COMPACT_PAGE_INSET);
        div()
            .absolute()
            .inset_0()
            // The edit menu floats above what was selected, under any dialog.
            .when(!dialogs.is_empty(), |root| {
                root.child(
                    div()
                        .absolute()
                        .inset_0()
                        .pt(seam.top)
                        .pb(seam.bottom)
                        .pl(seam.left)
                        .pr(seam.right)
                        .children(dialogs),
                )
            })
            .child(
                div()
                    .debug_selector(|| "notification-position".into())
                    .absolute()
                    .when(compact, |el| {
                        el.left(seam.left + inset)
                            .right(seam.right + inset)
                            .bottom(seam.bottom + inset)
                            .flex()
                            .justify_center()
                    })
                    .when(!compact, |el| {
                        el.top_0()
                            .right_0()
                            .mt(seam.top + inset)
                            .mr(seam.right + inset)
                    })
                    .child(self.notifications.clone()),
            )
    }
}

fn open_notification_dialog(note: Entity<Notification>, window: &mut Window, cx: &mut App) {
    let title = note.read(cx).dialog_title();
    window.open_dialog(cx, move |dialog, _, _| {
        let note = note.clone();
        dialog
            .when_some(title.clone(), |dialog, title| dialog.title(title))
            .content(move |content, window, cx| {
                content.child(note.update(cx, |note, cx| note.dialog_content(window, cx)))
            })
    });
}

/// Imperative overlay operations used by application views.
pub trait OverlayExt {
    fn open_dialog<F>(&mut self, cx: &mut App, build: F)
    where
        F: Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static;
    fn open_alert_dialog<F>(&mut self, cx: &mut App, build: F)
    where
        F: Fn(AlertDialog, &mut Window, &mut App) -> AlertDialog + 'static;
    fn close_dialog(&mut self, cx: &mut App);
    fn close_all_dialogs(&mut self, cx: &mut App);
    fn push_notification(&mut self, note: impl Into<Notification>, cx: &mut App);
    fn remove_notification<T: Sized + 'static>(&mut self, cx: &mut App);
    fn remove_notification1<T: Sized + 'static>(&mut self, key: impl Into<ElementId>, cx: &mut App);
    fn has_notification<T: Sized + 'static>(&mut self, cx: &mut App) -> bool;
    fn clear_notifications(&mut self, cx: &mut App);
}

impl OverlayExt for Window {
    fn open_dialog<F>(&mut self, cx: &mut App, build: F)
    where
        F: Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static,
    {
        Overlays::update(self, cx, move |host, window, cx| {
            let focus_handle = cx.focus_handle();
            let previous_focus_handle = window.focused(cx).map(|focus| focus.downgrade());
            focus_handle.focus(window, cx);
            host.dialogs.push(ActiveDialog {
                focus_handle,
                previous_focus_handle,
                builder: Rc::new(build),
            });
            cx.notify();
        });
    }

    fn open_alert_dialog<F>(&mut self, cx: &mut App, build: F)
    where
        F: Fn(AlertDialog, &mut Window, &mut App) -> AlertDialog + 'static,
    {
        self.open_dialog(cx, move |_, window, cx| {
            build(AlertDialog::new(cx), window, cx).build_surface(window, cx)
        });
    }

    fn close_dialog(&mut self, cx: &mut App) {
        Overlays::update(self, cx, |host, window, cx| host.close_dialog(window, cx));
    }

    fn close_all_dialogs(&mut self, cx: &mut App) {
        Overlays::update(self, cx, |host, window, cx| {
            host.close_all_dialogs(window, cx)
        });
    }

    fn push_notification(&mut self, note: impl Into<Notification>, cx: &mut App) {
        let note = note.into();
        Overlays::update(self, cx, |host, window, cx| {
            host.notifications
                .update(cx, |list, cx| list.push(note, window, cx));
        });
    }

    fn remove_notification<T: Sized + 'static>(&mut self, cx: &mut App) {
        Overlays::update(self, cx, |host, window, cx| {
            host.notifications.update(cx, |list, cx| {
                list.close_by_type(std::any::TypeId::of::<T>(), window, cx)
            });
        });
    }

    fn remove_notification1<T: Sized + 'static>(
        &mut self,
        key: impl Into<ElementId>,
        cx: &mut App,
    ) {
        let key = key.into();
        Overlays::update(self, cx, |host, window, cx| {
            host.notifications.update(cx, |list, cx| {
                list.close((std::any::TypeId::of::<T>(), key), window, cx)
            });
        });
    }

    fn has_notification<T: Sized + 'static>(&mut self, cx: &mut App) -> bool {
        Overlays::update(self, cx, |host, _, cx| {
            host.notifications
                .read(cx)
                .contains_type(std::any::TypeId::of::<T>())
        })
    }

    fn clear_notifications(&mut self, cx: &mut App) {
        Overlays::update(self, cx, |host, window, cx| {
            host.notifications
                .update(cx, |list, cx| list.clear(window, cx));
        });
    }
}

type SheetClose = Rc<dyn Fn(&mut Window, &mut App)>;

/// A sheet's binding to the window's edge-back gesture, scoped by its base focus trap.
pub(crate) struct SheetBack {
    pub focus: gpui::FocusHandle,
    pub open: bool,
    pub dragging: bool,
    pub progress: f32,
    pub height: gpui::Pixels,
    pub close: SheetClose,
    pub parent: gpui::EntityId,
}

pub(crate) fn bind_sheet(
    id: ElementId,
    focus: gpui::FocusHandle,
    open: bool,
    close: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> Entity<SheetBack> {
    let parent = window.current_view();
    let state = window.use_keyed_state((id, "edge-back"), cx, |_, _| SheetBack {
        focus: focus.clone(),
        open,
        dragging: false,
        progress: 0.,
        height: gpui::px(0.),
        close: Rc::new(|_, _| {}),
        parent,
    });
    state.update(cx, |state, _| {
        state.focus = focus;
        if open && !state.open {
            state.progress = 0.;
            state.dragging = false;
        }
        state.open = open;
        state.close = Rc::new(close);
        state.parent = parent;
    });
    if let Some(overlays) = window
        .root::<gpui_base::Root>()
        .flatten()
        .and_then(|root| root.read(cx).plugin::<Overlays>())
    {
        overlays.update(cx, |overlays, _| {
            overlays.sheets.retain(|sheet| sheet.upgrade().is_some());
            if !overlays
                .sheets
                .iter()
                .any(|sheet| sheet.entity_id() == state.entity_id())
            {
                overlays.sheets.push(state.downgrade());
            }
        });
    }
    state
}

pub(crate) fn dialog_open(window: &Window, cx: &App) -> bool {
    window
        .root::<gpui_base::Root>()
        .flatten()
        .and_then(|root| root.read(cx).plugin::<Overlays>())
        .is_some_and(|overlays| !overlays.read(cx).dialogs.is_empty())
}

pub(crate) fn active_sheet(window: &Window, cx: &App) -> Option<Entity<SheetBack>> {
    let trap = gpui_base::active_focus_trap(window, cx)?;
    let overlays = window
        .root::<gpui_base::Root>()
        .flatten()?
        .read(cx)
        .plugin::<Overlays>()?;
    overlays
        .read(cx)
        .sheets
        .iter()
        .rev()
        .filter_map(|sheet| sheet.upgrade())
        .find(|sheet| {
            let sheet = sheet.read(cx);
            sheet.open && sheet.focus == trap
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, size};

    struct Body;

    impl Render for Body {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    fn overlays(root: &Entity<gpui_base::Root>, cx: &mut VisualTestContext) -> Entity<Overlays> {
        root.read_with(cx, |root, _| root.plugin::<Overlays>().unwrap())
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            // A frame that reuses any view keeps every debug bound of the
            // previous one, so a dismissed toast would still be found.
            window.refresh();
            _ = window.draw(cx);
        });
    }

    struct ProjectDialogHost {
        store: Entity<crate::store::WorkspaceStore>,
        link: tcode_client::HostLink,
        incoming: async_channel::Sender<String>,
        outgoing: async_channel::Receiver<String>,
    }

    impl ProjectDialogHost {
        fn new(cx: &mut TestAppContext) -> Self {
            cx.update(crate::theme::init);
            let (to_host, outgoing) = async_channel::unbounded();
            let (incoming, from_host) = async_channel::unbounded();
            let link = tcode_client::HostLink::new(to_host, from_host);
            let store = cx.new(|cx| {
                crate::store::WorkspaceStore::new_attached(
                    link.clone(),
                    crate::store::WorkspaceAttachment::Local,
                    None,
                    None,
                    false,
                    cx,
                )
            });
            crate::store::tests::seed_full_scope(&store, &incoming, vec![], cx);
            Self {
                store,
                link,
                incoming,
                outgoing,
            }
        }

        fn send(&self, message: tcode_protocol::HostMessage, cx: &mut TestAppContext) {
            use std::future::Future as _;
            self.incoming
                .try_send(tcode_protocol::encode_line(&message).unwrap())
                .unwrap();
            let mut pump = std::pin::pin!(self.link.pump());
            let mut task = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(pump.as_mut().poll(&mut task).is_pending());
            self.store
                .update(cx, |store, cx| store.drain_host_events_for_test(cx));
            cx.run_until_parked();
        }

        fn index(&self, cx: &mut TestAppContext) {
            self.send(
                tcode_protocol::HostMessage::Event(tcode_protocol::EventEnvelope {
                    request_id: None,
                    topic: tcode_protocol::Topic::Index,
                    event: tcode_protocol::ServerEvent::IndexSnapshot(
                        tcode_protocol::IndexSnapshot {
                            summary: Default::default(),
                            sessions: vec![],
                            projects: vec![tcode_core::project::Project {
                                id: "project".into(),
                                name: "Project".into(),
                                root: "/host/project".into(),
                                quick_chat: false,
                                icon_path: None,
                                permission_defaults: Default::default(),
                                created_at: 0,
                            }],
                        },
                    ),
                }),
                cx,
            );
        }

        fn commands(&self) -> Vec<(u64, tcode_protocol::Command)> {
            std::iter::from_fn(|| self.outgoing.try_recv().ok())
                .filter_map(|line| {
                    let message = tcode_protocol::decode_client_line(&line).unwrap();
                    match message.payload {
                        tcode_protocol::ClientPayload::Command(command) => {
                            Some((message.id, command))
                        }
                        _ => None,
                    }
                })
                .collect()
        }
    }

    fn project_dialog_enter(cx: &mut TestAppContext, kind: &str, invalid: bool) {
        use tcode_protocol::{Command, CommandResponse, HostMessage};
        let host = ProjectDialogHost::new(cx);
        if kind == "change" {
            host.index(cx);
        }
        let (root, cx) = cx.add_window_view(|window, cx| {
            let body = cx.new(|_| Body);
            gpui_base::Root::new(body, window, cx)
        });
        cx.update(|window, cx| {
            if kind == "change" {
                crate::change_project_root_dialog::open(
                    host.store.clone(),
                    "project".into(),
                    window,
                    cx,
                );
            } else {
                crate::add_project_dialog::open(host.store.clone(), window, cx);
            }
        });
        draw(cx);
        // Tab follows the rendered form's focus order, starting at the modal.
        cx.simulate_keystrokes("tab");
        if kind == "import" {
            cx.simulate_keystrokes("tab tab");
        }
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        });
        cx.simulate_input(if invalid {
            "   "
        } else if kind == "create" {
            "New project"
        } else {
            "/host/destination"
        });
        host.commands();
        cx.simulate_keystrokes("enter");
        draw(cx);
        assert_eq!(
            overlays(&root, cx).read_with(cx, |host, _| host.dialogs.len()),
            1,
            "Enter must leave the dialog open until the action succeeds"
        );
        let commands = host.commands();
        if invalid {
            assert!(
                commands.is_empty(),
                "an empty path must not send a command: {commands:?}"
            );
            return;
        }
        let expected = match kind {
            "import" => Command::CreateProject {
                root: "/host/destination".into(),
            },
            "create" => Command::CreateNewProject {
                name: "New project".into(),
            },
            "change" => Command::SetProjectRoot {
                project_id: "project".into(),
                root: "/host/destination".into(),
                move_files: false,
            },
            _ => unreachable!(),
        };
        assert_eq!(
            commands.len(),
            1,
            "Enter must send the primary action: {commands:?}"
        );
        assert_eq!(commands[0].1, expected);
        if kind == "change" {
            cx.simulate_keystrokes("enter");
            assert!(
                host.commands().is_empty(),
                "a busy action must not be sent twice"
            );
        }
        host.send(
            HostMessage::Ack {
                id: commands[0].0,
                result: Ok(if kind == "change" {
                    CommandResponse::Unit
                } else {
                    CommandResponse::ProjectId(Some("project".into()))
                }),
            },
            cx,
        );
        if kind == "create" {
            assert_eq!(
                overlays(&root, cx).read_with(cx, |host, _| host.dialogs.len()),
                1,
                "creation waits until the project appears in the host index"
            );
            host.index(cx);
        }
        draw(cx);
        assert_eq!(
            overlays(&root, cx).read_with(cx, |host, _| host.dialogs.len()),
            0,
            "a successful action closes the dialog"
        );
    }

    #[gpui::test]
    fn import_project_input_enter_runs_action(cx: &mut TestAppContext) {
        project_dialog_enter(cx, "import", false);
    }

    #[gpui::test]
    fn create_project_input_enter_runs_action(cx: &mut TestAppContext) {
        project_dialog_enter(cx, "create", false);
    }

    #[gpui::test]
    fn change_project_path_input_enter_runs_action(cx: &mut TestAppContext) {
        project_dialog_enter(cx, "change", false);
    }

    #[gpui::test]
    fn invalid_project_path_enter_keeps_dialog_open(cx: &mut TestAppContext) {
        for kind in ["import", "change"] {
            project_dialog_enter(cx, kind, true);
        }
    }

    #[gpui::test]
    fn compact_toast_obeys_seam_replaces_and_wide_keeps_card(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        let (root, cx) = cx.add_window_view(|window, cx| {
            let body = cx.new(|_| Body);
            gpui_base::Root::new(body, window, cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        crate::window_seam::occlude_for_test(
            cx,
            gpui::Edges {
                bottom: px(300.),
                ..Default::default()
            },
        );
        cx.update(|window, cx| {
            window.push_notification(Notification::success("Copied"), cx);
        });
        draw(cx);
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        draw(cx);
        let bounds = cx.debug_bounds("compact-toast").expect("compact pill");
        assert!((bounds.center().x - px(196.5)).abs() < px(1.));
        assert!(
            (bounds.bottom() - px(536.)).abs() < px(1.),
            "bounds: {bounds:?}"
        );
        assert!(bounds.size.width <= px(361.));
        assert!(bounds.size.height >= px(44.));
        cx.update(|window, cx| window.push_notification("Second", cx));
        overlays(&root, cx).read_with(cx, |root, cx| {
            root.notifications.read(cx).assert_messages(cx, &["Second"]);
        });
        cx.simulate_resize(size(px(1200.), px(800.)));
        crate::window_seam::occlude_for_test(cx, gpui::Edges::default());
        cx.update(|window, cx| {
            window.push_notification("Wide", cx);
        });
        draw(cx);
        assert!(cx.debug_bounds("compact-toast").is_none());
        let wide = cx.debug_bounds("wide-toast").expect("wide corner card");
        assert_eq!(wide.size.width, px(356.));
        let position = cx.debug_bounds("notification-position").unwrap();
        assert_eq!(position.top(), px(16.));
        assert_eq!(position.right(), px(1184.));
    }

    #[gpui::test]
    fn compact_notification_timeouts(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        let (_root, cx) = cx.add_window_view(|window, cx| {
            let body = cx.new(|_| Body);
            gpui_base::Root::new(body, window, cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.update(|window, cx| window.push_notification("Copied", cx));
        draw(cx);
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        draw(cx);
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        draw(cx);
        assert!(cx.debug_bounds("compact-toast").is_none());
        cx.update(|window, cx| {
            window.push_notification(
                Notification::info("Removed")
                    .action(|_, _, _| crate::widgets::Button::new("undo").label("Undo")),
                cx,
            )
        });
        draw(cx);
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        draw(cx);
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        draw(cx);
        assert!(cx.debug_bounds("compact-toast").is_some());
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(2));
        draw(cx);
        assert!(cx.debug_bounds("compact-toast").is_none());
    }
}
