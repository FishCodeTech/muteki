//! In-process `tcode_computer_use` MCP server: desktop automation for every
//! MCP-capable provider (accessibility-tree observation, state-scoped refs,
//! transactional actions). The tool design was informed by
//! <https://github.com/injaneity/pi-computer-use>.
//!
//! Served over the shared loopback streamable-HTTP host with a distinct bearer
//! token, registered per session when computer use is enabled. It runs inside
//! Tcode so macOS permissions apply to the running app; there is no helper
//! app. macOS uses AX/CGEvent and Windows adapts the `uiautomation` crate;
//! other platforms report that computer use is unsupported rather than timing
//! out. Browser automation is the separate `tcode_preview` server: this one
//! exposes desktop windows, not CDP browser roots.
//!
//! # Tool surface
//!
//! `find_roots` (ranked window roots `@rN`), `observe_ui` (folded outline with
//! element refs `@eN`, a `state_id` and, per image mode, a screenshot),
//! `search_ui` / `expand_ui` / `inspect_ui` (queries over the stored outline
//! that never touch the live UI), `act_ui` (a transaction of `press`, `click`,
//! `set_text`, `type_text`, `keypress`, `scroll`, `drag`, `move_mouse` against a
//! `state_id`, optionally with an `expect` postcondition), `read_text` (page
//! through long text) and `wait_for` (a text/role condition).
//!
//! # Contract
//!
//! - Every `@e` ref belongs to the `state_id` that produced it. Observations
//!   are immutable and kept in a bounded LRU (default 8); acting from an
//!   evicted or stale state is rejected and the model must observe again.
//! - `act_ui` reports `worked` / `didnt` / `unknown` per step, stops at the
//!   first failure, and never treats event delivery alone as semantic success
//!   when an `expect` was given. Each step reports its `delivery` (`ax`,
//!   `background_pid`, `foreground_hid`, `none`) and the transaction its
//!   `activation` (`none`, `background`, `foreground`).
//! - Model-visible text is capped; oversized results return a preview plus a
//!   continuation ref for `read_text`.
//! - A window of at least 20,000 square points whose outline exposes fewer
//!   than three titled, valued or described descendants (the root title does
//!   not count) is reported `text_sparse`. Image mode `auto` then attaches one
//!   downscaled window screenshot when capture permission exists; `always` and
//!   `never` do what they say. The fallback is OCR-free and synthesizes no
//!   nodes.
//! - macOS tries AX actions before synthesizing input, posts events to the
//!   target process without stealing focus, and relies on optional private
//!   routing APIs, so delivery never implies the application changed.
//!   `allow_foreground_fallback` (default off) lets only `type_text` and
//!   `keypress` retry through foreground activation; pointer actions never do.
//!   `show_agent_cursor` (default on) drives the action overlay in
//!   `backend::macos::overlay`, visible only while no other application
//!   window covers the action point on the target window.
//! - macOS needs Accessibility (`AXIsProcessTrusted`) and, for screenshots,
//!   Screen Recording (`CGPreflightScreenCaptureAccess`); `permissions`
//!   checks and requests them, and Screen Recording grants that need a
//!   relaunch are carried across it by `tcode_services::relaunch`. Windows has
//!   no such gate and reports both as available.
//!
//! Compilation establishes none of the native permission or input-delivery
//! behaviour: exercise backend changes on the target platform, and run the
//! ignored `macos_overlay` test (`cargo test -p computer-use-mcp --test
//! macos_overlay -- --ignored`) only when a desktop slot is free.

pub mod backend;
pub mod config;
mod feedback;
pub mod outline;
pub mod permissions;
pub mod state;
pub mod tools;

/// Return the frontmost application pid on macOS, or `None` elsewhere.
pub use backend::frontmost_pid;

/// A running computer-use MCP server and the bearer token required to access it.
pub struct ComputerUseMcpServer {
    /// Streamable-HTTP endpoint, e.g. `http://127.0.0.1:53211/computer-use`.
    pub url: String,
    /// Per-session tokens and feedback cancellation controls.
    pub tokens: TokenRegistry,
}

