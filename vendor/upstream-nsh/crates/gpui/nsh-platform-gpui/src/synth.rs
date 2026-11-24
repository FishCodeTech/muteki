//! Input as a user's hands give it, made in process (GPUI's main thread):
//! pointers through the handlers real input goes through
//! ([`crate::input`]), keys and text ([`crate::text_input`]), and pictures
//! of the window. The agent of `nsh test --target gpui` acts with them
//! ([`crate::agent`]).

use std::{cell::Cell, rc::Rc, time::Duration};

use gpui::{App, AsyncApp, Window, WindowHandle};

use crate::{
    app::AppView,
    input::{KeyEvent, Modifiers},
    surface::Surface,
};

/// Runs `f` with the window's surface on the main thread, then redraws
/// what changed. `None` when the window is gone.
pub(crate) fn with_surface<R>(
    window: WindowHandle<AppView>,
    cx: &mut AsyncApp,
    f: impl FnOnce(&Rc<Surface>, &mut Window, &mut App) -> R,
) -> Option<R> {
    window
        .update(cx, |view, window, cx| {
            let surface = view.surface().clone();
            let r = f(&surface, window, cx);
            crate::rt::flush(cx);
            r
        })
        .ok()
}

pub(crate) async fn sleep(cx: &mut AsyncApp, ms: u64) {
    cx.background_executor()
        .timer(Duration::from_millis(ms))
        .await;
}

/// Waits until the window drew a frame with everything changed so far
/// (false when the window closed). A window that draws no frames (hidden,
/// minimized) is given up on after a second.
pub(crate) async fn drawn(window: WindowHandle<AppView>, cx: &mut AsyncApp) -> bool {
    // Next-frame callbacks run before their frame is drawn: the second
    // one runs once the frame the first one asked for is drawn.
    let done = Rc::new(Cell::new(false));
    let flag = done.clone();
    let Some(()) = with_surface(window, cx, move |_, window, _| {
        window.refresh();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |_, _| flag.set(true));
        });
    }) else {
        return false;
    };
    let start = std::time::Instant::now();
    while !done.get() {
        if start.elapsed() > Duration::from_secs(1) {
            log::warn!("test: the window draws no frames");
            break;
        }
        sleep(cx, 4).await;
    }
    true
}

pub(crate) fn press(surface: &Rc<Surface>, vk: u16, modifiers: Modifiers) {
    for down in [true, false] {
        let handled = crate::input::key(
            surface,
            KeyEvent {
                vk,
                down,
                repeat: false,
                modifiers,
            },
        );
        // Unhandled printable keys type their character, as GPUI's input
        // handler would get it.
        if down && !handled && !modifiers.ctrl && !modifiers.alt && !modifiers.meta {
            let ch = match vk {
                0x20 => Some(' '),
                0x30..=0x39 => char::from_u32(vk as u32),
                0x41..=0x5A => char::from_u32(if modifiers.shift { vk } else { vk + 32 } as u32),
                _ => None,
            };
            if let Some(ch) = ch {
                crate::text_input::char_input(surface, &ch.to_string());
            }
        }
    }
}

