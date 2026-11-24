//! The last navigation failure reported by the native webview.
//!
//! A failed navigation is invisible to JavaScript: WebKit, WebKitGTK and WebView2
//! keep the previous document loaded, so the status probe keeps reporting the old URL and
//! an agent cannot tell an untrusted certificate from a slow dev server. Both
//! engines *do* report the failure to their navigation delegate, so we keep the
//! last one per native webview and surface it from `preview_status` and
//! `preview_wait_for`.
//!
//! The map is keyed by the native webview pointer because the macOS callback is
//! a plain Objective-C IMP that cannot carry Rust state. Everything here runs on
//! the UI thread that owns the webviews, hence `thread_local!`.

#[cfg(not(target_os = "android"))]
use std::cell::RefCell;
#[cfg(not(target_os = "android"))]
use std::collections::HashMap;

/// One failed navigation, as the platform described it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoadError {
    pub(crate) url: String,
    /// `NSURLErrorDomain -1202` on macOS, `CertificateIsInvalid` on Windows,
    /// `g-io-error-quark ConnectionRefused` on Linux.
    pub(crate) code: String,
    pub(crate) message: String,
}

impl LoadError {
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "url": self.url,
            "code": self.code,
            "message": self.message,
        })
    }

    /// The one-line form used where a tool can only answer with an error.
    pub(crate) fn describe(&self) -> String {
        format!(
            "navigation to {} failed: {} ({})",
            self.url, self.message, self.code
        )
    }
}

#[cfg(not(target_os = "android"))]
thread_local! {
    static LAST: RefCell<HashMap<usize, LoadError>> = RefCell::new(HashMap::new());
}

#[cfg(not(target_os = "android"))]
fn clear(webview: usize) {
    LAST.with_borrow_mut(|last| last.remove(&webview));
}

#[cfg(not(target_os = "android"))]
fn record(webview: usize, error: LoadError) {
    log::info!("preview: {}", error.describe());
    LAST.with_borrow_mut(|last| last.insert(webview, error));
}

/// Start reporting failed navigations for a freshly created webview.
#[cfg(not(target_os = "android"))]
pub(crate) fn install(raw: &wry::WebView) {
    imp::install(raw);
}

#[cfg(not(target_os = "android"))]
pub(crate) fn get(raw: &wry::WebView) -> Option<LoadError> {
    let webview = imp::key(raw);
    LAST.with_borrow(|last| last.get(&webview).cloned())
}

