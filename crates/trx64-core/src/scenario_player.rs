//! scenario_player.rs — Spec 107 (M2.5): the scenario player.
//!
//! 1:1 PORT of the c64re TS
//!   C64ReverseEngineeringMCP/src/runtime/headless/input/scenario-player.ts
//! (class `ScenarioPlayer`).
//!
//! Replays a list of input actions at scheduled cycle/frame boundaries. The player
//! is a pure SCHEDULER: it sorts the steps by absolute cycle and, on each `tick`,
//! dispatches every step that has come due as of the current machine cycle. The
//! caller drives the machine cycle-by-cycle and calls `tick` to fire the inputs —
//! this is what makes a replay DETERMINISTIC: the same steps fire at the same
//! cycles regardless of wall time.
//!
//! WHAT DIFFERS FROM THE TS: the TS `dispatch` calls IntegratedSession methods
//! (typeText / setJoystick1 / setJoystick2 / setPaddle / triggerRestoreNmi /
//! runFor). TRX64 drives the dispatch through a `ScenarioTarget` trait so the
//! daemon can wire the live `Machine` (keyboard.type_text is the implemented path;
//! the joystick / paddle / restore-NMI steps are surfaced as trait calls the daemon
//! implements or no-ops, mirroring the daemon's existing joystick stubs at
//! session/joystick_set). The scheduling, sort order, and `tick`/`remaining`/`reset`
//! contract are byte-for-byte the TS.
//!
//! PAL frame = 19656 cycles (scenario-player.ts:16 / DEFAULT below), NTSC = 17030.

/// scenario-player.ts:34 — default PAL cycles per frame.
pub const DEFAULT_CYCLES_PER_FRAME: u64 = 19656;

/// scenario-player.ts:22-27 — joystick state (all fields optional / default false).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JoystickState {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    pub fire: bool,
}

/// scenario-player.ts:27 — one entry of a composite joystickScript.
#[derive(Debug, Clone)]
pub struct JoystickScriptEntry {
    pub state: JoystickState,
    pub duration_frames: u64,
}

/// scenario-player.ts:20-27 — `ScenarioStep`. Each variant carries the optional
/// schedule (`at_cycle` / `at_frame`) + its kind-specific payload.
#[derive(Debug, Clone)]
pub enum ScenarioStepKind {
    Type {
        text: String,
    },
    Joy1 {
        state: JoystickState,
    },
    Joy2 {
        state: JoystickState,
    },
    Paddle {
        idx: u8,
        value: i64,
    },
    Restore,
    JoystickScript {
        port: u8,
        sequence: Vec<JoystickScriptEntry>,
    },
}

/// scenario-player.ts:20-27 — a step = an optional schedule + a kind.
#[derive(Debug, Clone)]
pub struct ScenarioStep {
    pub at_cycle: Option<u64>,
    pub at_frame: Option<u64>,
    pub kind: ScenarioStepKind,
}

/// The replay target — the surface scenario-player.ts:72-103 `dispatch` calls. The
/// daemon implements this over the live `Machine` (keyboard for `type`; joystick /
/// paddle / restore are the daemon's existing stubs). `run_for` drives the inner
/// composite-macro replay (joystickScript) — the caller's run engine.
pub trait ScenarioTarget {
    /// scenario-player.ts:75 — session.typeText(text, 80_000, 80_000).
    fn type_text(&mut self, text: &str);
    /// scenario-player.ts:78 — session.setJoystick1(state).
    fn set_joystick1(&mut self, state: JoystickState);
    /// scenario-player.ts:81 — session.setJoystick2(state).
    fn set_joystick2(&mut self, state: JoystickState);
    /// scenario-player.ts:84 — session.setPaddle(idx, value).
    fn set_paddle(&mut self, idx: u8, value: i64);
    /// scenario-player.ts:87 — session.triggerRestoreNmi().
    fn trigger_restore_nmi(&mut self);
    /// scenario-player.ts:98 — session.runFor(cycles) (composite-macro inline run).
    fn run_for(&mut self, cycles: u64);
}

/// scenario-player.ts:34 — `class ScenarioPlayer`.
pub struct ScenarioPlayer {
    steps: Vec<ScenarioStep>,
    cycles_per_frame: u64,
    next_idx: usize,
}

