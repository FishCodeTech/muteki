use std::io::Write as _;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use gpui::{AnyWindowHandle, AsyncApp, Entity, px, size};
use tcode_ui::AppShell;

const STEP_DELAY: Duration = Duration::from_millis(20);
const RAPID_DELAY: Duration = Duration::from_millis(5);
const CREATION_TIMEOUT: Duration = Duration::from_secs(30);
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(120);
const DROP_DURING_CREATE_KEY: &str = "preview-smoke-drop-during-create";
#[cfg(target_os = "windows")]
const CREATION_IN_FLIGHT: &str = "preview creation is in flight";
const KEYS: [&str; 6] = [
    "preview-smoke-0",
    "preview-smoke-1",
    "preview-smoke-2",
    "preview-smoke-3",
    "preview-smoke-4",
    "preview-smoke-5",
];

fn log_line(message: &str) {
    eprintln!("{message}");
    let _ = std::io::stderr().flush();
    // Windows-subsystem builds have no console under a scheduled task, so the
    // on-device runner reads phases from this file instead of stderr.
    if let Ok(path) = std::env::var("TCODE_SMOKE_LOG")
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{message}");
        let _ = file.sync_all();
    }
}

pub struct Watchdog {
    phase: Arc<Mutex<&'static str>>,
    finished: Arc<AtomicBool>,
}

impl Watchdog {
    pub fn start() -> Self {
        let phase = Arc::new(Mutex::new("startup"));
        let finished = Arc::new(AtomicBool::new(false));
        let watchdog_phase = phase.clone();
        let watchdog_finished = finished.clone();
        std::thread::spawn(move || {
            std::thread::sleep(WATCHDOG_TIMEOUT);
            if !watchdog_finished.load(Ordering::SeqCst) {
                let phase = *watchdog_phase
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                log_line(&format!("preview-smoke: TIMEOUT phase {phase}"));
                std::process::exit(124);
            }
        });
        Self { phase, finished }
    }

    fn start_phase(&self, phase: &'static str) {
        *self.phase.lock().unwrap_or_else(|error| error.into_inner()) = phase;
        log_line(&format!("preview-smoke: phase {phase} start"));
    }

    fn finish_phase(&self, phase: &'static str) {
        log_line(&format!("preview-smoke: phase {phase} ok"));
    }

    fn pass(&self) {
        log_line("preview-smoke: PASS");
        self.finished.store(true, Ordering::SeqCst);
    }
}

async fn yield_for(cx: &AsyncApp, delay: Duration) {
    cx.background_executor().timer(delay).await;
}

fn create_once(
    shell: &Entity<AppShell>,
    window: AnyWindowHandle,
    key: &str,
    url: &str,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let lifecycle = shell.update(cx, |shell, cx| {
        shell
            .preview_lifecycle(cx)
            .expect("preview smoke runs on an attached window")
    });
    window
        .update(cx, |_, window, cx| {
            lifecycle.update(cx, |lifecycle, cx| {
                let availability = lifecycle.navigate(key, url, window, cx);
                lifecycle.set_visible(Some(key), cx);
                if availability.is_ready() {
                    Ok(())
                } else if let Some(message) = availability.pending_message() {
                    Err(message.into())
                } else if availability.is_unavailable() {
                    Err(lifecycle
                        .unavailable_error()
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            "the preview browser is unavailable on this machine \
                             (the system webview component could not be created: \
                             unknown creation failure)"
                                .into()
                        }))
                } else {
                    unreachable!("lifecycle availability was neither ready nor pending")
                }
            })
        })
        .expect("preview smoke window closed during webview creation")
}

fn creation_is_pending(error: &str) -> bool {
    error.starts_with("preview creation is ") || error.starts_with("preview is starting")
}

