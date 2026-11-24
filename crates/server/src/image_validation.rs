//! Decode raster evidence before publishing it. IDs remain independent from
//! content digests; the latter attest the bytes actually saved and replayed.
pub(crate) fn mime(bytes: &[u8]) -> Result<&'static str,String> {
    let format = image::guess_format(bytes).map_err(|error|error.to_string())?;
    let mime = match format {
        image::ImageFormat::Png => "image/png", image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif", image::ImageFormat::WebP => "image/webp",
        image::ImageFormat::Bmp => "image/bmp", image::ImageFormat::Ico => "image/x-icon",
        image::ImageFormat::Avif => "image/avif", _ => return Err("Unsupported raster image".into()),
    };
    #[cfg(target_os="macos")]
    if format == image::ImageFormat::Avif { mac_avif(bytes)?; return Ok(mime); }
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes),format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(32768); limits.max_image_height = Some(32768); limits.max_alloc = Some(256*1024*1024);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|error|error.to_string())?;
    if decoded.width()==0 || decoded.height()==0 || u64::from(decoded.width())*u64::from(decoded.height())>64_000_000 {
        return Err("Image dimensions exceed the decoding limit".into());
    }
    Ok(mime)
}

#[cfg(target_os="macos")]
fn mac_avif(bytes: &[u8]) -> Result<(),String> {
    use std::ffi::c_void;
    #[link(name="CoreFoundation",kind="framework")]
    unsafe extern "C" {
        fn CFDataCreate(allocator:*const c_void,bytes:*const u8,length:isize)->*const c_void;
        fn CFRelease(value:*const c_void);
        fn CFDictionaryGetValue(dictionary:*const c_void,key:*const c_void)->*const c_void;
        fn CFNumberGetValue(number:*const c_void,kind:isize,value:*mut c_void)->bool;
    }
    #[link(name="ImageIO",kind="framework")]
    unsafe extern "C" {
        fn CGImageSourceCreateWithData(data:*const c_void,options:*const c_void)->*const c_void;
        fn CGImageSourceGetStatus(source:*const c_void)->i32;
        fn CGImageSourceCopyPropertiesAtIndex(source:*const c_void,index:usize,options:*const c_void)->*const c_void;
        fn CGImageSourceCreateImageAtIndex(source:*const c_void,index:usize,options:*const c_void)->*const c_void;
        static kCGImagePropertyPixelWidth:*const c_void;
        static kCGImagePropertyPixelHeight:*const c_void;
    }
    #[link(name="CoreGraphics",kind="framework")]
    unsafe extern "C" {
        fn CGImageGetWidth(image:*const c_void)->usize;
        fn CGImageGetHeight(image:*const c_void)->usize;
        fn CGImageRelease(image:*const c_void);
    }
    unsafe {
        let data=CFDataCreate(std::ptr::null(),bytes.as_ptr(),bytes.len() as isize);
        if data.is_null(){return Err("Image data allocation failed".into());}
        let source=CGImageSourceCreateWithData(data,std::ptr::null()); CFRelease(data);
        if source.is_null(){return Err("Invalid AVIF image".into());}
        if CGImageSourceGetStatus(source)!=0 {CFRelease(source);return Err("Incomplete AVIF image".into());}
        let properties=CGImageSourceCopyPropertiesAtIndex(source,0,std::ptr::null());
        if properties.is_null(){CFRelease(source);return Err("AVIF dimensions unavailable".into());}
        let mut width=0i64;let mut height=0i64;
        let width_value=CFDictionaryGetValue(properties,kCGImagePropertyPixelWidth);
        let height_value=CFDictionaryGetValue(properties,kCGImagePropertyPixelHeight);
        let valid=!width_value.is_null() && !height_value.is_null()
            && CFNumberGetValue(width_value,4,(&mut width as *mut i64).cast())
            && CFNumberGetValue(height_value,4,(&mut height as *mut i64).cast());
        CFRelease(properties);
        if !valid || width<=0 || height<=0 || width>32768 || height>32768 || width.saturating_mul(height)>64_000_000 {
            CFRelease(source);return Err("Image dimensions exceed the decoding limit".into());
        }
        let decoded=CGImageSourceCreateImageAtIndex(source,0,std::ptr::null()); CFRelease(source);
        if decoded.is_null(){return Err("AVIF image failed to decode".into());}
        let width=CGImageGetWidth(decoded);let height=CGImageGetHeight(decoded);CGImageRelease(decoded);
        if width==0 || height==0 || width>32768 || height>32768 || width.saturating_mul(height)>64_000_000 {
            return Err("Image dimensions exceed the decoding limit".into());
        }
    }
    Ok(())
}
