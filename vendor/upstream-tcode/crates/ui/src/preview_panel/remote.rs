//! The WebKit slot's native navigation boundary. The transport owns URL identity;
//! this adapter preserves WebKit requests and resumes them on GPUI's foreground.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gpui::{Context, Entity, Task};
use gpui_webview::WebView;
use objc2::{
    DefinedClass as _, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject, Sel},
};
use objc2_foundation::{
    NSMutableCopying as _, NSObject, NSObjectProtocol, NSString, NSURL, NSURLRequest,
};
#[cfg(target_os = "macos")]
use objc2_web_kit::WKWebView;
use objc2_web_kit::{WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate};
use tcode_traverse::preview::PreviewRoutes;
#[cfg(target_os = "ios")]
use wry::WebViewExtIOS as _;
#[cfg(target_os = "macos")]
use wry::WebViewExtMacOS as _;

use super::lifecycle::BrowserLifecycle;

/// objc2-web-kit declares WKWebView, and the delegate methods that take one,
/// for macOS only.
#[cfg(target_os = "ios")]
type WKWebView = AnyObject;

pub(super) struct RemoteBrowser {
    pub(super) routes: Rc<RefCell<PreviewRoutes>>,
    generation: Rc<Cell<u64>>,
    _delegate: Retained<NavigationDelegate>,
    _events: Task<()>,
}

struct Navigation {
    generation: u64,
    request: Option<Retained<NSURLRequest>>,
}

pub struct DelegateState {
    original: Retained<ProtocolObject<dyn WKNavigationDelegate>>,
    routes: std::rc::Weak<RefCell<PreviewRoutes>>,
    generation: Rc<Cell<u64>>,
    events: async_channel::Sender<Navigation>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "TcodePreviewNavigationDelegate"]
    #[thread_kind = MainThreadOnly]
    #[ivars = DelegateState]
    struct NavigationDelegate;

    unsafe impl NSObjectProtocol for NavigationDelegate {
        #[unsafe(method(respondsToSelector:))]
        fn responds_to(&self, selector: Sel) -> bool {
            // WebKit caches optional delegate methods at installation time.
            (unsafe { msg_send![super(self), respondsToSelector: selector] })
                || self.ivars().original.respondsToSelector(selector)
        }
    }

    impl NavigationDelegate {
        #[unsafe(method(forwardingTargetForSelector:))]
        fn forward_to(&self, _selector: Sel) -> *const AnyObject {
            Retained::as_ptr(&self.ivars().original).cast()
        }
    }

    unsafe impl WKNavigationDelegate for NavigationDelegate {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn policy(&self, webview: &WKWebView, action: &WKNavigationAction,
            handler: &block2::Block<dyn Fn(WKNavigationActionPolicy)>) {
            // lb-wry's URL-only callback loses frame identity and request bytes.
            // Keep its policy for subframes and pass the full request for a
            // top-level redirect, including method/body/site authorization.
            let main_frame = unsafe { action.targetFrame() }.is_some_and(|frame| unsafe { frame.isMainFrame() });
            let request = unsafe { action.request() };
            let actual = request.URL().and_then(|url| url.absoluteString()).map(|url| url.to_string());
            if main_frame && let Some(actual) = actual && let Some(routes) = self.ivars().routes.upgrade() {
                let mapped = routes.borrow_mut().navigation(&actual);
                let generation = self.ivars().generation.get().wrapping_add(1);
                self.ivars().generation.set(generation);
                match mapped {
                    Ok(mapped) if mapped != actual => {
                        if let Some(url) = NSURL::URLWithString(&NSString::from_str(&mapped)) {
                            let request = request.mutableCopy();
                            request.setURL(Some(&url));
                            let _ = self.ivars().events.try_send(Navigation { generation, request: Some(request.into_super()) });
                        }
                        handler.call((WKNavigationActionPolicy::Cancel,));
                        return;
                    }
                    Err(_) => {
                        let _ = self.ivars().events.try_send(Navigation { generation, request: None });
                        handler.call((WKNavigationActionPolicy::Cancel,));
                        return;
                    }
                    Ok(_) => {
                        let _ = self.ivars().events.try_send(Navigation { generation, request: None });
                    }
                }
            }
            let original: &ProtocolObject<dyn WKNavigationDelegate> = &self.ivars().original;
            unsafe { msg_send![original, webView: webview, decidePolicyForNavigationAction: action, decisionHandler: handler] }
        }
    }
);

impl RemoteBrowser {
    pub(super) fn install(
        view: &Entity<WebView>,
        endpoint: tcode_traverse::preview::PreviewEndpoint,
        cx: &mut Context<BrowserLifecycle>,
    ) -> Self {
        let routes = Rc::new(RefCell::new(PreviewRoutes::new(endpoint)));
        let generation = Rc::new(Cell::new(0_u64));
        let (events, received) = async_channel::unbounded::<Navigation>();
        let native = view.read(cx).raw().webview();
        let original =
            unsafe { native.navigationDelegate() }.expect("wry installs a navigation delegate");
        let delegate = NavigationDelegate::alloc(native.mtm()).set_ivars(DelegateState {
            original,
            routes: Rc::downgrade(&routes),
            generation: generation.clone(),
            events,
        });
        let delegate: Retained<NavigationDelegate> = unsafe { msg_send![super(delegate), init] };
        unsafe {
            native.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        }
        let errors = routes.borrow().changes();
        let weak_view = view.downgrade();
        let live_generation = generation.clone();
        let task = cx.spawn(async move |this, cx| {
            while let Ok(navigation) =
                futures_lite::future::race(async { received.recv().await.map(Some) }, async {
                    errors.recv().await.map(|_| None)
                })
                .await
            {
                let result = this.update(cx, |_, cx| {
                    let Some(navigation) = navigation else {
                        cx.notify();
                        return;
                    };
                    if live_generation.get() != navigation.generation {
                        return;
                    }
                    if let Some(view) = weak_view.upgrade()
                        && let Some(request) = navigation.request
                    {
                        let raw = view.read(cx).raw();
                        super::load_error::forget(raw);
                        unsafe {
                            raw.webview().loadRequest(&request);
                        }
                    }
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
            }
        });
        Self {
            routes,
            generation,
            _delegate: delegate,
            _events: task,
        }
    }

    pub(super) fn navigate(&self, intent: &str) -> Result<String, String> {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.routes.borrow_mut().navigate(intent)
    }
}
