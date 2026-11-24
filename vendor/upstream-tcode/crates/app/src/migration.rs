//! Preparing the data dir — moving an older build's data dir into it, then
//! migrating older threads into `tcode.db` — before the local kernel starts,
//! behind a window that offers nothing but its progress and Quit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, Global, IntoElement,
    ParentElement as _, Render, Styled as _, Window, WindowOptions, div,
    prelude::FluentBuilder as _, px,
};
use tcode_services::store::{Migration, MigrationPhase, MigrationProgress, SessionStore};
use tcode_ui::overlay::{DialogActions, OverlayExt as _};
use tcode_ui::theme::{self, ActiveTheme as _, ThemeMode};
use tcode_ui::widgets::{Button, Progress};

/// What the dialog shows.
enum Status {
    Running(Option<MigrationProgress>),
    /// Quit was asked for: the migration stops at its next chunk, and the
    /// process exits once it has.
    Quitting,
    Failed(String),
}

struct MigrationView {
    status: Status,
    cancel: Arc<AtomicBool>,
    data_dir: String,
}

/// The view a Quit reaches while the migration window is up.
struct Running(Entity<MigrationView>);
impl Global for Running {}

/// Behind the dialog; the window shows nothing else.
struct Backdrop;

impl Render for Backdrop {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full()
    }
}

enum Update {
    Progress(MigrationProgress),
    Done(std::io::Result<Migration>),
}

/// Migrate `store` behind a modal progress dialog, then hand over to `start`
/// and close the window. Quit, ⌘Q or closing the window cancels the
/// migration and exits once it has stopped; a failure, of the migration or of
/// `start`, stays in the dialog until Quit, which then exits non-zero.
pub(crate) fn run(
    cx: &mut App,
    store: SessionStore,
    window_options: WindowOptions,
    appearance: Option<String>,
    start: impl FnOnce(&mut App) -> std::io::Result<()> + 'static,
) {
    let cancel = Arc::new(AtomicBool::new(false));
    let view = cx.new(|_| MigrationView {
        status: Status::Running(None),
        cancel: cancel.clone(),
        data_dir: store.root().display().to_string(),
    });
    cx.set_global(Running(view.clone()));

    let (updates, received) = async_channel::unbounded();
    let (finished, stopped) = async_channel::bounded::<()>(1);
    let spawned = std::thread::Builder::new()
        .name("tcode-migration".into())
        .spawn({
            let cancel = cancel.clone();
            move || {
                let progress = updates.clone();
                let result = store.migrate(
                    |value| {
                        let _ = progress.try_send(Update::Progress(value));
                    },
                    &cancel,
                );
                let _ = updates.try_send(Update::Done(result));
                drop(finished);
            }
        });
    // A quit the dialog does not see first (the Dock menu, logging out)
    // still stops the migration; gpui waits for it only briefly, and a
    // migration it outlasts is discarded by the next start.
    cx.on_app_quit({
        let cancel = cancel.clone();
        move |_| {
            cancel.store(true, Ordering::Relaxed);
            let stopped = stopped.clone();
            async move {
                let _ = stopped.recv().await;
            }
        }
    })
    .detach();

    let window = tcode_ui::open_client_window(
        cx,
        window_options,
        tcode_ui::tr!("app.name").into(),
        |_, cx| cx.new(|_| Backdrop),
    );
    cx.activate(true);
    let _ = window.update(cx, |_, window, cx| {
        // The host's theme setting is unknown until its kernel starts; this
        // client's own choice is not.
        match appearance.as_deref() {
            Some("light") => theme::change_mode(ThemeMode::Light, Some(window), cx),
            Some("dark") => theme::change_mode(ThemeMode::Dark, Some(window), cx),
            _ => {}
        }
        window.on_window_should_close(cx, |_, cx| {
            quit(cx);
            false
        });
        let content = view.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let content = content.clone();
            dialog
                .title(tcode_ui::tr!("migration.title").into_owned())
                .close_button(false)
                .overlay_closable(false)
                .keyboard(false)
                .content(move |body, _, _| body.child(content.clone()))
                .footer(
                    DialogActions::new().child(
                        Button::new("migration-quit")
                            .label(tcode_ui::tr!("quit.confirm"))
                            .on_click(|_, _, cx| {
                                quit(cx);
                            }),
                    ),
                )
        });
        window.activate_window();
    });

    if let Err(error) = spawned {
        fail(&view, error.to_string(), cx);
        return;
    }
    cx.spawn(async move |cx| {
        while let Ok(mut update) = received.recv().await {
            // Only the latest progress is drawn; `Done` is always the last.
            while let Ok(next) = received.try_recv() {
                update = next;
            }
            match update {
                Update::Progress(progress) => view.update(cx, |view, cx| {
                    if let Status::Running(shown) = &mut view.status {
                        *shown = Some(progress);
                        cx.notify();
                    }
                }),
                Update::Done(result) => {
                    cx.update(|cx| finish(result, &view, window, start, cx));
                    return;
                }
            }
        }
    })
    .detach();
}

