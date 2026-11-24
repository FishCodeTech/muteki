use std::time::{Duration, Instant};

use super::super::RootInfo;
use super::ax;

pub(super) struct FocusGuard {
    previous_pid: Option<u32>,
    ready: bool,
}

impl FocusGuard {
    pub(super) fn acquire(root: &RootInfo) -> Self {
        let Some(previous_pid) = ax::frontmost_application_pid() else {
            log::debug!(
                "could not determine the frontmost macOS application; skipping focus guard"
            );
            return Self::noop();
        };
        if previous_pid == root.pid {
            return Self {
                previous_pid: None,
                ready: true,
            };
        }
        if !ax::activate_application(root.pid) {
            log::debug!(
                "could not activate target macOS application pid {}; skipping focus guard",
                root.pid
            );
            return Self::noop();
        }
        if let Err(error) = ax::raise_window(root) {
            log::debug!("could not raise target macOS window: {error}; skipping focus guard");
            Self::restore_after_failed_acquire(previous_pid);
            return Self::noop();
        }

        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match ax::frontmost_application_pid() {
                Some(pid) if pid == root.pid => {
                    return Self {
                        previous_pid: Some(previous_pid),
                        ready: true,
                    };
                }
                Some(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Some(_) => {
                    log::debug!(
                        "target macOS application pid {} did not become frontmost within 500ms; skipping focus guard",
                        root.pid
                    );
                    Self::restore_after_failed_acquire(previous_pid);
                    return Self::noop();
                }
                None => {
                    log::debug!(
                        "could not poll the frontmost macOS application; skipping focus guard"
                    );
                    Self::restore_after_failed_acquire(previous_pid);
                    return Self::noop();
                }
            }
        }
    }

    fn noop() -> Self {
        Self {
            previous_pid: None,
            ready: false,
        }
    }

    pub(super) fn is_ready(&self) -> bool {
        self.ready
    }

    fn restore_after_failed_acquire(previous_pid: u32) {
        if !ax::activate_application(previous_pid) {
            log::debug!(
                "could not restore macOS application pid {previous_pid} after focus acquisition failed"
            );
        }
    }
}

impl Drop for FocusGuard {
    fn drop(&mut self) {
        if let Some(previous_pid) = self.previous_pid
            && !ax::activate_application(previous_pid)
        {
            log::debug!("could not restore previous macOS application pid {previous_pid}");
        }
    }
}