impl ScenarioPlayer {
    /// scenario-player.ts:39-52 — constructor. Sorts the steps by absolute cycle
    /// ascending (`at_cycle`, or `at_frame * cyclesPerFrame`, or 0). A STABLE sort
    /// preserves the relative order of equal-cycle steps (matching the TS Array.sort,
    /// which is stable in V8).
    pub fn new(mut steps: Vec<ScenarioStep>, cycles_per_frame: Option<u64>) -> Self {
        let cpf = cycles_per_frame.unwrap_or(DEFAULT_CYCLES_PER_FRAME);
        steps.sort_by_key(|s| abs_cycle(s, cpf));
        Self {
            steps,
            cycles_per_frame: cpf,
            next_idx: 0,
        }
    }

    /// scenario-player.ts:56-67 — `tick(target, currentCycle)`. Apply every step
    /// that has come due as of `current_cycle`. Returns the count fired.
    pub fn tick<T: ScenarioTarget>(&mut self, target: &mut T, current_cycle: u64) -> usize {
        let mut fired = 0;
        while self.next_idx < self.steps.len() {
            let due_at = abs_cycle(&self.steps[self.next_idx], self.cycles_per_frame);
            if current_cycle < due_at {
                break;
            }
            // Clone the step out so we can borrow target mutably during dispatch.
            let step = self.steps[self.next_idx].clone();
            self.dispatch(target, &step);
            self.next_idx += 1;
            fired += 1;
        }
        fired
    }

    /// scenario-player.ts:69 — `remaining()`.
    pub fn remaining(&self) -> usize {
        self.steps.len() - self.next_idx
    }

    /// scenario-player.ts:70 — `reset()`.
    pub fn reset(&mut self) {
        self.next_idx = 0;
    }

    /// The absolute cycle each step fires at (for the caller to drive its run loop
    /// up to the next due step). None when no steps remain.
    pub fn next_due_cycle(&self) -> Option<u64> {
        self.steps
            .get(self.next_idx)
            .map(|s| abs_cycle(s, self.cycles_per_frame))
    }

    /// scenario-player.ts:72-103 — `dispatch(target, step)`.
    fn dispatch<T: ScenarioTarget>(&self, target: &mut T, step: &ScenarioStep) {
        match &step.kind {
            ScenarioStepKind::Type { text } => target.type_text(text),
            ScenarioStepKind::Joy1 { state } => target.set_joystick1(*state),
            ScenarioStepKind::Joy2 { state } => target.set_joystick2(*state),
            ScenarioStepKind::Paddle { idx, value } => target.set_paddle(*idx, *value),
            ScenarioStepKind::Restore => target.trigger_restore_nmi(),
            ScenarioStepKind::JoystickScript { port, sequence } => {
                // scenario-player.ts:89-101 — composite: apply each sequence step
                // inline (set state, then run its duration), within this one tick.
                for entry in sequence {
                    if *port == 1 {
                        target.set_joystick1(entry.state);
                    } else {
                        target.set_joystick2(entry.state);
                    }
                    target.run_for(entry.duration_frames * self.cycles_per_frame);
                }
            }
        }
    }
}