/// Draws what changed and takes the window's content, in device pixels,
/// with its size in points. `None` when the window closed.
pub(crate) async fn frame(
    window: WindowHandle<AppView>,
    cx: &mut AsyncApp,
) -> Option<anyhow::Result<(::image::RgbaImage, gpui::Size<gpui::Pixels>)>> {
    #[cfg(target_os = "linux")]
    {
        let watch = match with_surface(window, cx, |_, window, _| x11::Watch::new(window))? {
            Ok(watch) => watch,
            Err(err) => return Some(Err(err)),
        };
        if !drawn(window, cx).await {
            return None;
        }
        watch.presented(cx).await;
        let frame = watch.capture();
        with_surface(window, cx, |_, window, _| {
            Ok((frame?, window.viewport_size()))
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        if !drawn(window, cx).await {
            return None;
        }
        with_surface(window, cx, |_, window, _| {
            Ok((capture(window)?, window.viewport_size()))
        })
    }
}

/// The window's last frame, in device pixels.
#[cfg(all(feature = "shot", any(target_os = "macos", windows)))]
pub(crate) fn capture(window: &mut Window) -> anyhow::Result<::image::RgbaImage> {
    window.render_to_image()
}

/// The window's last frame, in device pixels.
#[cfg(all(not(feature = "shot"), any(target_os = "macos", windows)))]
pub(crate) fn capture(_window: &mut Window) -> anyhow::Result<::image::RgbaImage> {
    anyhow::bail!("shots need nsh-platform-gpui's feature `shot`")
}

/// Shots on Linux: the window read from the X server.
#[cfg(target_os = "linux")]
pub(crate) mod x11 {
    use std::time::{Duration, Instant};

    use anyhow::{Context, bail};
    use gpui::{AsyncApp, Window};
    use x11rb::{
        connection::{Connection, RequestConnection},
        protocol::{
            Event,
            present::{self, ConnectionExt as _},
            xproto::{ConnectionExt as _, ImageFormat, ImageOrder},
        },
        rust_connection::RustConnection,
    };

    /// A window being shot, through a connection of our own.
    pub(super) struct Watch {
        conn: RustConnection,
        window: u32,
        /// GPUI's frames reach the X server through DRI3 buffers and the
        /// Present extension, which shows them on the window's vertical
        /// blanks (otherwise they are copied in when presented).
        present: bool,
    }

    /// The vertical blanks to wait for after the frame: a frame waits for
    /// the ones queued before it (`desired_maximum_frame_latency` 2).
    const BLANKS: u64 = 3;

    impl Watch {
        /// A connection to `window`'s X server.
        pub(super) fn new(window: &Window) -> anyhow::Result<Watch> {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let id = match HasWindowHandle::window_handle(window)?.as_raw() {
                RawWindowHandle::Xlib(h) => h.window as u32,
                RawWindowHandle::Xcb(h) => h.window.get(),
                _ => bail!("shots need X11 (run with `WAYLAND_DISPLAY=`)"),
            };
            let (conn, _) = x11rb::connect(None).context("cannot connect to the X server")?;
            let has = |name| {
                conn.extension_information(name)
                    .is_ok_and(|info| info.is_some())
            };
            let present = has(x11rb::protocol::dri3::X11_EXTENSION_NAME)
                && has(present::X11_EXTENSION_NAME)
                && conn
                    .present_query_version(1, 2)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .is_some()
                && conn
                    .present_select_input(
                        conn.generate_id()?,
                        id,
                        present::EventMask::COMPLETE_NOTIFY,
                    )
                    .ok()
                    .and_then(|c| c.check().ok())
                    .is_some();
            Ok(Watch {
                conn,
                window: id,
                present,
            })
        }

        /// Waits for the X server to show the frame GPUI drew last: for the
        /// window's vertical blanks to pass the ones that frame waits for.
        /// They come once a second for a hidden Xwayland (its compositor
        /// sends it no frame callbacks), which makes every shot take
        /// seconds; Xwayland started with `-shm` copies frames in at once.
        pub(super) async fn presented(&self, cx: &mut AsyncApp) {
            if !self.present {
                return;
            }
            let start = Instant::now();
            let Some(now) = self.msc(0, 1, cx).await else {
                return;
            };
            if self.msc(now + BLANKS, 2, cx).await.is_none() {
                return;
            }
            if start.elapsed() > Duration::from_secs(1) {
                log::warn!(
                    "test: the X server shows frames slowly (a hidden Xwayland? start it with -shm)"
                );
            }
        }

        /// Waits for the window's vertical blank `target` (0: the next
        /// event tells the current one) and returns its number; `None` on
        /// errors or after 5 s.
        async fn msc(&self, target: u64, serial: u32, cx: &mut AsyncApp) -> Option<u64> {
            self.conn
                .present_notify_msc(self.window, serial, target, 0, 0)
                .ok()?;
            self.conn.flush().ok()?;
            let start = Instant::now();
            loop {
                while let Some(event) = self.conn.poll_for_event().ok()? {
                    if let Event::PresentCompleteNotify(e) = event
                        && e.window == self.window
                        && e.kind == present::CompleteKind::NOTIFY_MSC
                        && e.serial == serial
                    {
                        return Some(e.msc);
                    }
                }
                if start.elapsed() > Duration::from_secs(5) {
                    log::warn!("test: the X server reports no vertical blanks");
                    return None;
                }
                super::sleep(cx, 2).await;
            }
        }

        /// The window's pixels.
        pub(super) fn capture(&self) -> anyhow::Result<::image::RgbaImage> {
            let conn = &self.conn;
            let geometry = conn.get_geometry(self.window)?.reply()?;
            let attributes = conn.get_window_attributes(self.window)?.reply()?;
            let (w, h) = (geometry.width, geometry.height);
            let reply = conn
                .get_image(ImageFormat::Z_PIXMAP, self.window, 0, 0, w, h, !0)?
                .reply()?;
            let setup = conn.setup();
            let bpp = setup
                .pixmap_formats
                .iter()
                .find(|f| f.depth == reply.depth)
                .map_or(0, |f| f.bits_per_pixel);
            let visual = setup
                .roots
                .iter()
                .flat_map(|s| &s.allowed_depths)
                .flat_map(|d| &d.visuals)
                .find(|v| v.visual_id == attributes.visual)
                .context("the window's visual is unknown")?;
            if bpp != 32 {
                bail!("{bpp} bits per pixel are not supported");
            }
            Ok(to_rgba(
                &reply.data,
                w as u32,
                h as u32,
                [visual.red_mask, visual.green_mask, visual.blue_mask],
                setup.image_byte_order == ImageOrder::MSB_FIRST,
            ))
        }
    }

    /// 32-bit Z pixmap pixels (rows of `width`, which need no padding at
    /// 32 bits) with `masks` (red, green, blue) as opaque RGBA.
    pub(super) fn to_rgba(
        data: &[u8],
        width: u32,
        height: u32,
        masks: [u32; 3],
        msb_first: bool,
    ) -> ::image::RgbaImage {
        let channel = |px: u32, mask: u32| -> u8 {
            if mask == 0 {
                return 0;
            }
            let shift = mask.trailing_zeros();
            let (v, max) = ((px & mask) >> shift, mask >> shift);
            (v as u64 * 255 / max as u64) as u8
        };
        let mut out = ::image::RgbaImage::new(width, height);
        for (px, rgba) in data.as_chunks::<4>().0.iter().zip(out.pixels_mut()) {
            let px = if msb_first {
                u32::from_be_bytes(*px)
            } else {
                u32::from_le_bytes(*px)
            };
            rgba.0 = [
                channel(px, masks[0]),
                channel(px, masks[1]),
                channel(px, masks[2]),
                255,
            ];
        }
        out
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn x_pixels_become_rgba() {
        // BGRX, least significant byte first: red, then green.
        let data = [0, 0, 255, 0, 0, 128, 0, 0];
        let masks = [0xff0000, 0xff00, 0xff];
        let image = x11::to_rgba(&data, 2, 1, masks, false);
        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(1, 0).0, [0, 128, 0, 255]);
        let image = x11::to_rgba(&[0, 255, 0, 0], 1, 1, masks, true);
        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }
}
