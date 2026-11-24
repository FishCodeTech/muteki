//! Embedded report renderer. This view has a separate data store and receives
//! report content through its owning window, never through an HTTP request.
use super::*;
use crate::policy::Outcome;

const RENDERER_PATH: &str = "/native-report-preview";

struct ReportView {
    document: String,
    surface: String,
    web: Retained<AnyObject>,
    _delegate: Retained<ReportDelegate>,
    payload: Value,
    ready: bool,
    presented: bool,
    shown: bool,
}

thread_local! {
    static REPORTS: RefCell<HashMap<String, ReportView>> = RefCell::new(HashMap::new());
}

fn evaluate(web: &AnyObject, function: &str, value: &Value) {
    let script = ns(&format!("window.{function}?.({value});"));
    unsafe {
        let _: () = msg_send![web, evaluateJavaScript: &*script, completionHandler: std::ptr::null::<AnyObject>()];
    }
}

fn send_preview(window: &str, report: &ReportView, event: Value) {
    notify(Notice::ReportPreview(
        window.to_owned(),
        report.document.clone(),
        json!({"surfaceId": report.surface, "event": event}),
    ));
}

fn refresh_visibility(report: &ReportView) {
    unsafe {
        let _: () = msg_send![&report.web, setHidden: Bool::new(!report.shown || !report.presented)];
    }
}

pub(super) fn dispose(window: &str) {
    if let Some(report) = REPORTS.with(|reports| reports.borrow_mut().remove(window)) {
        unsafe {
            let _: () = msg_send![&report.web, stopLoading];
            let _: () = msg_send![&report.web, removeFromSuperview];
        }
    }
}

fn write_pasteboard_text(text: &str) {
    unsafe {
        let pasteboard: Option<Retained<AnyObject>> = msg_send![class(c"NSPasteboard"), generalPasteboard];
        if let Some(pasteboard) = pasteboard {
            let _: isize = msg_send![&pasteboard, clearContents];
            let _: Bool = msg_send![&pasteboard, setString: &*ns(text), forType: &*ns("public.utf8-plain-text")];
        }
    }
}

fn failed(window: &str, web: &AnyObject) {
    let dead = REPORTS.with(|reports| {
        let mut reports = reports.borrow_mut();
        if let Some(report) = reports.get(window).filter(|report| same(&report.web, web)) {
            if !report.surface.is_empty() {
                send_preview(window, report, json!({"type": "error"}));
            }
            return reports.remove(window);
        }
        None
    });
    if let Some(report) = dead {
        unsafe {
            let _: () = msg_send![&report.web, stopLoading];
            let _: () = msg_send![&report.web, removeFromSuperview];
        }
    }
}

