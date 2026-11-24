//! Application heartbeat policy for transports whose platform hides WebSocket Pong.

pub const NATIVE_IDLE_MS: u64 = 10_000;
pub const BROWSER_IDLE_MS: u64 = 15_000;
pub const LIVENESS_REPLY_MS: u64 = 20_000;
pub const QUERY_TIMEOUT_MS: u64 = 15_000;
pub const COMMAND_TIMEOUT_MS: u64 = 30_000;

pub struct Heartbeat {
    idle_ms: u64,
    deadline_ms: u64,
    awaiting_reply: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Tick {
    Wait(u64),
    Ping,
    Lost,
}

impl Heartbeat {
    pub fn new(now_ms: u64) -> Self {
        Self::with_idle(now_ms, BROWSER_IDLE_MS)
    }

    /// A heartbeat that probes after `idle_ms` of silence instead of the
    /// browser default.
    pub fn with_idle(now_ms: u64, idle_ms: u64) -> Self {
        Self {
            idle_ms,
            deadline_ms: now_ms.saturating_add(idle_ms),
            awaiting_reply: false,
        }
    }

    pub fn received(&mut self, now_ms: u64) {
        *self = Self::with_idle(now_ms, self.idle_ms);
    }

    pub fn tick(&mut self, now_ms: u64) -> Tick {
        if now_ms < self.deadline_ms {
            Tick::Wait(self.deadline_ms - now_ms)
        } else if self.awaiting_reply {
            Tick::Lost
        } else {
            self.awaiting_reply = true;
            self.deadline_ms = now_ms.saturating_add(LIVENESS_REPLY_MS);
            Tick::Ping
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_browser_probes_then_reconnects_and_any_reply_recovers() {
        let mut heartbeat = Heartbeat::new(0);
        assert_eq!(heartbeat.tick(14_999), Tick::Wait(1));
        assert_eq!(heartbeat.tick(15_000), Tick::Ping);
        assert_eq!(heartbeat.tick(34_999), Tick::Wait(1));
        assert_eq!(heartbeat.tick(35_000), Tick::Lost);
        let mut heartbeat = Heartbeat::new(0);
        assert_eq!(heartbeat.tick(15_000), Tick::Ping);
        heartbeat.received(34_999);
        // A timeout queued before an incoming reply must not kill the socket.
        assert_eq!(heartbeat.tick(35_000), Tick::Wait(14_999));
        assert_eq!(heartbeat.tick(49_999), Tick::Ping);
        assert_eq!(heartbeat.tick(69_999), Tick::Lost);
    }
}
