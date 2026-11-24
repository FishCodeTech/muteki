use super::super::*;
use crate::attachments::{TransferVerdict, transfer_verdict};
use crate::thread_export::format_size;
use crate::widgets::menu::ContextMenuExt as _;
use gpui::StyledImage as _;
use image::GenericImageView as _;
use std::cell::RefCell;
use tcode_core::attachments::{AttachError, MAX_EDGE_PX, MAX_IMAGES, MAX_SOURCE_BYTES};

#[derive(Clone)]
/// A pending image attachment: validated, persisted to the session attachments
/// dir, and shown in the composer thumbnail strip. Kept per active session.
pub(in super::super) struct PendingImage {
    /// On-disk path of the persisted copy (also the thumbnail image source).
    pub(in super::super) path: PathBuf,
    pub(in super::super) name: String,
}

pub(in super::super) enum PendingImageSource {
    Bytes(Vec<u8>),
    Path(PathBuf),
}

pub(in super::super) enum AddImageError {
    Attachment(AttachError),
    Persist(std::io::Error),
}

/// Fitted bytes waiting for the host to store them.
struct PreparedImage {
    dir: PathBuf,
    bytes: Vec<u8>,
    ext: String,
}

/// Guess a file extension for a persisted attachment from its MIME type,
/// falling back to the source name's extension, then `png`.
fn image_extension(mime: &str, name: &str) -> String {
    let from_mime = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "",
    };
    if !from_mime.is_empty() {
        return from_mime.to_string();
    }
    std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_else(|| "png".to_string())
}

