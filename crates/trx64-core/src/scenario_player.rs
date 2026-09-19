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
//! A frame is the machine's (Spec 863: `Machine::timing().cycles_per_frame` — 19 656 on
//! PAL, 17 095 on NTSC 6567R8, 16 768 on the old 6567R56A, 20 280 on PAL-N); the caller
//! passes it. There is no default: an `at_frame` step means nothing without the machine it
//! was recorded on.

/// scenario-player.ts:22-27 — joystick state (all fields optional / default
/// false). Re-export of the canonical `keyboard::JoystickState` (the TS golden
/// declares `JoystickState` in `peripherals/keyboard.ts` and scenario-player.ts
/// imports it), so the scenario replay and the live CIA1 joystick model share
/// one type (no double declaration).
pub use crate::keyboard::JoystickState;

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
    /// Spec 863 — the machine switches to the model `name` at the frame boundary, as a
    /// recording's journal saw it. Scheduled at the cycle the recording switched on (a
    /// frame boundary), so the replay performs the same transplant at the same cycle; the
    /// frames the steps after it count are the new model's.
    Model {
        name: String,
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
    /// Spec 863 — switch the machine to the model `name` at the frame boundary. Returns the
    /// new model's cycles per frame, or why it could not switch. A target that cannot
    /// switch models says so.
    fn switch_model(&mut self, name: &str) -> Result<u64, String> {
        Err(format!("this replay target cannot switch the machine to {name}"))
    }
}

/// scenario-player.ts:34 — `class ScenarioPlayer`.
pub struct ScenarioPlayer {
    steps: Vec<ScenarioStep>,
    /// The frame an `at_frame` schedule counts in: the model the scenario STARTS on (the
    /// schedule is sorted once, up front). A scenario that switches models schedules by
    /// cycle — a recording does.
    cycles_per_frame: u64,
    /// The frame a duration counts in: the model the machine is on NOW — it follows a
    /// `Model` step.
    frame_now: u64,
    next_idx: usize,
    /// Why a step could not be performed (a `Model` step the target refused). The player
    /// stops there; the caller reads it with `failure`.
    failure: Option<String>,
}

impl ScenarioPlayer {
    /// scenario-player.ts:39-52 — constructor. Sorts the steps by absolute cycle
    /// ascending (`at_cycle`, or `at_frame * cyclesPerFrame`, or 0). A STABLE sort
    /// preserves the relative order of equal-cycle steps (matching the TS Array.sort,
    /// which is stable in V8).
    pub fn new(mut steps: Vec<ScenarioStep>, cycles_per_frame: u64) -> Self {
        let cpf = cycles_per_frame.max(1);
        steps.sort_by_key(|s| abs_cycle(s, cpf));
        Self {
            steps,
            cycles_per_frame: cpf,
            frame_now: cpf,
            next_idx: 0,
            failure: None,
        }
    }

    /// Why the replay stopped short, if a step could not be performed.
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// The frame length durations count in now (the current model's).
    pub fn cycles_per_frame_now(&self) -> u64 {
        self.frame_now
    }

    /// scenario-player.ts:56-67 — `tick(target, currentCycle)`. Apply every step
    /// that has come due as of `current_cycle`. Returns the count fired.
    pub fn tick<T: ScenarioTarget>(&mut self, target: &mut T, current_cycle: u64) -> usize {
        let mut fired = 0;
        while self.failure.is_none() && self.next_idx < self.steps.len() {
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
        self.frame_now = self.cycles_per_frame;
        self.failure = None;
    }

    /// The absolute cycle each step fires at (for the caller to drive its run loop
    /// up to the next due step). None when no steps remain.
    pub fn next_due_cycle(&self) -> Option<u64> {
        self.steps
            .get(self.next_idx)
            .map(|s| abs_cycle(s, self.cycles_per_frame))
    }

    /// scenario-player.ts:72-103 — `dispatch(target, step)`.
    fn dispatch<T: ScenarioTarget>(&mut self, target: &mut T, step: &ScenarioStep) {
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
                    target.run_for(entry.duration_frames * self.frame_now);
                }
            }
            ScenarioStepKind::Model { name } => match target.switch_model(name) {
                Ok(cpf) => self.frame_now = cpf.max(1),
                Err(why) => self.failure = Some(why),
            },
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