struct ReportOwner {
    window: String,
    url: String,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ReportOwner]
    #[name = "MutekiReportPreviewDelegate"]
    struct ReportDelegate;

    unsafe impl NSObjectProtocol for ReportDelegate {}

    impl ReportDelegate {
        #[unsafe(method(_webView:getContextMenuFromProposedMenu:forElement:userInfo:completionHandler:))]
        fn context_menu(&self, _web: &AnyObject, menu: &AnyObject, _element: &AnyObject, _user_info: *mut AnyObject, done: &Block<dyn Fn(*mut AnyObject)>) {
            let filtered = if filter_context_menu(menu, true) {
                menu as *const AnyObject as *mut AnyObject
            } else {
                std::ptr::null_mut()
            };
            done.call((filtered,));
        }

        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide(&self, _web: &AnyObject, action: &AnyObject, complete: &Block<dyn Fn(isize)>) {
            let allow = unsafe {
                let frame: Option<Retained<AnyObject>> = msg_send![action, targetFrame];
                let request: Retained<AnyObject> = msg_send![action, request];
                frame.as_ref().is_some_and(|frame| msg_send![&**frame, isMainFrame])
                    && url_text(msg_send![&request, URL]) == self.ivars().url
            };
            complete.call((isize::from(allow),));
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn load_failed(&self, web: &AnyObject, _navigation: *mut AnyObject, _error: &AnyObject) {
            failed(&self.ivars().window, web);
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn provisional_failed(&self, web: &AnyObject, _navigation: *mut AnyObject, _error: &AnyObject) {
            failed(&self.ivars().window, web);
        }

        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn process_failed(&self, web: &AnyObject) {
            failed(&self.ivars().window, web);
        }

        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn receive(&self, _controller: &AnyObject, message: &AnyObject) {
            let sender = unsafe {
                let frame: Option<Retained<AnyObject>> = msg_send![message, frameInfo];
                let Some(frame) = frame else { return };
                let main: bool = msg_send![&frame, isMainFrame];
                let request: Retained<AnyObject> = msg_send![&frame, request];
                if !main || url_text(msg_send![&request, URL]) != self.ivars().url {
                    return;
                }
                let web: Option<Retained<AnyObject>> = msg_send![message, webView];
                let Some(web) = web else { return };
                let body: Option<Retained<AnyObject>> = msg_send![message, body];
                let Some(body) = body else { return };
                let is_string: bool = msg_send![&body, isKindOfClass: class(c"NSString")];
                if !is_string { return; }
                let message: Retained<NSString> = Retained::cast_unchecked(body);
                (web, message.to_string())
            };
            let Ok(event) = serde_json::from_str::<Value>(&sender.1) else { return };
            let window = &self.ivars().window;
            REPORTS.with(|reports| {
                let mut reports = reports.borrow_mut();
                let Some(report) = reports.get_mut(window).filter(|r| same(&r.web, &sender.0)) else { return };
                match event["type"].as_str() {
                    Some("ready") => {
                        report.ready = true;
                        if report.payload.is_object() {
                            evaluate(&report.web, "__mutekiReceiveReport", &report.payload);
                        }
                    }
                    Some("rendered") if !report.surface.is_empty() && report.payload["revision"] == event["revision"] => {
                        report.presented = true;
                        refresh_visibility(report);
                        send_preview(window, report, event);
                    }
                    Some("scroll") if report.shown && report.presented => {
                        if event["ratio"].as_f64().is_some_and(|ratio| (0.0..=1.0).contains(&ratio)) {
                            send_preview(window, report, event);
                        }
                    }
                    Some("link") if report.shown && report.presented && event["href"].is_string() => {
                        send_preview(window, report, event);
                    }
                    Some("image") if report.shown && report.presented && event["src"].is_string() && event["name"].is_string() => {
                        send_preview(window, report, event);
                    }
                    Some("copy") if report.shown && report.presented => {
                        if let Some(text) = event["text"].as_str() {
                            write_pasteboard_text(text);
                        }
                    }
                    _ => {}
                }
            });
        }
    }
);

fn frame_of(parent: &AnyObject, host: &AnyObject, rect: &Value) -> Option<NSRect> {
    let number = |key: &str| rect[key].as_f64().filter(|value| value.is_finite());
    let (x, y, width, height) = (number("x")?, number("y")?, number("width")?, number("height")?);
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let bounds: NSRect = unsafe { msg_send![parent, bounds] };
    let flipped: bool = unsafe { msg_send![parent, isFlipped] };
    let frame = NSRect::new(
        NSPoint::new(x, if flipped { y } else { bounds.size.height - y - height }),
        NSSize::new(width, height),
    );
    Some(unsafe { msg_send![parent, convertRect: frame, toView: host] })
}

fn create_view(
    window: &str,
    document: &str,
    surface: &str,
    parent: &AnyObject,
    host: &AnyObject,
    frame: NSRect,
    payload: Value,
    shown: bool,
) -> Outcome {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| BridgeError::new("desktop.report_preview_thread", "报告预览须在主线程创建。"))?;
    let url = format!("{}{RENDERER_PATH}", crate::bridge::context().origin.trim_end_matches('/'));
    let request: Retained<AnyObject> = unsafe {
        let url: Option<Retained<AnyObject>> = msg_send![class(c"NSURL"), URLWithString: &*ns(&url)];
        let url = url.ok_or_else(|| BridgeError::new("desktop.report_preview_url", "报告渲染地址无效。"))?;
        msg_send![class(c"NSURLRequest"), requestWithURL: &*url]
    };

    dispose(window);
    unsafe {
        let config: Retained<AnyObject> = msg_send![class(c"WKWebViewConfiguration"), new];
        let store: Retained<AnyObject> = msg_send![class(c"WKWebsiteDataStore"), nonPersistentDataStore];
        let _: () = msg_send![&config, setWebsiteDataStore: &*store];
        let delegate_alloc = mtm.alloc::<ReportDelegate>().set_ivars(ReportOwner { window: window.to_owned(), url: url.clone() });
        let delegate: Retained<ReportDelegate> = msg_send![super(delegate_alloc), init];
        let controller: Retained<AnyObject> = msg_send![&config, userContentController];
        let _: () = msg_send![&controller, addScriptMessageHandler: &*delegate, name: &*ns("mutekiReport")];
        let web_alloc: Allocated<AnyObject> = msg_send![class(c"WKWebView"), alloc];
        let web: Retained<AnyObject> = msg_send![web_alloc, initWithFrame: frame, configuration: &*config];
        configure_web_view(&web);
        let _: () = msg_send![&web, setUIDelegate: &*delegate];
        let _: () = msg_send![&web, setNavigationDelegate: &*delegate];
        let _: () = msg_send![&web, setHidden: Bool::YES];
        let _: () = msg_send![host, addSubview: &*web, positioned: 1isize, relativeTo: parent];
        REPORTS.with(|reports| reports.borrow_mut().insert(window.to_owned(), ReportView {
            document: document.to_owned(),
            surface: surface.to_owned(),
            web: web.clone(),
            _delegate: delegate,
            payload,
            ready: false,
            presented: false,
            shown,
        }));
        let _: Option<Retained<AnyObject>> = msg_send![&web, loadRequest: &*request];
    }
    Ok(Value::Null)
}

