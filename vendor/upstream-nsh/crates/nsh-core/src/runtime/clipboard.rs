//! `nsh/clipboard`: the system clipboard's text, image and files
//! (UIPasteboard, ClipboardManager, NSPasteboard, `navigator.clipboard`,
//! ...). Needs the `clipboard` capability when the app declares
//! capabilities: without it every function rejects with
//! `ERR_ACCESS_DENIED` (checked here, in the call; the core also refuses
//! the commands, for Rust components' calls, which then get nothing).

use crate::element::{ClipboardImage, JsError, System, module};

/// Whether the app may use the clipboard: why not when it may not.
fn granted(sys: &System) -> Result<(), JsError> {
    if sys.access.allows_clipboard() {
        return Ok(());
    }
    Err(JsError::access_denied(
        "the clipboard is not granted: add `\"clipboard\": true` to capabilities in nsh.json",
        "clipboard",
        "",
    ))
}

/// The system clipboard's text, image and files.
#[module]
pub(crate) struct Clipboard;

#[module]
impl Clipboard {
    /// Puts `text` on the clipboard, replacing what it holds.
    pub(crate) async fn write_text(text: String, sys: System) -> Result<(), JsError> {
        granted(&sys)?;
        crate::prelude::set_clipboard(text);
        Ok(())
    }

    /// The clipboard's text, `""` when it holds none. On iOS, reading it
    /// may show the system's paste notice.
    pub(crate) async fn read_text(sys: System) -> Result<String, JsError> {
        granted(&sys)?;
        Ok(crate::prelude::clipboard_string().await)
    }

    /// Whether the clipboard holds text, without reading it (no paste
    /// notice).
    pub(crate) async fn has_text(sys: System) -> Result<bool, JsError> {
        granted(&sys)?;
        Ok(crate::prelude::clipboard_has_string().await)
    }

    /// Puts the image of `src`, a `data:` URL (as `readImage` gives), on
    /// the clipboard, replacing what it holds.
    pub(crate) async fn write_image(src: String, sys: System) -> Result<(), JsError> {
        granted(&sys)?;
        crate::prelude::set_clipboard_image(src);
        Ok(())
    }

    /// The clipboard's image, `null` when it holds none: `url` is a
    /// `data:` URL an `Image` shows and `fetch(url)` reads the bytes of,
    /// `size` its size in pixels; `null` too for an image
    /// over 48 MB (encoded), which the app's memory may not hold. On iOS,
    /// reading it may show the system's paste notice.
    pub(crate) async fn read_image(sys: System) -> Result<Option<ClipboardImage>, JsError> {
        granted(&sys)?;
        Ok(crate::prelude::clipboard_image().await)
    }

    /// Whether the clipboard holds an image, without reading it (no paste
    /// notice).
    pub(crate) async fn has_image(sys: System) -> Result<bool, JsError> {
        granted(&sys)?;
        Ok(crate::prelude::clipboard_has_image().await)
    }

    /// The `file:` URLs of the local files on the clipboard (files copied in
    /// a file manager), `[]` when it holds none.
    pub(crate) async fn read_files(sys: System) -> Result<Vec<String>, JsError> {
        granted(&sys)?;
        Ok(crate::prelude::clipboard_files().await)
    }
}