    /// The default model's frame (`c64-pal`).
    const PAL_FRAME: u64 = 19656;

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
        let mut p = ScenarioPlayer::new(steps, PAL_FRAME);
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
        let mut p = ScenarioPlayer::new(steps, PAL_FRAME);
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
        let mut p = ScenarioPlayer::new(steps, 100);
        let mut t = LogTarget::default();
        assert_eq!(p.tick(&mut t, 0), 1);
        // Each entry: set joystick1 then run_for(duration*100).
        assert_eq!(t.log, vec!["joy1:true", "run:100", "joy1:false", "run:200"]);
        assert_eq!(t.clock, 300);
    }

    #[test]
    fn reset_replays_from_start() {
        let steps = vec![step(0, ScenarioStepKind::Type { text: "X".into() })];
        let mut p = ScenarioPlayer::new(steps, PAL_FRAME);
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
            let mut p = ScenarioPlayer::new(mk(), PAL_FRAME);
            let mut t = LogTarget::default();
            for cycle in (0..=400).step_by(50) {
                p.tick(&mut t, cycle);
            }
            t.log
        };
        assert_eq!(run(), run(), "deterministic dispatch order");
    }

    /// Spec 863 — a `Model` step switches the machine at its cycle, and the durations after
    /// it count the new model's frames; a target that refuses the switch stops the replay
    /// there and says why.
    #[test]
    fn a_model_step_switches_and_later_frames_are_the_new_models() {
        struct Switcher {
            log: Vec<String>,
        }
        impl ScenarioTarget for Switcher {
            fn type_text(&mut self, text: &str) {
                self.log.push(format!("type:{text}"));
            }
            fn set_joystick1(&mut self, _: JoystickState) {}
            fn set_joystick2(&mut self, s: JoystickState) {
                self.log.push(format!("joy2:{}", s.fire));
            }
            fn set_paddle(&mut self, _: u8, _: i64) {}
            fn trigger_restore_nmi(&mut self) {}
            fn run_for(&mut self, cycles: u64) {
                self.log.push(format!("run:{cycles}"));
            }
            fn switch_model(&mut self, name: &str) -> Result<u64, String> {
                self.log.push(format!("model:{name}"));
                match name {
                    "c64-ntsc" => Ok(17_095),
                    _ => Err(format!("{name} cannot run here")),
                }
            }
        }
        let press = |frames| ScenarioStepKind::JoystickScript {
            port: 2,
            sequence: vec![JoystickScriptEntry { state: JoystickState { fire: true, ..Default::default() }, duration_frames: frames }],
        };
        let steps = vec![
            step(100, press(1)),
            step(PAL_FRAME * 3, ScenarioStepKind::Model { name: "c64-ntsc".into() }),
            step(PAL_FRAME * 3 + 500, press(2)),
            step(PAL_FRAME * 4, ScenarioStepKind::Model { name: "c64c-pal".into() }),
            step(PAL_FRAME * 5, ScenarioStepKind::Type { text: "never".into() }),
        ];
        let mut p = ScenarioPlayer::new(steps, PAL_FRAME);
        let mut t = Switcher { log: Vec::new() };
        p.tick(&mut t, PAL_FRAME * 3 - 1);
        assert_eq!(p.cycles_per_frame_now(), PAL_FRAME);
        p.tick(&mut t, PAL_FRAME * 3);
        assert_eq!(p.cycles_per_frame_now(), 17_095, "the frame is the new model's from the switch on");
        p.tick(&mut t, PAL_FRAME * 10);
        assert_eq!(
            t.log,
            vec!["joy2:true", "run:19656", "model:c64-ntsc", "joy2:true", "run:34190", "model:c64c-pal"],
            "a press after the switch lasts NTSC frames; the refused switch stops the replay"
        );
        assert_eq!(p.failure(), Some("c64c-pal cannot run here"));
        assert_eq!(p.remaining(), 1, "nothing after the refusal ran");
        // The default target cannot switch, and says so.
        let mut p = ScenarioPlayer::new(vec![step(0, ScenarioStepKind::Model { name: "c64-ntsc".into() })], PAL_FRAME);
        p.tick(&mut LogTarget::default(), 0);
        assert!(p.failure().unwrap().contains("c64-ntsc"));
    }
}
