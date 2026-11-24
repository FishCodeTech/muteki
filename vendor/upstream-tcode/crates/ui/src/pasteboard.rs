#[cfg(all(feature = "pasteboard-image", target_os = "macos"))]
pub(crate) fn read_pasteboard_image() -> Option<(String, Vec<u8>)> {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::ns_string;

    let pasteboard = NSPasteboard::generalPasteboard();
    for (pasteboard_type, mime) in [
        (ns_string!("public.png"), "image/png"),
        (ns_string!("public.jpeg"), "image/jpeg"),
        (ns_string!("public.tiff"), "image/tiff"),
    ] {
        if let Some(data) = pasteboard.dataForType(pasteboard_type) {
            return Some((mime.to_string(), data.to_vec()));
        }
    }
    None
}

#[cfg(not(all(feature = "pasteboard-image", target_os = "macos")))]
pub(crate) fn read_pasteboard_image() -> Option<(String, Vec<u8>)> {
    None
}
