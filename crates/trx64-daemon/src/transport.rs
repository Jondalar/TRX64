//! transport.rs — Spec 808: the rewind transport.
//!
//! Play the machine BACKWARDS. Not a picture player — every step is a real
//! `restore_live_checkpoint`, measured at **177 µs** (`perf_bench
//! bench_checkpoint_restore_step`), which is 0.89 % of a PAL frame and 0.9 % of
//! wall-clock at 50 steps/s. Because the machine actually moves, everything that
//! already looks at it follows for free: the TUI's `/window`, the C64RE UI, the
//! register panels, `chis`, screenshots. There is no filmstrip cache to keep in step
//! and no second renderer, which is also why feature parity between the two front-ends
//! costs nothing — they are looking at the same thing.
//!
//! THE MODEL (Spec 808 §3, owner decisions 2026-08-13):
//!   - the DAEMON drives the loop; clients only display and send verbs
//!   - ONE timeline: diverging from a rewound position truncates the future
//!   - play-forward REPLAYS the anchors that are still there, and only becomes live
//!     emulation when it reaches the head — watching never costs you your recording
//!
//! THE HAZARD (§5): replaying forward and running live look identical. So the mode is
//! part of every status this module returns, and `Cut` reports how many anchors went.

use serde_json::{json, Value};

/// Which of the three states the transport is in. §5 — this is not decoration: a
/// transport that changes what it is without saying so is the defect the mode exists
/// to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    /// Standing on / moving through anchors that are still intact.
    Replay,
    /// At the head; the machine is running and new anchors are being recorded.
    Live,
    /// An intervention truncated the future at the cursor. Sticky until the next
    /// transport move, so the user actually sees it happened.
    Cut,
}

impl TransportMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransportMode::Replay => "REPLAY",
            TransportMode::Live => "LIVE",
            TransportMode::Cut => "CUT",
        }
    }
    /// The glyph the TUI line and the UI ribbon both show.
    pub fn glyph(&self) -> &'static str {
        match self {
            TransportMode::Replay => "\u{25c0}\u{25c0}",
            TransportMode::Live => "\u{25b6}",
            TransportMode::Cut => "\u{25b6}",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Back,
    Fwd,
}

impl Direction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Direction::Back => "back",
            Direction::Fwd => "fwd",
        }
    }
}

/// Live transport state. Lives on the daemon `State`, because decision 1 is that the
/// daemon owns the loop — two clients each running their own would mean building the
/// smoothing twice and watching two timers drift.
#[derive(Debug, Clone)]
pub struct Transport {
    /// The anchor the machine is standing on. `None` = at the head, i.e. live.
    pub cursor: Option<String>,
    /// `Some(dir)` while playing; `None` when paused or live.
    pub playing: Option<Direction>,
    /// Steps per stream-loop frame. 1 = 50 steps/s at cadence 1 = real-time.
    pub speed: u32,
    pub mode: TransportMode,
    /// How many anchors the last truncation dropped (0 when nothing was cut).
    pub last_cut: u64,
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            cursor: None,
            playing: None,
            speed: 1,
            mode: TransportMode::Live,
            last_cut: 0,
        }
    }
}

impl Transport {
    /// True while the transport owns the machine — the stream loop must NOT advance
    /// emulation, because the transport is placing the machine itself.
    pub fn holds_the_machine(&self) -> bool {
        self.cursor.is_some()
    }
}

/// One position on the timeline, resolved against the ring.
#[derive(Debug, Clone)]
pub struct Position {
    pub index: usize,
    pub total: usize,
    pub id: String,
    pub frame: u64,
    pub cycles: u64,
}

/// Where the cursor sits in a list of anchors (oldest-first, as `ring.list()` returns).
/// `None` when the ring is empty or the id is gone (evicted while we stood on it).
pub fn locate(ids: &[(String, u64, u64)], cursor: Option<&str>) -> Option<Position> {
    let total = ids.len();
    if total == 0 {
        return None;
    }
    let index = match cursor {
        // No cursor = the head, which is the NEWEST anchor.
        None => total - 1,
        Some(id) => ids.iter().position(|(i, _, _)| i == id)?,
    };
    let (id, frame, cycles) = ids[index].clone();
    Some(Position {
        index,
        total,
        id,
        frame,
        cycles,
    })
}

/// Clamp a signed step against the anchor list. Returns the new index and whether the
/// move hit an end — the transport stops at the ends rather than wrapping, because a
/// rewind that silently loops back to `now` is indistinguishable from one that broke.
pub fn step_index(from: usize, delta: i64, total: usize) -> (usize, bool) {
    if total == 0 {
        return (0, true);
    }
    let last = total as i64 - 1;
    let want = from as i64 + delta;
    let clamped = want.clamp(0, last);
    (clamped as usize, clamped != want)
}

