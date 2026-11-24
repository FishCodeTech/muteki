//! iOS static-library entry point used by the Swift host.
//!
//! Everything here references symbols the Swift host provides, so the crate
//! is empty on other targets (it is a workspace member and gets built there).

#[cfg(target_os = "ios")]
mod host;

#[cfg(target_os = "ios")]
mod speech;

#[cfg(target_os = "ios")]
mod entry;

#[cfg(target_os = "ios")]
pub use entry::*;
