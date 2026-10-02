//! Spec 887 — the idle clock behind `--idle-exit`.
//!
//! The daemon ends itself once it has been idle for `armed` seconds: no request, no
//! connected client, no recording trace (the last two HOLD the clock at "now"), unless a
//! `daemon/keep_alive` pushed the deadline out or held it forever. [`IdleClock`] is pure —
//! every method takes `now` — so the rules are tested without sleeping; the process-wide
//! clock below is the one the daemon arms, touches and polls.

use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct IdleClock {
    /// Idle window in seconds; 0 = idle exit off.
    pub armed: u64,
    last_activity: Instant,
    kept_until: Option<Instant>,
    kept_forever: bool,
    holding: Option<&'static str>,
}

impl IdleClock {
    pub fn new(armed: u64, now: Instant) -> Self {
        Self { armed, last_activity: now, kept_until: None, kept_forever: false, holding: None }
    }

    /// A request arrived: the window starts again.
    pub fn touch(&mut self, now: Instant) {
        self.last_activity = now;
    }

    /// Something that keeps the daemon in use (`"client"`, `"trace"`) is present, or not.
    /// While it is, the clock stays at `now`, so the window starts when it ends.
    pub fn hold(&mut self, what: Option<&'static str>, now: Instant) {
        self.holding = what;
        if what.is_some() {
            self.last_activity = now;
        }
    }

    /// `daemon/keep_alive`: hold at least `seconds` from now; `None` = never exit on idle.
    pub fn keep_alive(&mut self, seconds: Option<u64>, now: Instant) {
        self.last_activity = now;
        match seconds {
            Some(s) => {
                self.kept_forever = false;
                self.kept_until = Some(now + Duration::from_secs(s));
            }
            None => {
                self.kept_forever = true;
                self.kept_until = None;
            }
        }
    }

    /// When the daemon ends if nothing else happens; `None` = it will not (off, kept
    /// forever, or held).
    pub fn deadline(&self) -> Option<Instant> {
        if self.armed == 0 || self.kept_forever || self.holding.is_some() {
            return None;
        }
        let idle = self.last_activity + Duration::from_secs(self.armed);
        Some(match self.kept_until {
            Some(k) if k > idle => k,
            _ => idle,
        })
    }

    pub fn expired(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|d| now >= d)
    }

    pub fn status(&self, now: Instant) -> Value {
        let ms = |i: Instant| epoch_ms_of(i, now);
        json!({
            "armedSeconds": self.armed,
            "deadlineMs": self.deadline().map(ms),
            "keptAliveUntilMs": self.kept_until.filter(|k| *k > now).map(ms),
            "keptForever": self.kept_forever,
            "holding": self.holding,
        })
    }
}

fn epoch_ms_of(at: Instant, now: Instant) -> u64 {
    let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let wall = if at >= now { wall + (at - now) } else { wall.saturating_sub(now - at) };
    wall.as_millis() as u64
}

static CLOCK: Mutex<Option<IdleClock>> = Mutex::new(None);

/// Arm the process clock (`--idle-exit N`, N > 0).
pub fn arm(seconds: u64) {
    *CLOCK.lock().unwrap() = Some(IdleClock::new(seconds, Instant::now()));
}

/// A request arrived. A no-op when idle exit is not armed.
pub fn touch() {
    if let Some(c) = CLOCK.lock().unwrap().as_mut() {
        c.touch(Instant::now());
    }
}

pub fn hold(what: Option<&'static str>) {
    if let Some(c) = CLOCK.lock().unwrap().as_mut() {
        c.hold(what, Instant::now());
    }
}

/// `daemon/keep_alive`. Returns false when idle exit is not armed (nothing to hold).
pub fn keep_alive(seconds: Option<u64>) -> bool {
    match CLOCK.lock().unwrap().as_mut() {
        Some(c) => {
            c.keep_alive(seconds, Instant::now());
            true
        }
        None => false,
    }
}

/// The window, if the clock has run out.
pub fn expired() -> Option<u64> {
    CLOCK.lock().unwrap().as_ref().filter(|c| c.expired(Instant::now())).map(|c| c.armed)
}

/// The `idleExit` status object (Spec 887 D5).
pub fn status() -> Value {
    match CLOCK.lock().unwrap().as_ref() {
        Some(c) => c.status(Instant::now()),
        None => json!({
            "armedSeconds": 0,
            "deadlineMs": null,
            "keptAliveUntilMs": null,
            "keptForever": false,
            "holding": null,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn exits_after_the_window_and_a_request_resets_it() {
        let t0 = Instant::now();
        let mut c = IdleClock::new(600, t0);
        assert!(!c.expired(t0 + s(599)));
        assert!(c.expired(t0 + s(600)), "idle for the whole window ends it");
        c.touch(t0 + s(500));
        assert!(!c.expired(t0 + s(600)), "a request resets the window");
        assert!(c.expired(t0 + s(1100)));
    }

    #[test]
    fn keep_alive_holds_at_least_that_long_and_null_holds_for_good() {
        let t0 = Instant::now();
        let mut c = IdleClock::new(60, t0);
        c.keep_alive(Some(3600), t0);
        assert!(!c.expired(t0 + s(61)), "the keep-alive outlasts the idle window");
        assert!(!c.expired(t0 + s(3599)));
        assert!(c.expired(t0 + s(3600)));
        c.keep_alive(None, t0);
        assert_eq!(c.deadline(), None);
        assert!(!c.expired(t0 + s(1_000_000)), "null: never on idle");
        c.keep_alive(Some(10), t0);
        assert!(c.expired(t0 + s(60)), "a number replaces the forever");
    }

    #[test]
    fn a_client_or_a_trace_holds_the_clock_and_the_window_starts_when_it_ends() {
        let t0 = Instant::now();
        let mut c = IdleClock::new(60, t0);
        c.hold(Some("client"), t0 + s(10));
        assert_eq!(c.deadline(), None);
        c.hold(Some("trace"), t0 + s(500));
        assert!(!c.expired(t0 + s(500)));
        c.hold(None, t0 + s(500));
        assert!(!c.expired(t0 + s(559)), "the window starts when the hold ends");
        assert!(c.expired(t0 + s(560)));
    }

    #[test]
    fn off_never_expires_and_reports_zero() {
        let t0 = Instant::now();
        let c = IdleClock::new(0, t0);
        assert!(!c.expired(t0 + s(1_000_000)));
        let st = c.status(t0);
        assert_eq!(st["armedSeconds"], 0);
        assert!(st["deadlineMs"].is_null());
    }

    #[test]
    fn the_status_names_the_deadline_and_what_holds() {
        let t0 = Instant::now();
        let mut c = IdleClock::new(600, t0);
        let st = c.status(t0);
        assert!(st["deadlineMs"].as_u64().is_some(), "{st}");
        assert!(st["keptAliveUntilMs"].is_null());
        c.hold(Some("client"), t0);
        let st = c.status(t0);
        assert!(st["deadlineMs"].is_null());
        assert_eq!(st["holding"], "client");
    }
}