/// The one-line transport status both surfaces render (§4/§5). Kept here rather than in
/// the TUI so the C64RE ribbon and the terminal cannot drift into describing the same
/// state differently.
pub fn status_line(t: &Transport, pos: Option<&Position>, seconds_behind: f64) -> String {
    let mode = t.mode;
    match pos {
        None => format!("{:<7} {}  (no anchors yet)", mode.as_str(), mode.glyph()),
        Some(p) => {
            let when = if p.index + 1 == p.total {
                "now".to_string()
            } else {
                format!("-{seconds_behind:.1}s")
            };
            let filled = if p.total <= 1 {
                30
            } else {
                (p.index * 30 / (p.total - 1)).min(30)
            };
            let bar: String = "\u{2593}".repeat(filled) + &"\u{2591}".repeat(30 - filled);
            let mut s = format!(
                "{:<7} {}  frame {}/{}   {when}   {bar}",
                mode.as_str(),
                mode.glyph(),
                p.index + 1,
                p.total
            );
            if mode == TransportMode::Cut && t.last_cut > 0 {
                s.push_str(&format!("  (-{} anchors)", t.last_cut));
            }
            s
        }
    }
}

/// The key legend, printed under the status line whenever the transport is showing
/// (§4). The keys are legible exactly when they are usable, so nobody has to have
/// learned them.
pub const KEY_LEGEND: &str =
    " F9 \u{25c0}|   F10 \u{25c0}\u{25c0}   F11 \u{23f8}/\u{25b6}   F12 |\u{25b6}        (`rewind` for the full picture)";

/// The structured status — the RPC half of the parity requirement (§2/G2). Every field
/// the TUI line shows is in here, so the UI ribbon renders from data rather than by
/// parsing a string.
pub fn status_json(t: &Transport, pos: Option<&Position>, seconds_behind: f64) -> Value {
    json!({
        "mode": t.mode.as_str(),
        "playing": t.playing.map(|d| d.as_str()),
        "speed": t.speed,
        "cursor": t.cursor,
        "frameIndex": pos.map(|p| p.index as u64 + 1),
        "frameTotal": pos.map(|p| p.total as u64),
        "anchorId": pos.map(|p| p.id.clone()),
        "frame": pos.map(|p| p.frame),
        "cycles": pos.map(|p| p.cycles),
        "secondsBehind": if pos.map(|p| p.index + 1 == p.total).unwrap_or(true) { 0.0 } else { seconds_behind },
        "atHead": pos.map(|p| p.index + 1 == p.total).unwrap_or(true),
        "lastCut": t.last_cut,
        "line": status_line(t, pos, seconds_behind),
        "keys": KEY_LEGEND,
    })
}


/// Spec 808 §4 — the ONE key map. F9..F12 → the monitor verb they run.
///
/// This exists as a shared function because the two front-ends have separate key paths
/// — winit in `trx64-cli/src/window.rs`, crossterm in `trx64-cli/src/tui.rs` — and the
/// whole point of a host hotkey is that it does the same thing wherever the focus
/// happens to be. When the mapping lived in both places, moving pause from F10 to F11
/// updated the terminal and left the emulator window still pausing on F10. One table,
/// two callers, and that class of drift is gone rather than merely fixed once.
///
/// `running` only matters for F11, the one key whose meaning depends on where you are:
/// running → pause, otherwise → play forward (which at the head is just "run on", the
/// old freeze/resume behaviour under a new name).
pub fn key_verb(f: u8, _running: bool) -> Option<&'static str> {
    match f {
        9 => Some("frame -1"),
        10 => Some("play back"),
        // F11 is NOT a fixed verb — it is a DECISION, so it is not in this table. See
        // `f11_verb`. Mapping it to one string was wrong in two of the four states: at
        // the head `play fwd` has nothing ahead to replay, so it correctly did nothing
        // and the machine stayed paused with the key looking dead.
        11 => None,
        12 => Some("frame +1"),
        _ => None,
    }
}


/// F11 — the universal go/stop, as a DECISION rather than a fixed verb.
///
/// This is the state machine in one function, and writing it out is what exposed the
/// hole. The rule is one sentence — **if anything is moving, stop it; otherwise resume
/// from where we are** — and "resume" differs by position:
///
/// ```text
///   running  playing  rewound   F11 does     because
///   ──────────────────────────────────────────────────────────────
///   yes      -        -         /pause       stop the machine
///   no       yes      yes       pause        stop the playback
///   no       no       yes       play fwd     replay forward from here
///   no       no       no        /run         at the head: just run
/// ```
///
/// The last row is the one that was broken.
pub fn f11_verb(running: bool, playing: bool, rewound: bool) -> &'static str {
    if running {
        "/pause"
    } else if playing {
        "pause"
    } else if rewound {
        "play fwd"
    } else {
        "/run"
    }
}

#[cfg(test)]
mod key_tests {
    use super::{f11_verb, key_verb};

    #[test]
    fn the_four_host_keys_map_and_nothing_else_does() {
        assert_eq!(key_verb(9, false), Some("frame -1"));
        assert_eq!(key_verb(10, false), Some("play back"));
        assert_eq!(key_verb(12, false), Some("frame +1"));
        // F1..F8 belong to the emulated machine and must never be intercepted.
        for f in 1..=8u8 {
            assert_eq!(key_verb(f, false), None, "F{f} is a C64 key");
        }
        assert_eq!(key_verb(13, false), None);
    }


