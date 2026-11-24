//! Word segmentation for a double-clicked word, from the platform's own text
//! services where it has them.
//!
//! Each platform's segmenter knows the words of languages written without
//! spaces, such as Chinese, which a rule over character classes cannot
//! find. Apple platforms use Core Foundation's tokenizer, Windows the ICU it
//! ships, and a browser its `Intl.Segmenter`; Android's comes from its Java
//! ICU through the host. Linux has no system segmenter that every install
//! carries, so it bundles one.

use std::rc::Rc;

/// Splits text into words, runs of spaces and marks as the platform's own
/// text does.
pub trait WordSegmenter {
    /// The byte offsets at which `text`'s segments begin and end, ascending,
    /// from 0 to its length.
    fn breaks(&self, text: &str) -> Vec<usize>;
}

/// The segmentation this platform offers without a host's help, when it has
/// one.
pub fn system() -> Option<Rc<dyn WordSegmenter>> {
    platform::system()
}

/// The byte offset in `text` of every UTF-16 index, its length included.
fn utf16_offsets(text: &str) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(text.len() + 1);
    for (offset, character) in text.char_indices() {
        for _ in 0..character.len_utf16() {
            offsets.push(offset);
        }
    }
    offsets.push(text.len());
    offsets
}

/// Byte offsets for `breaks` given as UTF-16 indices into `text`, the two
/// ends of the text always among them.
pub fn breaks_from_utf16(text: &str, breaks: impl IntoIterator<Item = usize>) -> Vec<usize> {
    let offsets = utf16_offsets(text);
    let mut bytes = breaks
        .into_iter()
        .filter_map(|index| offsets.get(index).copied())
        .chain([0, text.len()])
        .collect::<Vec<_>>();
    bytes.sort_unstable();
    bytes.dedup();
    bytes
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod platform {
    use std::{ffi::c_void, rc::Rc};

    use super::WordSegmenter;

    type CFIndex = isize;
    type CFOptionFlags = usize;
    type CFTypeRef = *const c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CFRange {
        location: CFIndex,
        length: CFIndex,
    }

    const WORD_BOUNDARY: CFOptionFlags = 4;
    const TOKEN_NONE: CFOptionFlags = 0;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCharacters(
            allocator: CFTypeRef,
            characters: *const u16,
            length: CFIndex,
        ) -> CFTypeRef;
        fn CFStringTokenizerCreate(
            allocator: CFTypeRef,
            string: CFTypeRef,
            range: CFRange,
            options: CFOptionFlags,
            locale: CFTypeRef,
        ) -> CFTypeRef;
        fn CFStringTokenizerAdvanceToNextToken(tokenizer: CFTypeRef) -> CFOptionFlags;
        fn CFStringTokenizerGetCurrentTokenRange(tokenizer: CFTypeRef) -> CFRange;
        fn CFRelease(object: CFTypeRef);
    }

    struct CoreFoundation;

    impl WordSegmenter for CoreFoundation {
        fn breaks(&self, text: &str) -> Vec<usize> {
            let units = text.encode_utf16().collect::<Vec<_>>();
            let mut breaks = Vec::new();
            // SAFETY: the string and tokenizer are created here, used only
            // while `units` lives, and released before returning.
            unsafe {
                let string = CFStringCreateWithCharacters(
                    std::ptr::null(),
                    units.as_ptr(),
                    units.len() as CFIndex,
                );
                if string.is_null() {
                    return super::breaks_from_utf16(text, []);
                }
                let range = CFRange {
                    location: 0,
                    length: units.len() as CFIndex,
                };
                let tokenizer = CFStringTokenizerCreate(
                    std::ptr::null(),
                    string,
                    range,
                    WORD_BOUNDARY,
                    std::ptr::null(),
                );
                if !tokenizer.is_null() {
                    while CFStringTokenizerAdvanceToNextToken(tokenizer) != TOKEN_NONE {
                        let token = CFStringTokenizerGetCurrentTokenRange(tokenizer);
                        breaks.push(token.location as usize);
                        breaks.push((token.location + token.length) as usize);
                    }
                    CFRelease(tokenizer);
                }
                CFRelease(string);
            }
            super::breaks_from_utf16(text, breaks)
        }
    }

    pub(super) fn system() -> Option<Rc<dyn WordSegmenter>> {
        Some(Rc::new(CoreFoundation))
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use std::{
        ffi::{c_char, c_void},
        rc::Rc,
        sync::OnceLock,
    };

    use super::WordSegmenter;

    const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x800;
    const UBRK_WORD: i32 = 1;
    const UBRK_DONE: i32 = -1;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    }

    type Open = unsafe extern "C" fn(i32, *const c_char, *const u16, i32, *mut i32) -> *mut c_void;
    type Step = unsafe extern "C" fn(*mut c_void) -> i32;
    type Close = unsafe extern "C" fn(*mut c_void);

    /// The break iterator of the ICU Windows ships since 10 1903, loaded
    /// when first needed so a Windows without it still starts.
    #[derive(Clone, Copy)]
    struct Icu {
        open: Open,
        first: Step,
        next: Step,
        close: Close,
    }

    fn icu() -> Option<Icu> {
        static ICU: OnceLock<Option<Icu>> = OnceLock::new();
        *ICU.get_or_init(|| {
            let name = "icu.dll\0".encode_utf16().collect::<Vec<_>>();
            // SAFETY: icu.dll is loaded from System32 and never unloaded;
            // each symbol is its documented ICU4C C function.
            unsafe {
                let module = LoadLibraryExW(
                    name.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_LIBRARY_SEARCH_SYSTEM32,
                );
                if module.is_null() {
                    return None;
                }
                let symbol = |name: &[u8]| {
                    let address = GetProcAddress(module, name.as_ptr().cast());
                    (!address.is_null()).then_some(address)
                };
                Some(Icu {
                    open: std::mem::transmute::<*mut c_void, Open>(symbol(b"ubrk_open\0")?),
                    first: std::mem::transmute::<*mut c_void, Step>(symbol(b"ubrk_first\0")?),
                    next: std::mem::transmute::<*mut c_void, Step>(symbol(b"ubrk_next\0")?),
                    close: std::mem::transmute::<*mut c_void, Close>(symbol(b"ubrk_close\0")?),
                })
            }
        })
    }

    impl WordSegmenter for Icu {
        fn breaks(&self, text: &str) -> Vec<usize> {
            let units = text.encode_utf16().collect::<Vec<_>>();
            let mut breaks = Vec::new();
            let mut status = 0;
            // SAFETY: the iterator reads `units`, which outlives it, and is
            // closed before returning.
            unsafe {
                let iterator = (self.open)(
                    UBRK_WORD,
                    c"".as_ptr(),
                    units.as_ptr(),
                    units.len() as i32,
                    &mut status,
                );
                if iterator.is_null() || status > 0 {
                    return super::breaks_from_utf16(text, []);
                }
                let mut boundary = (self.first)(iterator);
                while boundary != UBRK_DONE {
                    breaks.push(boundary as usize);
                    boundary = (self.next)(iterator);
                }
                (self.close)(iterator);
            }
            super::breaks_from_utf16(text, breaks)
        }
    }

    pub(super) fn system() -> Option<Rc<dyn WordSegmenter>> {
        icu().map(|icu| Rc::new(icu) as Rc<dyn WordSegmenter>)
    }
}