pub(super) fn warm(window: &str, document: &str) {
    let already = REPORTS.with(|reports| {
        reports.borrow().get(window).is_some_and(|r| r.document == document)
    });
    if already {
        return;
    }
    let Some(parent) = workbench_web_view(window) else { return };
    let host: Option<Retained<AnyObject>> = unsafe { msg_send![&parent, superview] };
    let Some(host) = host else { return };
    let bounds: NSRect = unsafe { msg_send![&parent, bounds] };
    let frame: NSRect = unsafe { msg_send![&parent, convertRect: bounds, toView: &*host] };
    let frame = if frame.size.width > 0.0 && frame.size.height > 0.0 {
        frame
    } else {
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(640.0, 720.0))
    };
    let _ = create_view(window, document, "", &parent, &host, frame, Value::Null, false);
}

fn present(window: &str, document: &str, surface: &str, args: &Value) -> Outcome {
    let parent = workbench_web_view(window)
        .ok_or_else(|| BridgeError::new("desktop.window_closed", "工作台窗口已关闭。"))?;
    let host: Option<Retained<AnyObject>> = unsafe { msg_send![&parent, superview] };
    let host = host.ok_or_else(|| BridgeError::new("desktop.report_preview_host", "报告预览所在窗口尚未就绪。"))?;
    let frame = frame_of(&parent, &host, &args["rect"])
        .ok_or_else(|| BridgeError::new("desktop.report_preview_bounds", "报告预览区域无效。"))?;
    let payload = args.get("payload").filter(|value| value.is_object());

    enum ReuseOutcome {
        Reused,
        MissingPayload,
        NotFound,
    }

    let outcome = REPORTS.with(|reports| {
        let mut reports = reports.borrow_mut();
        let Some(report) = reports.get_mut(window).filter(|r| r.document == document) else {
            return ReuseOutcome::NotFound;
        };
        if report.surface == surface {
            report.shown = true;
            unsafe { let _: () = msg_send![&report.web, setFrame: frame]; }
            refresh_visibility(report);
            if let Some(payload) = payload {
                report.payload = payload.clone();
                if report.ready {
                    evaluate(&report.web, "__mutekiReceiveReport", payload);
                }
            }
            return ReuseOutcome::Reused;
        }
        let Some(payload) = payload else {
            return ReuseOutcome::MissingPayload;
        };
        report.surface = surface.to_owned();
        report.shown = true;
        report.presented = false;
        unsafe { let _: () = msg_send![&report.web, setFrame: frame]; }
        refresh_visibility(report);
        report.payload = payload.clone();
        if report.ready {
            evaluate(&report.web, "__mutekiReceiveReport", payload);
        }
        ReuseOutcome::Reused
    });
    match outcome {
        ReuseOutcome::Reused => return Ok(Value::Null),
        ReuseOutcome::MissingPayload => {
            return Err(BridgeError::new("desktop.report_preview_content", "报告预览未收到内容。"));
        }
        ReuseOutcome::NotFound => {}
    }

    let payload = payload.ok_or_else(|| BridgeError::new("desktop.report_preview_content", "报告预览未收到内容。"))?;
    create_view(window, document, surface, &parent, &host, frame, payload.clone(), true)
}

