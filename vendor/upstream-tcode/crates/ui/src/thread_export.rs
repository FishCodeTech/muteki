//! Exporting a thread.
//!
//! Rendering belongs to the host: it owns the event log and the store-flush
//! barrier, and it answers with complete bytes. Delivery belongs to the client,
//! because "where does this file go" is a question about the machine the user is
//! sitting at — which, over a remote link, is not the host. A desktop client
//! saves through the platform panel, a browser downloads a Blob, and every
//! client can copy the text. Nothing is written on the host.

use std::path::PathBuf;

use gpui::{App, ClipboardItem, Context, Entity, Window};
use tcode_protocol::ThreadExportFormat;

use crate::overlay::{DialogActions, Notification, OverlayExt as _};
use crate::store::{ThreadExportArtifact, WorkspaceStore};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use gpui::{IntoElement, ParentElement as _, Styled as _, div, prelude::FluentBuilder as _};
use gpui_base::v_flex;

pub(crate) fn prompt_thread_export<V: 'static>(
    store: Entity<WorkspaceStore>,
    session_id: String,
    directory: PathBuf,
    format: ThreadExportFormat,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    // The host's cwd means nothing on this machine, so a remote export starts
    // the save panel where the user actually is.
    let start_directory = if store.read(cx).is_remote() {
        std::env::current_dir().unwrap_or_default()
    } else {
        directory
    };
    let request = store.update(cx, |store, cx| {
        store.render_thread_export(session_id, format, cx)
    });
    cx.spawn_in(window, async move |this, cx| {
        let result = request.await;
        let _ = this.update_in(cx, |_view, window, cx| match result {
            Ok(artifact) => open_delivery_dialog(store, artifact, start_directory, window, cx),
            Err(error) => window.push_notification(
                Notification::error(crate::tr!("errors.export_thread", error = error)),
                cx,
            ),
        });
    })
    .detach();
}

/// Offer every delivery this client can actually perform. A client with no save
/// panel and no platform download shows Copy alone rather than a button that
/// would report a success that never happened.
fn open_delivery_dialog(
    store: Entity<WorkspaceStore>,
    artifact: ThreadExportArtifact,
    directory: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    let name = artifact.suggested_name.clone();
    let summary = crate::tr!(
        "export.ready",
        name = name.clone(),
        size = format_size(artifact.bytes.len())
    )
    .into_owned();
    let can_download = store.read(cx).supports_artifact_delivery();
    window.open_dialog(cx, move |builder, _, _| {
        let summary = summary.clone();
        let copy_artifact = artifact.clone();
        let download_artifact = artifact.clone();
        let save_artifact = artifact.clone();
        let download_store = store.clone();
        let directory = directory.clone();
        builder
            .title(crate::tr!("export.title").into_owned())
            .content(move |content, _, cx| {
                content.child(
                    v_flex().gap_1().child(
                        div()
                            .text_body(false, cx)
                            .text_color(cx.theme().muted_foreground)
                            .child(summary.clone()),
                    ),
                )
            })
            .footer(
                DialogActions::new()
                    .child(
                        Button::new("export-cancel")
                            .label(crate::tr!("export.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("export-copy")
                            .label(crate::tr!("export.copy"))
                            .on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    String::from_utf8_lossy(&copy_artifact.bytes).into_owned(),
                                ));
                                window.close_dialog(cx);
                                window.push_notification(
                                    Notification::success(crate::tr!("export.copied")),
                                    cx,
                                );
                            }),
                    )
                    .when(can_download, |actions| {
                        let artifact = download_artifact.clone();
                        let store = download_store.clone();
                        actions.child(
                            Button::new("export-download")
                                .label(crate::tr!("export.download"))
                                .on_click(move |_, window, cx| {
                                    let result = store.read(cx).deliver_artifact(
                                        &artifact.suggested_name,
                                        &artifact.mime,
                                        &artifact.bytes,
                                    );
                                    window.close_dialog(cx);
                                    match result {
                                        Ok(()) => window.push_notification(
                                            Notification::success(crate::tr!(
                                                "export.downloaded",
                                                name = artifact.suggested_name.clone()
                                            )),
                                            cx,
                                        ),
                                        Err(error) => window.push_notification(
                                            Notification::error(crate::tr!(
                                                "errors.export_thread",
                                                error = error
                                            )),
                                            cx,
                                        ),
                                    }
                                }),
                        )
                    })
                    .when(NATIVE_SAVE_PANEL, |actions| {
                        let artifact = save_artifact.clone();
                        let directory = directory.clone();
                        actions.child(
                            Button::new("export-save")
                                .primary()
                                .label(crate::tr!("export.save"))
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    save_to_disk(artifact.clone(), directory.clone(), window, cx);
                                }),
                        )
                    })
                    .into_any_element(),
            )
    });
}

/// Whether this build has a platform save panel.
const NATIVE_SAVE_PANEL: bool = cfg!(feature = "native-dialogs");

#[cfg(feature = "native-dialogs")]
fn save_to_disk(
    artifact: ThreadExportArtifact,
    directory: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    // The platform save panel owns destination selection and overwrite
    // confirmation; nothing is written before it returns `Some`. A dismissed
    // panel is a decision, not a failure, so it reports nothing.
    let receiver = cx.prompt_for_new_path(&directory, Some(&artifact.suggested_name));
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let Ok(result) = receiver.await else {
            return;
        };
        let outcome = match result {
            Ok(Some(destination)) => match std::fs::write(&destination, &artifact.bytes) {
                Ok(()) => Some(Ok(destination)),
                Err(error) => Some(Err(error.to_string())),
            },
            Ok(None) => None,
            Err(error) => Some(Err(error.to_string())),
        };
        let Some(outcome) = outcome else {
            return;
        };
        let _ = handle.update(cx, |_, window, cx| match outcome {
            Ok(destination) => window.push_notification(
                Notification::success(crate::tr!(
                    "export.saved",
                    name = destination
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| destination.display().to_string())
                )),
                cx,
            ),
            Err(error) => window.push_notification(
                Notification::error(crate::tr!("errors.export_thread", error = error)),
                cx,
            ),
        });
    })
    .detach();
}

/// Unreachable: the Save action is only rendered when `NATIVE_SAVE_PANEL`.
#[cfg(not(feature = "native-dialogs"))]
fn save_to_disk(
    _artifact: ThreadExportArtifact,
    _directory: PathBuf,
    _window: &mut Window,
    _cx: &mut App,
) {
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024. * 1024.))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024. * 1024. * 1024.))
    }
}
