use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use image::ImageEncoder;
use uiautomation::UIElement;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, GetDC, HBITMAP, HDC, HGDIOBJ, ReleaseDC, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindow};

use super::super::{BackendError, BackendErrorCode, RootInfo};

static NEXT_CAPTURE_ID: AtomicU64 = AtomicU64::new(1);

pub(super) fn capture_window(root: &RootInfo) -> Result<Vec<u8>, BackendError> {
    let hwnd = HWND(root.window_id as usize as *mut _);
    if hwnd.is_invalid() {
        return Err(capture_error("the root HWND is null"));
    }
    // SAFETY: The HWND is validated before use, and the capture resources own all GDI cleanup.
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(capture_error("the root HWND is no longer valid"));
        }
        capture_window_inner(hwnd)
    }
}

unsafe fn capture_window_inner(hwnd: HWND) -> Result<Vec<u8>, BackendError> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }
        .map_err(|error| capture_error(format!("GetWindowRect failed: {error}")))?;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return Err(capture_error(format!(
            "the window has invalid capture dimensions {width}x{height}"
        )));
    }
    let width_u32 = u32::try_from(width)
        .map_err(|error| capture_error(format!("window width is out of range: {error}")))?;
    let height_u32 = u32::try_from(height)
        .map_err(|error| capture_error(format!("window height is out of range: {error}")))?;
    let byte_len = usize::try_from(width_u32)
        .ok()
        .zip(usize::try_from(height_u32).ok())
        .and_then(|(width, height)| width.checked_mul(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| capture_error("window capture buffer size overflowed"))?;

    let window_dc = unsafe { WindowDc::new(hwnd) }?;
    let memory_dc = unsafe { MemoryDc::new(window_dc.hdc) }?;
    let surface = unsafe { DibSurface::new(memory_dc.hdc, width, height) }?;

    if !unsafe { PrintWindow(hwnd, memory_dc.hdc, PRINT_WINDOW_FLAGS(2)) }.as_bool() {
        return Err(capture_error("PrintWindow returned FALSE"));
    }

    // DIB sections are tightly packed at 32 bpp. PrintWindow commonly leaves alpha at zero, so
    // normalize the BGRA pixels to opaque RGBA for a portable PNG.
    let bgra = unsafe { std::slice::from_raw_parts(surface.bits.cast::<u8>(), byte_len) };
    let mut rgba = Vec::with_capacity(byte_len);
    for pixel in bgra.as_chunks::<4>().0 {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], u8::MAX]);
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            &rgba,
            width_u32,
            height_u32,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|error| capture_error(format!("PrintWindow PNG encoding failed: {error}")))?;
    Ok(png)
}

struct WindowDc {
    hwnd: HWND,
    hdc: HDC,
}

impl WindowDc {
    unsafe fn new(hwnd: HWND) -> Result<Self, BackendError> {
        let hdc = unsafe { GetDC(Some(hwnd)) };
        if hdc.is_invalid() {
            Err(capture_error("GetDC returned a null device context"))
        } else {
            Ok(Self { hwnd, hdc })
        }
    }
}

impl Drop for WindowDc {
    fn drop(&mut self) {
        // SAFETY: `hdc` was returned by GetDC for this HWND and is released exactly once.
        let _ = unsafe { ReleaseDC(Some(self.hwnd), self.hdc) };
    }
}

struct MemoryDc {
    hdc: HDC,
}

impl MemoryDc {
    unsafe fn new(compatible_with: HDC) -> Result<Self, BackendError> {
        let hdc = unsafe { CreateCompatibleDC(Some(compatible_with)) };
        if hdc.is_invalid() {
            Err(capture_error(
                "CreateCompatibleDC returned a null device context",
            ))
        } else {
            Ok(Self { hdc })
        }
    }
}

impl Drop for MemoryDc {
    fn drop(&mut self) {
        // SAFETY: `hdc` was created by CreateCompatibleDC and is deleted exactly once.
        let _ = unsafe { DeleteDC(self.hdc) };
    }
}

struct DibSurface {
    hdc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut std::ffi::c_void,
}

impl DibSurface {
    unsafe fn new(hdc: HDC, width: i32, height: i32) -> Result<Self, BackendError> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or_default(),
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..BITMAPINFOHEADER::default()
            },
            ..BITMAPINFO::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap =
            unsafe { CreateDIBSection(Some(hdc), &info, DIB_RGB_COLORS, &mut bits, None, 0) }
                .map_err(|error| capture_error(format!("CreateDIBSection failed: {error}")))?;
        if bits.is_null() {
            let _ = unsafe { DeleteObject(bitmap.into()) };
            return Err(capture_error(
                "CreateDIBSection returned a null pixel buffer",
            ));
        }
        let previous = unsafe { SelectObject(hdc, bitmap.into()) };
        if previous.is_invalid() {
            let _ = unsafe { DeleteObject(bitmap.into()) };
            return Err(capture_error("SelectObject failed for the capture bitmap"));
        }
        Ok(Self {
            hdc,
            bitmap,
            previous,
            bits,
        })
    }
}

impl Drop for DibSurface {
    fn drop(&mut self) {
        // SAFETY: Restore the previously selected object before deleting the owned bitmap.
        unsafe {
            let _ = SelectObject(self.hdc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
        }
    }
}

pub(super) fn capture_element(element: &UIElement) -> Result<Vec<u8>, BackendError> {
    let screenshot = element.screenshot().map_err(|error| {
        capture_error(format!("uiautomation element screenshot failed: {error}"))
    })?;
    let temporary = TemporaryPng::new()?;
    screenshot
        .save_png(&temporary.path)
        .map_err(|error| capture_error(format!("uiautomation PNG encoding failed: {error}")))?;
    std::fs::read(&temporary.path)
        .map_err(|error| capture_error(format!("reading encoded screenshot failed: {error}")))
}

struct TemporaryPng {
    directory: PathBuf,
    path: PathBuf,
}

impl TemporaryPng {
    fn new() -> Result<Self, BackendError> {
        let base = std::env::temp_dir();
        for _ in 0..100 {
            let id = NEXT_CAPTURE_ID.fetch_add(1, Ordering::Relaxed);
            let directory = base.join(format!("tcode-uia-capture-{}-{id}", std::process::id()));
            match std::fs::create_dir(&directory) {
                Ok(()) => {
                    let path = directory.join("capture.png");
                    return Ok(Self { directory, path });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(capture_error(format!(
                        "creating screenshot temporary directory failed: {error}"
                    )));
                }
            }
        }
        Err(capture_error(
            "could not allocate a unique screenshot temporary directory",
        ))
    }
}

impl Drop for TemporaryPng {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

fn capture_error(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::CaptureFailed, message)
}