fn is_wire_ready_image(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// Bring an image to what the wire and the model take: a format every
/// provider accepts, no longer than [`MAX_EDGE_PX`] on either side. Bytes
/// that already qualify pass through untouched so nothing is recompressed;
/// GIFs always do, since re-encoding keeps only their first frame.
pub(crate) fn fit_for_upload(mime: &str, bytes: Vec<u8>) -> image::ImageResult<(String, Vec<u8>)> {
    if mime == "image/gif" {
        return Ok((mime.to_string(), bytes));
    }
    let decoded = image::load_from_memory(&bytes)?;
    let (width, height) = decoded.dimensions();
    let oversized = width.max(height) > MAX_EDGE_PX;
    if is_wire_ready_image(mime) && !oversized {
        return Ok((mime.to_string(), bytes));
    }
    let image = if oversized {
        decoded.resize(
            MAX_EDGE_PX,
            MAX_EDGE_PX,
            image::imageops::FilterType::Triangle,
        )
    } else {
        decoded
    };
    let mut out = std::io::Cursor::new(Vec::new());
    // Photos stay JPEG; anything else becomes PNG, which keeps transparency.
    if mime == "image/jpeg" {
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
        image.to_rgb8().write_with_encoder(encoder)?;
        Ok(("image/jpeg".to_string(), out.into_inner()))
    } else {
        image.write_to(&mut out, image::ImageFormat::Png)?;
        Ok(("image/png".to_string(), out.into_inner()))
    }
}

impl Composer {
    /// Reset the thumbnail strip when the active session changes (its pending
    /// images belong to a specific session).
    pub(in super::super) fn sync_images_session(&mut self, cx: &mut Context<Self>) {
        let id = self.workspace_store.read(cx).active_session_id();
        if id != self.images_session {
            self.images_session = id;
            self.pending_images.clear();
            self.image_load_generation = self.image_load_generation.wrapping_add(1);
            self.pending_image_loads = 0;
        }
    }

    /// Validate, fit, and persist an image off the main thread. Completion
    /// appends only while the same session still owns the strip.
    pub(in super::super) fn add_image(
        &mut self,
        name: String,
        mime: String,
        source: PendingImageSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.sync_images_session(cx);
        let current_count = self.pending_images.len() + self.pending_image_loads;
        // Type and count are known now; the size is checked once the image is
        // fitted, since a phone photo starts far larger than it is sent.
        if let Err(err) = validate_attachment(&name, &mime, 0, current_count) {
            window.push_notification(Notification::error(attach_error_message(&err)), cx);
            return false;
        }
        let store = self.workspace_store.read(cx);
        let session_id = store.active_session_id();
        let attachments_dir = store.composer_state().attachments_dir;
        let link = store.attachment_link();
        let remote_limit = store.client_remote_attachment_limit_bytes();
        self.pending_image_loads += 1;
        let generation = self.image_load_generation;
        let result_name = name.clone();
        cx.spawn_in(window, async move |this, cx| {
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    let too_large =
                        || AddImageError::Attachment(AttachError::TooLarge { name: name.clone() });
                    let bytes = match source {
                        PendingImageSource::Bytes(bytes) => bytes,
                        PendingImageSource::Path(path) => {
                            let size = std::fs::metadata(&path)
                                .map_err(AddImageError::Persist)?
                                .len();
                            if size > MAX_SOURCE_BYTES {
                                return Err(too_large());
                            }
                            std::fs::read(path).map_err(AddImageError::Persist)?
                        }
                    };
                    if bytes.len() as u64 > MAX_SOURCE_BYTES {
                        return Err(too_large());
                    }
                    let (mime, bytes) = fit_for_upload(&mime, bytes).map_err(|_| {
                        AddImageError::Attachment(AttachError::UnsupportedType {
                            name: name.clone(),
                        })
                    })?;
                    validate_attachment(&name, &mime, bytes.len() as u64, current_count)
                        .map_err(AddImageError::Attachment)?;
                    let dir = attachments_dir.ok_or_else(|| {
                        AddImageError::Persist(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "no active session",
                        ))
                    })?;
                    let ext = image_extension(&mime, &name);
                    Ok::<_, AddImageError>(PreparedImage { dir, bytes, ext })
                })
                .await;
            let _ = this.update_in(cx, |composer, window, cx| {
                if !composer.owns_image_load(generation, &session_id, cx) {
                    return;
                }
                let prepared = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        composer.pending_image_loads =
                            composer.pending_image_loads.saturating_sub(1);
                        composer.notify_add_image_error(error, window, cx);
                        return;
                    }
                };
                let size = prepared.bytes.len() as u64;
                match transfer_verdict(link, size, remote_limit) {
                    TransferVerdict::Send => composer.persist_image(
                        session_id,
                        generation,
                        result_name,
                        prepared,
                        window,
                        cx,
                    ),
                    TransferVerdict::Reject => {
                        composer.pending_image_loads =
                            composer.pending_image_loads.saturating_sub(1);
                        window.push_notification(
                            Notification::error(
                                crate::tr!(
                                    "attach.relay_rejected",
                                    name = result_name,
                                    size = format_size(size as usize),
                                    limit = format_size(remote_limit as usize)
                                )
                                .into_owned(),
                            ),
                            cx,
                        );
                    }
                    TransferVerdict::Confirm => {
                        let composer_entity = cx.entity();
                        // Taken by whichever button fires; a dialog closed
                        // by Escape or the backdrop releases the slot too.
                        let prepared = Rc::new(RefCell::new(Some(prepared)));
                        let session_id = session_id.clone();
                        window.open_alert_dialog(cx, move |alert, _, _| {
                            let composer = composer_entity.clone();
                            let release_entity = composer_entity.clone();
                            let prepared = prepared.clone();
                            let dropped = prepared.clone();
                            let session_id = session_id.clone();
                            let result_name = result_name.clone();
                            let release = move |cx: &mut App| {
                                if dropped.borrow_mut().take().is_some() {
                                    release_entity.update(cx, |composer, _cx| {
                                        composer.pending_image_loads =
                                            composer.pending_image_loads.saturating_sub(1);
                                    });
                                }
                            };
                            let on_cancel = release.clone();
                            alert
                                .title(crate::tr!("attach.tunnel_title"))
                                .description(crate::tr!(
                                    "attach.tunnel_body",
                                    name = result_name,
                                    size = format_size(size as usize)
                                ))
                                .button_props(
                                    DialogButtons::default()
                                        .ok_text(crate::tr!("attach.tunnel_confirm"))
                                        .cancel_text(crate::tr!("mobile.cancel"))
                                        .show_cancel(true),
                                )
                                .on_ok(move |_, window, cx| {
                                    if let Some(prepared) = prepared.borrow_mut().take() {
                                        composer.update(cx, |composer, cx| {
                                            composer.persist_image(
                                                session_id.clone(),
                                                generation,
                                                result_name.clone(),
                                                prepared,
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                    true
                                })
                                .on_cancel(move |_, _, cx| {
                                    on_cancel(cx);
                                    true
                                })
                                .on_close(move |_, _, cx| release(cx))
                        });
                    }
                }
            });
        })
        .detach();
        true
    }

    /// Whether a load started under `generation` for `session_id` still
    /// belongs to the strip on screen.
    fn owns_image_load(&self, generation: u64, session_id: &Option<String>, cx: &App) -> bool {
        self.image_load_generation == generation
            && &self.images_session == session_id
            && &self.workspace_store.read(cx).active_session_id() == session_id
    }

    fn notify_add_image_error(
        &self,
        error: AddImageError,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message = match error {
            AddImageError::Attachment(err) => attach_error_message(&err),
            AddImageError::Persist(err) => {
                crate::tr!("errors.persist_event", error = err).into_owned()
            }
        };
        window.push_notification(Notification::error(message), cx);
    }

    /// Send fitted bytes to the host and, once saved, show the thumbnail.
    fn persist_image(
        &mut self,
        session_id: Option<String>,
        generation: u64,
        name: String,
        prepared: PreparedImage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let PreparedImage { dir, bytes, ext } = prepared;
        let task = self.workspace_store.update(cx, |store, cx| {
            store.save_attachment_to_dir(dir, bytes, ext, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |composer, window, cx| {
                if !composer.owns_image_load(generation, &session_id, cx) {
                    return;
                }
                composer.pending_image_loads = composer.pending_image_loads.saturating_sub(1);
                match result {
                    Ok(path) => {
                        composer.pending_images.push(PendingImage { path, name });
                        cx.notify();
                    }
                    Err(err) => {
                        composer.notify_add_image_error(AddImageError::Persist(err), window, cx)
                    }
                }
            });
        })
        .detach();
    }

    /// Open the platform's media picker (phones) and add what it returns.
    pub(in super::super) fn pick_images_from_library(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = cx
            .try_global::<crate::remote::ClientAttachment>()
            .map(crate::remote::ClientAttachment::host)
        else {
            return;
        };
        self.sync_images_session(cx);
        let remaining =
            MAX_IMAGES.saturating_sub(self.pending_images.len() + self.pending_image_loads);
        if remaining == 0 {
            window.push_notification(
                Notification::error(attach_error_message(&AttachError::TooMany)),
                cx,
            );
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let picked = host.pick_images(remaining).await;
            let _ = this.update_in(cx, |composer, window, cx| match picked {
                Ok(images) => {
                    for image in images {
                        composer.add_image_bytes(image.name, image.mime, image.bytes, window, cx);
                    }
                }
                Err(error) => window.push_notification(
                    Notification::error(
                        crate::tr!("attach.pick_failed", error = error).into_owned(),
                    ),
                    cx,
                ),
            });
        })
        .detach();
    }

    pub(in super::super) fn add_image_bytes(
        &mut self,
        name: String,
        mime: String,
        bytes: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.add_image(name, mime, PendingImageSource::Bytes(bytes), window, cx)
    }

    /// Add an image from a dropped file path.
    pub(in super::super) fn add_image_path(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".to_string());
        let mime = mime_from_path(&path);
        self.add_image(name, mime, PendingImageSource::Path(path), window, cx)
    }

    /// Pull an image off the clipboard (⌘V with image content), if present.
    /// Returns whether an image was accepted.
    pub(in super::super) fn paste_clipboard_image(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut accepted_image = cx
            .read_from_clipboard()
            .is_some_and(|item| self.paste_images(&item, window, cx));
        if !accepted_image && let Some((mime, bytes)) = crate::pasteboard::read_pasteboard_image() {
            accepted_image =
                self.add_image_bytes("pasted-image".to_string(), mime, bytes, window, cx);
        }
        accepted_image
    }

    /// Attach the images of a pasted clipboard item. Returns whether an image
    /// was accepted.
    pub(in super::super) fn paste_images(
        &mut self,
        item: &gpui::ClipboardItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut accepted_image = false;
        for entry in &item.entries {
            match entry {
                ClipboardEntry::Image(image) => {
                    let mime = image.format().mime_type().to_string();
                    let bytes = image.bytes().to_vec();
                    accepted_image |=
                        self.add_image_bytes("pasted-image".to_string(), mime, bytes, window, cx);
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    for path in paths.paths() {
                        if mime_from_path(path).starts_with("image/") {
                            accepted_image |= self.add_image_path(path.clone(), window, cx);
                        }
                    }
                }
                ClipboardEntry::String(_) => {}
            }
        }
        accepted_image
    }

    pub(in super::super) fn remove_image(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.pending_images.len() {
            let removed = self.pending_images.remove(index);
            self.workspace_store
                .update(cx, |store, cx| store.remove_user_file(removed.path, cx))
                .detach();
            cx.notify();
        }
    }

    pub(in super::super) fn render_image_strip(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.pending_images.is_empty() {
            return None;
        }
        let mut row = h_flex().w_full().gap_1().flex_wrap();
        for (index, image) in self.pending_images.iter().enumerate() {
            let path = image.path.clone();
            let menu_path = path.to_string_lossy().into_owned();
            let name = image.name.clone();
            row = row.child(
                Button::new(("thumb", index))
                    .secondary()
                    .small()
                    .chip()
                    .aria_label(name.clone())
                    // A long file name truncates rather than taking the strip.
                    .max_w(px(220.))
                    .overflow_hidden()
                    // The thumbnail is decoded at the size it draws, inside the
                    // chip's height; no owner sizes a chip's leading image.
                    .child(
                        img(crate::store::image_preview(path, 18., false))
                            .id("attachment-image")
                            .size(px(18.))
                            .rounded(crate::material::radius_chip(cx))
                            .with_fallback(crate::image_viewer::unpreviewed(false, false, cx)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_meta_mono(self.compact, cx)
                            .child(crate::widgets::fade_text(name)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_image_preview(index, window, cx);
                    }))
                    .child(
                        Button::new(("thumb-x", index))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .aria_label(crate::tr!("composer.remove_image"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.remove_image(index, cx);
                            })),
                    )
                    .context_menu(move |menu, _, _| {
                        menu.menu(
                            crate::tr!("markdown.open_image").into_owned(),
                            Box::new(ComposerMenu::ImageOpen(index)),
                        )
                        .separator()
                        .path_items(&menu_path, None)
                        .separator()
                        .menu(
                            crate::tr!("composer.remove_image").into_owned(),
                            Box::new(ComposerMenu::ImageRemove(index)),
                        )
                    }),
            );
        }
        Some(row.into_any_element())
    }

    /// Open the clicked thumbnail as a window-level lightbox (see
    /// [`crate::image_viewer::open`]).
    pub(in super::super) fn open_image_preview(
        &self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(image) = self.pending_images.get(index) else {
            return;
        };
        crate::image_viewer::open(
            crate::image_viewer::ViewerSource::original(
                tcode_protocol::OriginalTarget::File {
                    path: image.path.clone(),
                },
                crate::store::image_preview(image.path.clone(), 18., false),
            ),
            image.name.clone(),
            window,
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(image: image::DynamicImage, format: image::ImageFormat) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    #[test]
    fn fitting_bounds_the_long_edge_and_keeps_qualifying_bytes() {
        let wide = image::DynamicImage::new_rgb8(MAX_EDGE_PX * 2, 100);
        let (mime, bytes) =
            fit_for_upload("image/png", encoded(wide, image::ImageFormat::Png)).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(
            image::load_from_memory(&bytes).unwrap().dimensions(),
            (MAX_EDGE_PX, 50)
        );

        let photo = encoded(
            image::DynamicImage::new_rgb8(MAX_EDGE_PX + 1, MAX_EDGE_PX + 1),
            image::ImageFormat::Jpeg,
        );
        let (mime, bytes) = fit_for_upload("image/jpeg", photo).unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(
            image::load_from_memory(&bytes).unwrap().dimensions(),
            (MAX_EDGE_PX, MAX_EDGE_PX)
        );

        let small = encoded(
            image::DynamicImage::new_rgba8(10, 10),
            image::ImageFormat::Png,
        );
        assert_eq!(
            fit_for_upload("image/png", small.clone()).unwrap(),
            ("image/png".to_string(), small)
        );

        let bmp = encoded(
            image::DynamicImage::new_rgb8(10, 10),
            image::ImageFormat::Bmp,
        );
        let (mime, bytes) = fit_for_upload("image/bmp", bmp).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(
            image::guess_format(&bytes).unwrap(),
            image::ImageFormat::Png
        );

        let gif = encoded(
            image::DynamicImage::new_rgba8(MAX_EDGE_PX * 2, 10),
            image::ImageFormat::Gif,
        );
        assert_eq!(
            fit_for_upload("image/gif", gif.clone()).unwrap(),
            ("image/gif".to_string(), gif)
        );

        assert!(fit_for_upload("image/png", b"not an image".to_vec()).is_err());
    }
}
