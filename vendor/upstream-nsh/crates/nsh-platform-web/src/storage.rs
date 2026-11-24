//! The app's storage (`nsh/storage`) in the browser's `localStorage`.
//!
//! `localStorage` is the origin's, shared by every app the origin serves:
//! a published site per path, the development host and the website's
//! demos side by side. Each entry is kept under the app's prefix
//! ([`nsh_engine::local_storage_prefix`]): its id (`nsh.json` `"id"`), else
//! its page's path; so apps keep apart and the page's other keys are left
//! alone.
//!
//! `localStorage` throws when the browser refuses it (blocked storage,
//! the quota): the error is logged and the app keeps its entries in
//! memory.

use std::{cell::RefCell, collections::BTreeMap};

use nsh_engine::{StorageBackend, StorageChange};

use crate::{bytes, ffi};

thread_local! {
    /// What `nsh.js` handed over during a `storage_*` call
    /// ([`nsh_storage_reply`]).
    static REPLY: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Takes what [`nsh_storage_reply`] handed over.
pub(crate) fn take_reply() -> Option<String> {
    REPLY.with(|r| r.borrow_mut().take())
}

/// The reply to the `storage_*` call in progress: the entries `storage_load`
/// read (JSON) or the error a call failed with.
///
/// # Safety
///
/// `ptr..ptr + len` is memory from [`crate::nsh_alloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nsh_storage_reply(ptr: *const u8, len: usize) {
    // SAFETY: guaranteed by the caller.
    let reply = String::from_utf8_lossy(unsafe { bytes(ptr, len) }).into_owned();
    REPLY.with(|r| *r.borrow_mut() = Some(reply));
}

/// The app's entries in `localStorage`, under the app's prefix. Holds no JS
/// handle: `nsh.js` reaches `localStorage` on each call.
pub(crate) struct LocalStorage {
    prefix: String,
}

impl LocalStorage {
    /// The storage of app `app_id` on the page at `base_url`
    /// (`document.baseURI`). Without an id, the page's path names it
    /// (`None`: an unknown page, whose apps share one namespace).
    pub(crate) fn new(app_id: Option<&str>, base_url: Option<&str>) -> Self {
        Self {
            prefix: nsh_engine::local_storage_prefix(app_id, base_url.unwrap_or("")),
        }
    }
}

impl StorageBackend for LocalStorage {
    fn load(&self) -> Result<BTreeMap<String, String>, String> {
        let reply = ffi::storage_load(&self.prefix)?;
        serde_json::from_str(&reply).map_err(|e| e.to_string())
    }

    fn write(&self, change: StorageChange) {
        let result = match &change {
            StorageChange::Set(key, value) => ffi::storage_set(&self.prefix, key, value),
            StorageChange::Remove(key) => ffi::storage_remove(&self.prefix, key),
            StorageChange::Clear => ffi::storage_clear(&self.prefix),
        };
        if let Err(e) = result {
            log::error!(target: "js", "the app's storage could not be written: {e}");
        }
    }
}