/// scenario-player.ts:44/60 — the absolute cycle of a step.
fn abs_cycle(s: &ScenarioStep, cycles_per_frame: u64) -> u64 {
    s.at_cycle
        .unwrap_or_else(|| s.at_frame.map(|f| f * cycles_per_frame).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording target: logs every dispatched action (and advances a clock for
    /// composite run_for).
    #[derive(Default)]
    struct LogTarget {
        log: Vec<String>,
        clock: u64,
    }
    impl ScenarioTarget for LogTarget {
        fn type_text(&mut self, text: &str) {
            self.log.push(format!("type:{text}"));
        }
        fn set_joystick1(&mut self, s: JoystickState) {
            self.log.push(format!("joy1:{}", s.fire));
        }
        fn set_joystick2(&mut self, s: JoystickState) {
            self.log.push(format!("joy2:{}", s.fire));
        }
        fn set_paddle(&mut self, idx: u8, value: i64) {
            self.log.push(format!("paddle:{idx}={value}"));
        }
        fn trigger_restore_nmi(&mut self) {
            self.log.push("restore".into());
        }
        fn run_for(&mut self, cycles: u64) {
            self.clock += cycles;
            self.log.push(format!("run:{cycles}"));
        }
    }

    fn step(at_cycle: u64, kind: ScenarioStepKind) -> ScenarioStep {
        ScenarioStep {
            at_cycle: Some(at_cycle),
            at_frame: None,
            kind,
        }
    }

    #[test]
    fn fires_due_steps_in_cycle_order() {
        let steps = vec![
            step(
                3000,
                ScenarioStepKind::Type {
                    text: "C".into(),
                },
            ),
            step(
                1000,
                ScenarioStepKind::Type {
                    text: "A".into(),
                },
            ),
            step(
                2000,
                ScenarioStepKind::Type {
                    text: "B".into(),
                },
            ),
        ];
        let mut p = ScenarioPlayer::new(steps, None);
        let mut t = LogTarget::default();
        assert_eq!(p.tick(&mut t, 500), 0, "none due yet");
        assert_eq!(p.tick(&mut t, 1500), 1, "A due");
        assert_eq!(p.tick(&mut t, 2500), 1, "B due");
        assert_eq!(p.tick(&mut t, 999999), 1, "C due");
        assert_eq!(t.log, vec!["type:A", "type:B", "type:C"]);
        assert_eq!(p.remaining(), 0);
    }

    #[test]
    fn at_frame_resolves_via_cycles_per_frame() {
        // at_frame 2 @ PAL = 2*19656 = 39312.
        let steps = vec![ScenarioStep {
            at_cycle: None,
            at_frame: Some(2),
            kind: ScenarioStepKind::Restore,
        }];
        let mut p = ScenarioPlayer::new(steps, None);
        assert_eq!(p.next_due_cycle(), Some(39312));
        let mut t = LogTarget::default();
        assert_eq!(p.tick(&mut t, 39311), 0);
        assert_eq!(p.tick(&mut t, 39312), 1);
        assert_eq!(t.log, vec!["restore"]);
    }

    #[test]
    fn joystick_script_runs_inline_within_one_tick() {
        let seq = vec![
            JoystickScriptEntry {
                state: JoystickState {
                    fire: true,
                    ..Default::default()
                },
                duration_frames: 1,
            },
            JoystickScriptEntry {
                state: JoystickState::default(),
                duration_frames: 2,
            },
        ];
        let steps = vec![step(0, ScenarioStepKind::JoystickScript { port: 1, sequence: seq })];
        let mut p = ScenarioPlayer::new(steps, Some(100));
        let mut t = LogTarget::default();
        assert_eq!(p.tick(&mut t, 0), 1);
        // Each entry: set joystick1 then run_for(duration*100).
        assert_eq!(t.log, vec!["joy1:true", "run:100", "joy1:false", "run:200"]);
        assert_eq!(t.clock, 300);
    }

    #[test]
    fn reset_replays_from_start() {
        let steps = vec![step(0, ScenarioStepKind::Type { text: "X".into() })];
        let mut p = ScenarioPlayer::new(steps, None);
        let mut t = LogTarget::default();
        p.tick(&mut t, 0);
        assert_eq!(p.remaining(), 0);
        p.reset();
        assert_eq!(p.remaining(), 1);
        p.tick(&mut t, 0);
        assert_eq!(t.log, vec!["type:X", "type:X"]);
    }

    /// Determinism: two identical players ticked at the same cycle schedule produce
    /// the identical dispatch log.
    #[test]
    fn deterministic_replay_same_log() {
        let mk = || {
            vec![
                step(100, ScenarioStepKind::Type { text: "LOAD".into() }),
                step(200, ScenarioStepKind::Joy1 { state: JoystickState { fire: true, ..Default::default() } }),
                step(300, ScenarioStepKind::Paddle { idx: 0, value: 128 }),
            ]
        };
        let run = || {
            let mut p = ScenarioPlayer::new(mk(), None);
            let mut t = LogTarget::default();
            for cycle in (0..=400).step_by(50) {
                p.tick(&mut t, cycle);
            }
            t.log
        };
        assert_eq!(run(), run(), "deterministic dispatch order");
    }
}