    #[test]
    fn f10_is_no_longer_pause() {
        // The regression this table exists for: 808 moved pause to F11, and the
        // emulator window kept pausing on F10 because it had its own copy.
        assert_ne!(key_verb(10, true), Some("pause"));
        assert_ne!(key_verb(10, false), Some("pause"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<(String, u64, u64)> {
        (0..n)
            .map(|i| (format!("cp_{i}_0"), i as u64, i as u64 * 19_656))
            .collect()
    }

    #[test]
    fn no_cursor_means_the_head_not_the_start() {
        // The head is the NEWEST anchor. Getting this backwards would make `play back`
        // from a live machine jump 10 seconds into the past on its first step.
        let l = ids(5);
        let p = locate(&l, None).unwrap();
        assert_eq!(p.index, 4);
        assert_eq!(p.total, 5);
        assert_eq!(p.id, "cp_4_0");
    }

    #[test]
    fn a_cursor_on_an_evicted_anchor_resolves_to_nothing() {
        // The ring evicts while you stand on it. Better to report "gone" than to
        // silently snap somewhere and let the user think they are where they were.
        let l = ids(3);
        assert!(locate(&l, Some("cp_99_0")).is_none());
    }

    #[test]
    fn stepping_stops_at_the_ends_instead_of_wrapping() {
        assert_eq!(step_index(4, -1, 5), (3, false));
        assert_eq!(step_index(0, -1, 5), (0, true), "start must clamp, not wrap");
        assert_eq!(step_index(4, 1, 5), (4, true), "head must clamp, not wrap");
        assert_eq!(step_index(2, -10, 5), (0, true));
        assert_eq!(step_index(0, 0, 0), (0, true), "empty ring is always 'at an end'");
    }

    #[test]
    fn the_status_line_says_now_at_the_head_and_a_delta_behind_it() {
        let l = ids(500);
        let mut t = Transport::default();
        let head = locate(&l, None).unwrap();
        assert!(status_line(&t, Some(&head), 0.0).contains("now"));
        assert!(status_line(&t, Some(&head), 0.0).contains("LIVE"));

        t.mode = TransportMode::Replay;
        t.cursor = Some("cp_339_0".into());
        let back = locate(&l, t.cursor.as_deref()).unwrap();
        let line = status_line(&t, Some(&back), 3.2);
        assert!(line.contains("REPLAY"), "{line}");
        assert!(line.contains("frame 340/500"), "{line}");
        assert!(line.contains("-3.2s"), "{line}");
    }

    #[test]
    fn a_cut_reports_how_many_anchors_it_dropped() {
        // §5: the cut is the moment the recording is lost. Saying so, with a number, is
        // the whole mitigation for choosing play-forward-replays over truncate-at-once.
        let l = ids(341);
        let t = Transport {
            cursor: Some("cp_339_0".into()),
            playing: None,
            speed: 1,
            mode: TransportMode::Cut,
            last_cut: 159,
        };
        let pos = locate(&l, t.cursor.as_deref()).unwrap();
        let line = status_line(&t, Some(&pos), 3.2);
        assert!(line.contains("CUT"), "{line}");
        assert!(line.contains("-159 anchors"), "{line}");
    }

    #[test]
    fn status_json_carries_every_field_the_line_shows() {
        // G2 parity: the UI must render from data, not by parsing the terminal string.
        let l = ids(500);
        let t = Transport {
            cursor: Some("cp_339_0".into()),
            playing: Some(Direction::Back),
            speed: 2,
            mode: TransportMode::Replay,
            last_cut: 0,
        };
        let pos = locate(&l, t.cursor.as_deref()).unwrap();
        let j = status_json(&t, Some(&pos), 3.2);
        assert_eq!(j["mode"], "REPLAY");
        assert_eq!(j["playing"], "back");
        assert_eq!(j["speed"], 2);
        assert_eq!(j["frameIndex"], 340);
        assert_eq!(j["frameTotal"], 500);
        assert_eq!(j["atHead"], false);
        assert_eq!(j["anchorId"], "cp_339_0");
        assert!(j["line"].as_str().unwrap().contains("REPLAY"));
        assert!(j["keys"].as_str().unwrap().contains("F10"));
    }

    #[test]
    fn at_the_head_seconds_behind_is_zero_whatever_was_passed() {
        let l = ids(10);
        let t = Transport::default();
        let pos = locate(&l, None).unwrap();
        let j = status_json(&t, Some(&pos), 99.0);
        assert_eq!(j["secondsBehind"], 0.0, "the head is never behind itself");
        assert_eq!(j["atHead"], true);
    }

    #[test]
    fn the_transport_holds_the_machine_only_while_rewound() {
        let mut t = Transport::default();
        assert!(!t.holds_the_machine(), "live must let the stream loop advance");
        t.cursor = Some("cp_1_0".into());
        assert!(t.holds_the_machine(), "rewound must stop the stream loop advancing");
    }
}