/// Diagnostic entry for `tcode --cu-smoke`: exercises the platform backend
/// (root enumeration + observing every root) against the live desktop and
/// returns a human-readable summary. Used to validate a backend on real
/// hardware without wiring an MCP client. Read-only: it never performs an
/// action, so it cannot disturb the live session. Never panics — every failure
/// becomes a line in the summary.
pub fn smoke() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let roots = match backend::list_roots(&backend::RootFilters::default()) {
        Ok(roots) => roots,
        Err(error) => return format!("cu-smoke: list_roots failed: {error}\n"),
    };
    let _ = writeln!(out, "cu-smoke: {} root(s)", roots.len());
    for root in roots.iter().take(20) {
        let observed = backend::observe(
            root,
            backend::ObserveRequest {
                semantic: true,
                capture: backend::CapturePolicy::Never,
            },
        );
        let detail = match observed {
            Ok(observation) => format!(
                "{} node(s), text_sparse={}",
                observation.tree.node_count(),
                observation.text_sparse
            ),
            Err(error) => format!("observe failed: {error}"),
        };
        let _ = writeln!(
            out,
            "  [{}] {} pid={} kind={} frame={}x{} title={:?} -> {detail}",
            root.ref_id,
            root.app_name,
            root.pid,
            root.kind,
            root.frame.w as i64,
            root.frame.h as i64,
            root.title,
        );
    }
    let _ = writeln!(out, "cu-smoke: PASS");
    out
}

/// Register the authenticated computer-use route on the shared MCP host.
pub fn start(host: &mut mcp_host::Host) -> ComputerUseMcpServer {
    let url = host.url("/computer-use");
    let sessions = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let feedback = sessions.clone();
    let services = mcp_host::TokenRegistry::new(move |scope| {
        let mut sessions = feedback.lock().unwrap();
        let session = sessions
            .get(&scope)
            .and_then(std::sync::Weak::upgrade)
            .unwrap_or_else(feedback::FeedbackSession::new);
        sessions.insert(scope, std::sync::Arc::downgrade(&session));
        tools::service(session)
    });
    let tokens = TokenRegistry { services, sessions };
    host.mount(mcp_host::route("/computer-use", &tokens.services));

    log::info!("computer-use-mcp: serving at {url}");
    ComputerUseMcpServer { url, tokens }
}

/// Uses the shared authenticated registry; weak feedback controls do not keep a
/// mounted service alive after its actual route and handlers have been dropped.
#[derive(Clone)]
pub struct TokenRegistry {
    services: mcp_host::TokenRegistry<tools::Service>,
    sessions: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, std::sync::Weak<feedback::FeedbackSession>>,
        >,
    >,
}

impl TokenRegistry {
    pub fn register(&self, session_id: &str) -> String {
        self.services.register(session_id)
    }
    pub fn cancel(&self, session_id: &str) {
        if let Some(session) = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .and_then(std::sync::Weak::upgrade)
        {
            session.cancel();
        }
    }
    pub fn revoke(&self, session_id: &str, token: &str) {
        if let Some(session) = self
            .sessions
            .lock()
            .unwrap()
            .remove(session_id)
            .and_then(|session| session.upgrade())
        {
            session.stop();
        }
        self.services.revoke(token);
    }
}

#[cfg(test)]
mod feedback_lifecycle_tests {
    use super::*;

    #[test]
    fn mounted_services_own_feedback_and_registrations_cancel_independently() {
        let mut host = mcp_host::Host::bind().unwrap();
        let server = start(&mut host);
        let first = server.tokens.register("first");
        let second = server.tokens.register("second");
        assert!(
            first != second,
            "sessions need distinct authorization scopes"
        );
        let session = |id: &str| {
            server.tokens.sessions.lock().unwrap()[id]
                .upgrade()
                .unwrap()
        };
        let first_run = session("first").begin(None);
        let second_run = session("second").begin(None);
        server.tokens.cancel("first");
        assert!(!first_run.is_current());
        assert!(second_run.is_current());
        let retained_handler = session("first");
        let next_turn = retained_handler.begin(None);
        assert!(next_turn.is_current(), "Stop does not disable future turns");
        server.tokens.revoke("first", &first);
        assert!(!next_turn.is_current());
        assert!(
            !retained_handler.begin(None).is_current(),
            "a retained handler cannot restart feedback after revocation"
        );
        assert!(second_run.is_current());
        drop(server);
        assert!(
            second_run.is_current(),
            "registration metadata does not own mounted service lifetime"
        );
        drop(host);
        assert!(
            !second_run.is_current(),
            "last mounted service drop invalidates delayed publications"
        );
    }

    #[test]
    fn mcp_cancellation_invalidates_pending_feedback_before_the_handler_resumes() {
        let session = feedback::FeedbackSession::new();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let mut success = session.begin(None);
        let published_success = success.ticket();
        success.complete();
        drop(success);
        assert!(
            published_success.is_current(),
            "successful actions retain their bounded tail"
        );
        let cancelled = session.begin(Some(cancellation.clone()));
        let pending = cancelled.ticket();
        cancellation.cancel();
        assert!(!pending.is_current());
        assert!(
            published_success.is_current(),
            "request cancellation does not invalidate another request"
        );
    }
}