/// Drop the record for a webview that is going away, so a later allocation at
/// the same address cannot inherit it.
#[cfg(not(target_os = "android"))]
pub(crate) fn forget(raw: &wry::WebView) {
    clear(imp::key(raw));
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod imp {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{ffi, msg_send, sel};
    use objc2_foundation::{NSError, NSURL, NSURLErrorFailingURLErrorKey};
    #[cfg(target_os = "ios")]
    use wry::WebViewExtIOS as _;
    #[cfg(target_os = "macos")]
    use wry::WebViewExtMacOS as _;

    use super::LoadError;

    pub(super) fn key(raw: &wry::WebView) -> usize {
        Retained::as_ptr(&raw.webview()) as usize
    }

    /// wry's `WryNavigationDelegate` implements navigation policy, `didCommit`
    /// and `didFinish` only, so nothing observes a failed load. Add the three
    /// missing callbacks to its class — once for the process, and only where a
    /// future wry does not define them itself.
    pub(super) fn install(raw: &wry::WebView) {
        static PATCHED: std::sync::Once = std::sync::Once::new();

        let webview = raw.webview();
        let Some(delegate) = (unsafe { webview.navigationDelegate() }) else {
            log::debug!("preview: webview has no navigation delegate to observe");
            return;
        };
        // SAFETY: any Objective-C object may be viewed as an `AnyObject`.
        let class = unsafe { &*Retained::as_ptr(&delegate).cast::<AnyObject>() }.class();
        PATCHED.call_once(|| {
            let start: unsafe extern "C-unwind" fn(
                *mut AnyObject,
                Sel,
                *mut AnyObject,
                *mut AnyObject,
            ) = did_start;
            let fail: unsafe extern "C-unwind" fn(
                *mut AnyObject,
                Sel,
                *mut AnyObject,
                *mut AnyObject,
                *mut NSError,
            ) = did_fail;
            // SAFETY: both functions use the Objective-C calling convention and
            // match the encodings below.
            unsafe {
                let start: Imp = std::mem::transmute(start);
                let fail: Imp = std::mem::transmute(fail);
                add_method(
                    class,
                    sel!(webView:didStartProvisionalNavigation:),
                    c"v@:@@",
                    start,
                );
                add_method(
                    class,
                    sel!(webView:didFailProvisionalNavigation:withError:),
                    c"v@:@@@",
                    fail,
                );
                add_method(
                    class,
                    sel!(webView:didFailNavigation:withError:),
                    c"v@:@@@",
                    fail,
                );
            }
        });
        // WKWebView caches which delegate methods exist when the delegate is
        // assigned, so re-assign it: without this the very first webview (whose
        // delegate was installed before the patch above) never calls them.
        unsafe {
            webview.setNavigationDelegate(None);
            webview.setNavigationDelegate(Some(&delegate));
        }
    }

    /// # Safety
    ///
    /// `imp` must be an `extern "C"` function whose signature matches `types`.
    unsafe fn add_method(class: &AnyClass, selector: Sel, types: &std::ffi::CStr, imp: Imp) {
        if class.responds_to(selector) {
            return;
        }
        let class = std::ptr::from_ref(class).cast_mut();
        let added = unsafe { ffi::class_addMethod(class, selector, imp, types.as_ptr()) };
        if !added.as_bool() {
            log::debug!("preview: failed to observe {selector} on the navigation delegate");
        }
    }

    unsafe extern "C-unwind" fn did_start(
        _this: *mut AnyObject,
        _cmd: Sel,
        webview: *mut AnyObject,
        _navigation: *mut AnyObject,
    ) {
        super::clear(webview as usize);
    }

    unsafe extern "C-unwind" fn did_fail(
        _this: *mut AnyObject,
        _cmd: Sel,
        webview: *mut AnyObject,
        _navigation: *mut AnyObject,
        error: *mut NSError,
    ) {
        // SAFETY: WebKit hands us a live error for the failing web view.
        let Some(error) = (unsafe { error.as_ref() }) else {
            return;
        };
        super::record(
            webview as usize,
            LoadError {
                url: failing_url(error)
                    .or_else(|| current_url(webview))
                    .unwrap_or_default(),
                code: format!("{} {}", error.domain(), error.code()),
                message: error.localizedDescription().to_string(),
            },
        );
    }

    /// The URL WebKit was loading, which is *not* `WKWebView.URL` for a
    /// provisional failure — that still points at the page left on screen.
    fn failing_url(error: &NSError) -> Option<String> {
        let info = error.userInfo();
        let failing = unsafe { info.objectForKey(NSURLErrorFailingURLErrorKey) }?;
        let url = failing.downcast::<NSURL>().ok()?;
        url.absoluteString().map(|url| url.to_string())
    }

    fn current_url(webview: *mut AnyObject) -> Option<String> {
        // SAFETY: WebKit passes the live web view that failed. objc2-web-kit
        // declares WKWebView for macOS only, so `URL` is sent untyped.
        let webview = unsafe { webview.as_ref() }?;
        let url: Option<Retained<NSURL>> = unsafe { msg_send![webview, URL] };
        url?.absoluteString().map(|url| url.to_string())
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_WEB_ERROR_STATUS, COREWEBVIEW2_WEB_ERROR_STATUS_CANNOT_CONNECT,
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_COMMON_NAME_IS_INCORRECT,
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_EXPIRED,
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_IS_INVALID,
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_REVOKED,
        COREWEBVIEW2_WEB_ERROR_STATUS_CLIENT_CERTIFICATE_CONTAINS_ERRORS,
        COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_ABORTED,
        COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_RESET, COREWEBVIEW2_WEB_ERROR_STATUS_DISCONNECTED,
        COREWEBVIEW2_WEB_ERROR_STATUS_ERROR_HTTP_INVALID_SERVER_RESPONSE,
        COREWEBVIEW2_WEB_ERROR_STATUS_HOST_NAME_NOT_RESOLVED,
        COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED,
        COREWEBVIEW2_WEB_ERROR_STATUS_REDIRECT_FAILED,
        COREWEBVIEW2_WEB_ERROR_STATUS_SERVER_UNREACHABLE, COREWEBVIEW2_WEB_ERROR_STATUS_TIMEOUT,
        COREWEBVIEW2_WEB_ERROR_STATUS_UNEXPECTED_ERROR,
        COREWEBVIEW2_WEB_ERROR_STATUS_VALID_AUTHENTICATION_CREDENTIALS_REQUIRED,
        COREWEBVIEW2_WEB_ERROR_STATUS_VALID_PROXY_AUTHENTICATION_REQUIRED, ICoreWebView2,
    };
    use webview2_com::{
        NavigationCompletedEventHandler, NavigationStartingEventHandler, take_pwstr,
    };
    use windows_core::Interface as _;
    use wry::WebViewExtWindows as _;

    use super::LoadError;

    pub(super) fn key(raw: &wry::WebView) -> usize {
        raw.webview().as_raw() as usize
    }

    /// WebView2 reports a failed navigation only through `NavigationCompleted`;
    /// the page itself keeps showing the previous document.
    pub(super) fn install(raw: &wry::WebView) {
        let webview = raw.webview();
        let mut token = 0i64;
        let started = NavigationStartingEventHandler::create(Box::new(move |webview, _| {
            if let Some(webview) = webview {
                super::clear(webview.as_raw() as usize);
            }
            Ok(())
        }));
        if let Err(error) = unsafe { webview.add_NavigationStarting(&started, &mut token) } {
            log::debug!("preview: failed to observe navigation start: {error}");
        }
        let completed = NavigationCompletedEventHandler::create(Box::new(move |webview, args| {
            let (Some(webview), Some(args)) = (webview, args) else {
                return Ok(());
            };
            let mut succeeded = windows_core::BOOL::default();
            unsafe { args.IsSuccess(&mut succeeded)? };
            if succeeded.as_bool() {
                return Ok(());
            }
            let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
            unsafe { args.WebErrorStatus(&mut status)? };
            super::record(
                webview.as_raw() as usize,
                LoadError {
                    url: source_url(&webview),
                    code: status_name(status).to_string(),
                    message: format!("WebView2 error status {}", status.0),
                },
            );
            Ok(())
        }));
        if let Err(error) = unsafe { webview.add_NavigationCompleted(&completed, &mut token) } {
            log::debug!("preview: failed to observe navigation completion: {error}");
        }
    }

    fn source_url(webview: &ICoreWebView2) -> String {
        let mut source = windows_core::PWSTR::null();
        match unsafe { webview.Source(&mut source) } {
            Ok(()) => take_pwstr(source),
            Err(error) => {
                log::debug!("preview: failed to read the failing URL: {error}");
                String::new()
            }
        }
    }

    /// WebView2 statuses are a plain integer newtype; report the SDK name so the
    /// agent (and the user) can recognise e.g. a self-signed certificate.
    fn status_name(status: COREWEBVIEW2_WEB_ERROR_STATUS) -> &'static str {
        match status {
            COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_COMMON_NAME_IS_INCORRECT => {
                "CertificateCommonNameIsIncorrect"
            }
            COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_EXPIRED => "CertificateExpired",
            COREWEBVIEW2_WEB_ERROR_STATUS_CLIENT_CERTIFICATE_CONTAINS_ERRORS => {
                "ClientCertificateContainsErrors"
            }
            COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_REVOKED => "CertificateRevoked",
            COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_IS_INVALID => "CertificateIsInvalid",
            COREWEBVIEW2_WEB_ERROR_STATUS_SERVER_UNREACHABLE => "ServerUnreachable",
            COREWEBVIEW2_WEB_ERROR_STATUS_TIMEOUT => "Timeout",
            COREWEBVIEW2_WEB_ERROR_STATUS_ERROR_HTTP_INVALID_SERVER_RESPONSE => {
                "ErrorHttpInvalidServerResponse"
            }
            COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_ABORTED => "ConnectionAborted",
            COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_RESET => "ConnectionReset",
            COREWEBVIEW2_WEB_ERROR_STATUS_DISCONNECTED => "Disconnected",
            COREWEBVIEW2_WEB_ERROR_STATUS_CANNOT_CONNECT => "CannotConnect",
            COREWEBVIEW2_WEB_ERROR_STATUS_HOST_NAME_NOT_RESOLVED => "HostNameNotResolved",
            COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED => "OperationCanceled",
            COREWEBVIEW2_WEB_ERROR_STATUS_REDIRECT_FAILED => "RedirectFailed",
            COREWEBVIEW2_WEB_ERROR_STATUS_UNEXPECTED_ERROR => "UnexpectedError",
            COREWEBVIEW2_WEB_ERROR_STATUS_VALID_AUTHENTICATION_CREDENTIALS_REQUIRED => {
                "ValidAuthenticationCredentialsRequired"
            }
            COREWEBVIEW2_WEB_ERROR_STATUS_VALID_PROXY_AUTHENTICATION_REQUIRED => {
                "ValidProxyAuthenticationRequired"
            }
            _ => "Unknown",
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use webkit2gtk::glib::{self, ObjectType as _};
    use webkit2gtk::{LoadEvent, NetworkError, PolicyError, WebViewExt as _, gio};
    use wry::WebViewExtUnix as _;

    use super::LoadError;

    pub(super) fn key(raw: &wry::WebView) -> usize {
        raw.webview().as_ptr() as usize
    }

    /// WebKitGTK reports a failed navigation through `load-failed`, and an
    /// untrusted certificate first through `load-failed-with-tls-errors`.
    /// `load-failed` is handled so WebKit does not load its stock error page:
    /// that page's own `Started` would clear the record, and the other engines
    /// keep the previous document on screen too.
    pub(super) fn install(raw: &wry::WebView) {
        let webview = raw.webview();
        webview.connect_load_changed(|webview, event| {
            if event == LoadEvent::Started {
                super::clear(webview.as_ptr() as usize);
            }
        });
        webview.connect_load_failed(|webview, _, uri, error| {
            // A certificate failure was already recorded with its flags.
            if error.kind::<gio::TlsError>().is_none()
                && error.kind::<NetworkError>() != Some(NetworkError::Cancelled)
            {
                super::record(
                    webview.as_ptr() as usize,
                    LoadError {
                        url: uri.to_string(),
                        code: error_code(error),
                        message: error.message().to_string(),
                    },
                );
            }
            true
        });
        webview.connect_load_failed_with_tls_errors(|webview, uri, _, flags| {
            super::record(
                webview.as_ptr() as usize,
                LoadError {
                    url: uri.to_string(),
                    code: format!("WebKitTlsCertificate {flags:?}"),
                    message: "the server's TLS certificate is not trusted".into(),
                },
            );
            false
        });
    }

    /// GLib errors carry a domain and an enum value; name both so a refused
    /// connection reads differently from a policy cancellation.
    fn error_code(error: &glib::Error) -> String {
        fn named<T: glib::error::ErrorDomain + std::fmt::Debug>(
            error: &glib::Error,
        ) -> Option<String> {
            error.kind::<T>().map(|kind| format!("{kind:?}"))
        }
        let kind = named::<NetworkError>(error)
            .or_else(|| named::<PolicyError>(error))
            .or_else(|| named::<gio::IOErrorEnum>(error))
            .or_else(|| named::<gio::ResolverError>(error));
        let domain = error.domain();
        match kind {
            Some(kind) => format!("{} {kind}", domain.as_str()),
            None => domain.as_str().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LoadError;

    fn certificate_failure() -> LoadError {
        LoadError {
            url: "https://localhost:8443/".into(),
            code: "NSURLErrorDomain -1202".into(),
            message: "The certificate for this server is invalid.".into(),
        }
    }

    #[test]
    fn load_error_preserves_platform_diagnostics_in_text_and_json() {
        assert_eq!(
            certificate_failure().describe(),
            "navigation to https://localhost:8443/ failed: \
             The certificate for this server is invalid. (NSURLErrorDomain -1202)"
        );
        assert_eq!(
            certificate_failure().to_json(),
            serde_json::json!({
                "url": "https://localhost:8443/",
                "code": "NSURLErrorDomain -1202",
                "message": "The certificate for this server is invalid.",
            })
        );
    }
}

#[cfg(target_os = "android")]
pub(crate) fn get(raw: &super::android::RawWebView) -> Option<LoadError> {
    raw.load_error()
}
#[cfg(target_os = "android")]
pub(crate) fn forget(raw: &super::android::RawWebView) {
    raw.clear_load_error();
}

/// Android errors apply only to the main frame. Page-finished must not clear a
/// failure: Android also finishes the generated error document.
#[cfg(any(target_os = "android", test))]
pub(super) fn android_event(
    last: &mut Option<LoadError>,
    kind: i32,
    url: String,
    code: i32,
    message: String,
) {
    match kind {
        0 => *last = None,
        3..=6 => {
            let domain = match kind {
                3 => "WebView",
                4 => "HTTP",
                5 => "SSL",
                _ => "Android",
            };
            *last = Some(LoadError {
                url,
                code: format!("{domain} {code}"),
                message,
            });
        }
        _ => {}
    }
}
#[cfg(test)]
mod android_tests {
    use super::*;
    #[test]
    fn android_failure_survives_error_document_finish_until_next_navigation() {
        for (kind, code, expected) in [(3, -2, "WebView -2"), (4, 503, "HTTP 503"), (5, 3, "SSL 3")]
        {
            let mut last = None;
            android_event(
                &mut last,
                kind,
                "https://dead.invalid/".into(),
                code,
                "failure".into(),
            );
            android_event(
                &mut last,
                1,
                "https://dead.invalid/".into(),
                0,
                String::new(),
            );
            assert_eq!(
                last.as_ref().unwrap().to_json(),
                serde_json::json!({"url":"https://dead.invalid/", "code":expected, "message":"failure"})
            );
            android_event(
                &mut last,
                0,
                "https://example.com/".into(),
                0,
                String::new(),
            );
            assert_eq!(last, None);
        }
    }
}