async fn create_and_wait(
    shell: &Entity<AppShell>,
    window: AnyWindowHandle,
    key: &str,
    url: &str,
    cx: &mut AsyncApp,
) {
    let deadline = Instant::now() + CREATION_TIMEOUT;
    loop {
        match create_once(shell, window, key, url, cx) {
            Ok(()) => return,
            Err(error) if creation_is_pending(&error) => {}
            Err(error) => panic!("native preview webview creation failed: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "native preview webview creation timed out for {key}"
        );
        yield_for(cx, RAPID_DELAY).await;
    }
}

/// Wait until wry's Windows future has returned `Pending` at least once. The
/// lifecycle adapter's smoke-only pause keeps that already-polled future alive
/// long enough for the harness to deterministically remove its generation.
#[cfg(target_os = "windows")]
async fn start_and_wait_until_in_flight(
    shell: &Entity<AppShell>,
    window: AnyWindowHandle,
    key: &str,
    url: &str,
    cx: &mut AsyncApp,
) {
    let deadline = Instant::now() + CREATION_TIMEOUT;
    loop {
        match create_once(shell, window, key, url, cx) {
            Err(error) if error == CREATION_IN_FLIGHT => return,
            Err(error) if creation_is_pending(&error) => {}
            Ok(()) => panic!("drop-during-create completed before teardown could race it"),
            Err(error) => panic!("native preview webview creation failed: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "native preview webview creation never entered flight"
        );
        yield_for(cx, RAPID_DELAY).await;
    }
}

pub async fn run(
    watchdog: Watchdog,
    shell: Entity<AppShell>,
    window: AnyWindowHandle,
    cx: &mut AsyncApp,
) {
    watchdog.start_phase("create-first");
    create_and_wait(&shell, window, KEYS[0], "about:blank", cx).await;
    yield_for(cx, STEP_DELAY).await;
    watchdog.finish_phase("create-first");

    watchdog.start_phase("show-hide-churn");
    for _ in 0..20 {
        shell.update(cx, |shell, cx| {
            shell
                .preview_lifecycle(cx)
                .expect("preview smoke runs on an attached window")
                .update(cx, |lifecycle, cx| lifecycle.set_visible(None, cx));
        });
        yield_for(cx, RAPID_DELAY).await;
        shell.update(cx, |shell, cx| {
            shell
                .preview_lifecycle(cx)
                .expect("preview smoke runs on an attached window")
                .update(cx, |lifecycle, cx| lifecycle.set_visible(Some(KEYS[0]), cx));
        });
        yield_for(cx, RAPID_DELAY).await;
    }
    watchdog.finish_phase("show-hide-churn");

    watchdog.start_phase("multi-create");
    for key in &KEYS[1..] {
        create_and_wait(&shell, window, key, "about:blank", cx).await;
        yield_for(cx, STEP_DELAY).await;
    }
    watchdog.finish_phase("multi-create");

    watchdog.start_phase("rapid-switch");
    for ix in 0..30 {
        shell.update(cx, |shell, cx| {
            shell
                .preview_lifecycle(cx)
                .expect("preview smoke runs on an attached window")
                .update(cx, |lifecycle, cx| {
                    lifecycle.set_visible(Some(KEYS[ix % KEYS.len()]), cx)
                });
        });
        yield_for(cx, RAPID_DELAY).await;
    }
    watchdog.finish_phase("rapid-switch");

    watchdog.start_phase("resize-churn");
    for (width, height) in [(1100., 720.), (940., 640.), (1280., 820.), (1000., 700.)] {
        window
            .update(cx, |_, window, _| {
                window.resize(size(px(width), px(height)))
            })
            .expect("preview smoke window closed during resize-churn");
        yield_for(cx, STEP_DELAY).await;
    }
    watchdog.finish_phase("resize-churn");

    watchdog.start_phase("navigate-all");
    for (ix, key) in KEYS.iter().enumerate() {
        let url = format!("about:blank#preview-smoke-{ix}");
        create_and_wait(&shell, window, key, &url, cx).await;
        yield_for(cx, STEP_DELAY).await;
    }
    watchdog.finish_phase("navigate-all");

    watchdog.start_phase("drop-one");
    shell.update(cx, |shell, cx| {
        shell
            .preview_lifecycle(cx)
            .expect("preview smoke runs on an attached window")
            .update(cx, |lifecycle, cx| {
                lifecycle.set_visible(Some(KEYS[1]), cx);
                lifecycle.drop_view(KEYS[0], cx);
            });
    });
    yield_for(cx, STEP_DELAY).await;
    watchdog.finish_phase("drop-one");

    watchdog.start_phase("drop-during-create");
    #[cfg(target_os = "windows")]
    start_and_wait_until_in_flight(
        &shell,
        window,
        DROP_DURING_CREATE_KEY,
        "about:blank#drop-during-create",
        cx,
    )
    .await;
    #[cfg(not(target_os = "windows"))]
    create_and_wait(
        &shell,
        window,
        DROP_DURING_CREATE_KEY,
        "about:blank#drop-during-create",
        cx,
    )
    .await;
    // Model a conversation switch that removes the old conversation's native
    // child while its Windows creation future is still alive. Keep the primary
    // window open so a premature process exit is observable as a missing phase.
    shell.update(cx, |shell, cx| {
        shell
            .preview_lifecycle(cx)
            .expect("preview smoke runs on an attached window")
            .update(cx, |lifecycle, cx| {
                lifecycle.set_visible(Some(KEYS[1]), cx);
                lifecycle.drop_view(DROP_DURING_CREATE_KEY, cx);
            });
    });
    yield_for(cx, STEP_DELAY).await;
    watchdog.finish_phase("drop-during-create");

    watchdog.start_phase("recreate-after-inflight-drop");
    // Reuse the exact key. Its replacement generation is serialized behind the
    // cancelled raw build, so reaching Ready proves that stale completion was
    // discarded and did not dead-end or overwrite the new slot.
    create_and_wait(
        &shell,
        window,
        DROP_DURING_CREATE_KEY,
        "about:blank#replacement-after-drop",
        cx,
    )
    .await;
    shell.update(cx, |shell, cx| {
        shell
            .preview_lifecycle(cx)
            .expect("preview smoke runs on an attached window")
            .update(cx, |lifecycle, cx| {
                lifecycle.drop_view(DROP_DURING_CREATE_KEY, cx)
            });
    });
    watchdog.finish_phase("recreate-after-inflight-drop");

    watchdog.start_phase("quit");
    watchdog.finish_phase("quit");
    watchdog.pass();
    cx.update(|cx| cx.quit());
}