fn finish(
    result: std::io::Result<Migration>,
    view: &Entity<MigrationView>,
    window: AnyWindowHandle,
    start: impl FnOnce(&mut App) -> std::io::Result<()>,
    cx: &mut App,
) {
    let quitting = matches!(view.read(cx).status, Status::Quitting);
    match result {
        Err(error) => {
            log::error!("migration failed: {error}");
            fail(view, error.to_string(), cx);
            if quitting {
                cx.quit();
            }
        }
        Ok(Migration::Cancelled) => cx.quit(),
        Ok(Migration::Completed) if quitting => cx.quit(),
        Ok(Migration::Completed) => {
            cx.remove_global::<Running>();
            match start(cx) {
                Ok(()) => {
                    let _ = window.update(cx, |_, window, _| window.remove_window());
                }
                Err(error) => {
                    log::error!("the local host did not start after the migration: {error}");
                    cx.set_global(Running(view.clone()));
                    fail(view, error.to_string(), cx);
                }
            }
        }
    }
}

/// Show `reason` in the dialog; the process exits non-zero once it quits.
fn fail(view: &Entity<MigrationView>, reason: String, cx: &mut App) {
    // gpui ends a quit with status 0; this observer is the last code that
    // runs before it.
    cx.on_app_quit(|_| async { std::process::exit(1) }).detach();
    view.update(cx, |view, cx| {
        view.status = Status::Failed(reason);
        cx.notify();
    });
}

/// Quit while the migration window is up. Returns whether it was.
pub(crate) fn quit(cx: &mut App) -> bool {
    let Some(Running(view)) = cx.try_global::<Running>() else {
        return false;
    };
    let view = view.clone();
    view.update(cx, |view, cx| match view.status {
        Status::Running(_) => {
            view.cancel.store(true, Ordering::Relaxed);
            view.status = Status::Quitting;
            cx.notify();
        }
        Status::Quitting => {}
        Status::Failed(_) => cx.quit(),
    });
    true
}

impl Render for MigrationView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (percent, line, failed) = match &self.status {
            Status::Running(progress) => (
                progress.as_ref().map_or(0., overall_percent),
                progress.as_ref().map_or_else(
                    || phase_line(MigrationPhase::Scanning, None, &self.data_dir),
                    |progress| phase_line(progress.phase, Some(progress), &self.data_dir),
                ),
                false,
            ),
            Status::Quitting => (0., tcode_ui::tr!("migration.quitting").into_owned(), false),
            Status::Failed(reason) => (
                0.,
                tcode_ui::tr!("migration.failed", reason = reason.clone()).into_owned(),
                true,
            ),
        };
        let quitting = matches!(self.status, Status::Quitting);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .text_size(px(13.))
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(tcode_ui::tr!(
                        "migration.description",
                        path = self.data_dir.clone()
                    )),
            )
            .when(!failed, |column| {
                column.child(
                    Progress::new("migration-progress")
                        .value(percent)
                        .loading(quitting),
                )
            })
            .child(
                div()
                    .text_color(if failed {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .child(line),
            )
    }
}

/// A move by rename counts entries; a copy across filesystems counts bytes,
/// and gets a bar of its own before the threads migrate. Import and
/// verification each go through every log once, but import writes and syncs
/// each chunk while verification only reads it back, which takes a fraction
/// of the time. The phases around them take a moment.
fn overall_percent(progress: &MigrationProgress) -> f32 {
    let fraction = |done: u64, total: u64| {
        if total == 0 {
            1.
        } else {
            (done as f32 / total as f32).min(1.)
        }
    };
    let phase = fraction(progress.bytes_done, progress.bytes_total);
    match progress.phase {
        MigrationPhase::Relocating if progress.bytes_total == 0 => {
            fraction(progress.threads_done as u64, progress.threads_total as u64) * 100.
        }
        MigrationPhase::Relocating => phase * 100.,
        MigrationPhase::Scanning => 0.,
        MigrationPhase::Importing => phase * 85.,
        MigrationPhase::Verifying => 85. + phase * 15.,
        MigrationPhase::Publishing | MigrationPhase::Archiving => 100.,
    }
}

fn phase_line(
    phase: MigrationPhase,
    progress: Option<&MigrationProgress>,
    data_dir: &str,
) -> String {
    let counts = |key: &str| {
        let progress = progress.copied().unwrap_or(MigrationProgress {
            phase,
            threads_done: 0,
            threads_total: 0,
            bytes_done: 0,
            bytes_total: 0,
        });
        tcode_ui::tr!(
            key,
            done = progress.threads_done,
            total = progress.threads_total,
            bytes = tcode_ui::format_size(progress.bytes_done as usize),
            total_bytes = tcode_ui::format_size(progress.bytes_total as usize),
            path = data_dir
        )
        .into_owned()
    };
    match phase {
        MigrationPhase::Relocating if progress.is_some_and(|progress| progress.bytes_total > 0) => {
            counts("migration.relocating_copy")
        }
        MigrationPhase::Relocating => {
            tcode_ui::tr!("migration.relocating", path = data_dir).into_owned()
        }
        MigrationPhase::Scanning => tcode_ui::tr!("migration.scanning").into_owned(),
        MigrationPhase::Importing => counts("migration.importing"),
        MigrationPhase::Verifying => counts("migration.verifying"),
        MigrationPhase::Publishing => tcode_ui::tr!("migration.publishing").into_owned(),
        MigrationPhase::Archiving => tcode_ui::tr!("migration.archiving").into_owned(),
    }
}