#[cfg(target_arch = "wasm32")]
mod platform {
    use std::rc::Rc;

    use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

    use super::WordSegmenter;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_namespace = Intl, js_name = Segmenter)]
        type IntlSegmenter;

        #[wasm_bindgen(constructor, js_namespace = Intl, js_class = "Segmenter", catch)]
        fn new(locales: &JsValue, options: &JsValue) -> Result<IntlSegmenter, JsValue>;

        #[wasm_bindgen(method)]
        fn segment(this: &IntlSegmenter, text: &str) -> JsValue;
    }

    struct Browser(IntlSegmenter);

    impl WordSegmenter for Browser {
        fn breaks(&self, text: &str) -> Vec<usize> {
            let segments = js_sys::Array::from(&self.0.segment(text));
            let indices = segments.iter().filter_map(|segment| {
                js_sys::Reflect::get(&segment, &JsValue::from_str("index"))
                    .ok()?
                    .as_f64()
                    .map(|index| index as usize)
            });
            super::breaks_from_utf16(text, indices)
        }
    }

    pub(super) fn system() -> Option<Rc<dyn WordSegmenter>> {
        let options = js_sys::Object::new();
        js_sys::Reflect::set(
            &options,
            &JsValue::from_str("granularity"),
            &JsValue::from_str("word"),
        )
        .ok()?;
        let segmenter = IntlSegmenter::new(&JsValue::UNDEFINED, &options).ok()?;
        Some(Rc::new(Browser(segmenter)))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::rc::Rc;

    use icu_segmenter::{
        WordSegmenter as IcuWordSegmenter, WordSegmenterBorrowed,
        options::WordBreakInvariantOptions,
    };

    use super::WordSegmenter;

    struct Bundled(WordSegmenterBorrowed<'static>);

    impl WordSegmenter for Bundled {
        fn breaks(&self, text: &str) -> Vec<usize> {
            self.0.segment_str(text).collect()
        }
    }

    pub(super) fn system() -> Option<Rc<dyn WordSegmenter>> {
        Some(Rc::new(Bundled(IcuWordSegmenter::new_dictionary(
            WordBreakInvariantOptions::default(),
        ))))
    }
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "linux",
    target_arch = "wasm32"
)))]
mod platform {
    use std::rc::Rc;

    use super::WordSegmenter;

    pub(super) fn system() -> Option<Rc<dyn WordSegmenter>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_breaks_land_on_the_bytes_of_the_same_characters() {
        // "中" is three bytes and one unit, "𝄞" four bytes and two units.
        assert_eq!(breaks_from_utf16("a中𝄞b", [1, 2, 4]), vec![0, 1, 4, 8, 9]);
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn the_system_segmenter_finds_chinese_words() {
        let segmenter = system().expect("this platform segments words");
        let text = "我们修复问题";
        let breaks = segmenter.breaks(text);
        let words = breaks
            .windows(2)
            .map(|pair| &text[pair[0]..pair[1]])
            .collect::<Vec<_>>();
        assert!(words.contains(&"修复"), "{words:?}");
    }
}