pub(super) fn command(window: &str, document: &str, method: &str, args: &Value) -> Outcome {
    let surface = args["surfaceId"].as_str().filter(|surface| !surface.is_empty())
        .ok_or_else(|| BridgeError::new("desktop.report_preview_surface", "报告预览缺少页面标识。"))?;
    if method == "reportPreview.present" {
        return present(window, document, surface, args);
    }

    REPORTS.with(|reports| {
        let mut reports = reports.borrow_mut();
        let Some(report) = reports.get_mut(window).filter(|r| r.document == document && r.surface == surface) else {
            return;
        };
        match method {
            "reportPreview.close" => {
                report.surface.clear();
                report.payload = Value::Null;
                report.presented = false;
                report.shown = false;
                refresh_visibility(report);
                if report.ready {
                    evaluate(&report.web, "__mutekiClearReport", &Value::Null);
                }
            }
            "reportPreview.hide" => {
                report.shown = false;
                refresh_visibility(report);
                if report.ready && args["resetScroll"] == true {
                    evaluate(&report.web, "__mutekiScrollReport", &json!(0));
                }
            }
            "reportPreview.scroll" if report.ready => {
                if args["ratio"].as_f64().is_some_and(|ratio| (0.0..=1.0).contains(&ratio)) {
                    evaluate(&report.web, "__mutekiScrollReport", &args["ratio"]);
                }
            }
            "reportPreview.scrollBy" if report.shown && report.presented => {
                if args["delta"].as_f64().is_some_and(|delta| delta.is_finite()) {
                    evaluate(&report.web, "__mutekiWheelReport", &args["delta"]);
                }
            }
            "reportPreview.focus" if report.shown && report.presented => {
                unsafe {
                    let window: Option<Retained<AnyObject>> = msg_send![&report.web, window];
                    if let Some(window) = window {
                        let _: Bool = msg_send![&window, makeFirstResponder: &*report.web];
                        evaluate(&report.web, "__mutekiFocusReport", &Value::Null);
                    }
                }
            }
            "reportPreview.pointer" if report.shown && report.presented => {
                let pointer = &args["pointer"];
                if pointer["x"].as_f64().is_some_and(|x| x.is_finite())
                    && pointer["y"].as_f64().is_some_and(|y| y.is_finite())
                    && matches!(pointer["phase"].as_str(), Some("start" | "move" | "finish" | "click"))
                {
                    evaluate(&report.web, "__mutekiPointerReport", pointer);
                }
            }
            _ => {}
        }
    });
    Ok(Value::Null)
}
