//! Typed event stream — NotifyHub broadcasts mapped to a typed [`RuntimeEvent`].
//!
//! The daemon's [`NotifyHub`](trx64_daemon::streaming::NotifyHub) fans every server
//! push (a JSON-RPC NOTIFICATION `{jsonrpc, method, params}`, no id) to its
//! subscribers. The FFI subscribes a tokio mpsc channel, and a dedicated OS thread
//! ([`forward_loop`]) BLOCK-drains it (`blocking_recv` — no async runtime needed),
//! parses each `Message::Text` envelope back to `(method, params)`, maps it to a
//! typed [`RuntimeEvent`], and calls [`EventListener::on_event`].
//!
//! Every known event has a typed variant; anything else (and any future event)
//! falls through to [`RuntimeEvent::Other`] with the raw method + params JSON, so no
//! broadcast is ever dropped silently.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::tungstenite::Message;

/// A typed runtime event delivered to a Swift [`EventListener`].
#[derive(Debug, Clone, uniffi::Enum)]
pub enum RuntimeEvent {
    /// A new video frame is available (`session/frame_available`).
    FrameAvailable {
        session_id: String,
        frame: u64,
        c64_cycles: u64,
    },
    /// Free-run resumed (`debug/running`).
    Running { session_id: String },
    /// Paused (`debug/paused`).
    Paused {
        session_id: String,
        reason: String,
        pc: u32,
        cycles: u64,
    },
    /// Execution halted (breakpoint / step / JAM / crash) (`debug/stopped`).
    Stopped {
        session_id: String,
        reason: String,
        pc: u32,
        cycles: u64,
    },
    /// A numbered breakpoint was hit (`debug/breakpoint_hit`).
    BreakpointHit {
        session_id: String,
        pc: u32,
        num: u32,
    },
    /// A monitor-DSL observer fired (`debug/observer_hit`).
    ObserverHit { session_id: String, name: String },
    /// An observer `log` action emitted a line (`debug/observer_log`).
    ObserverLog { session_id: String, message: String },
    /// A checkpoint was restored (scrub) (`debug/checkpoint_restored`).
    CheckpointRestored { session_id: String, id: String },
    /// Control owner changed (human ⇄ llm) (`debug/control`).
    ControlChanged {
        session_id: String,
        control_owner: String,
    },
    /// The audio timeline is discontinuous — flush the worklet ring (`audio/flush`).
    AudioFlush { session_id: String },
    /// A cartridge's flash was persisted to its host file (`media/cart_persisted`).
    MediaChanged { session_id: String },
    /// A batch run reported progress (`batch/progress`).
    BatchProgress { params_json: String },
    /// Any other / future broadcast — raw method + params JSON (nothing is dropped).
    Other { method: String, params_json: String },
}

/// The Swift-side listener. uniffi delivers each [`RuntimeEvent`] via this callback
/// interface; the implementor is a Swift class (kept alive by the `Arc` the FFI
/// holds in the forwarder, so it outlives every `on_event` call).
#[uniffi::export(callback_interface)]
pub trait EventListener: Send + Sync {
    fn on_event(&self, event: RuntimeEvent);
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}
fn u32f(v: &Value, k: &str) -> u32 {
    v.get(k).and_then(|x| x.as_u64()).unwrap_or(0) as u32
}
fn u64f(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(|x| x.as_u64()).unwrap_or(0)
}

/// Map a `(method, params)` notification envelope to a typed [`RuntimeEvent`].
fn map_event(method: &str, params: &Value) -> RuntimeEvent {
    let sid = s(params, "session_id");
    match method {
        "session/frame_available" => RuntimeEvent::FrameAvailable {
            session_id: sid,
            frame: u64f(params, "frame"),
            c64_cycles: u64f(params, "c64Cycles"),
        },
        "debug/running" => RuntimeEvent::Running { session_id: sid },
        "debug/paused" => {
            let stop = params.get("stop").cloned().unwrap_or(Value::Null);
            RuntimeEvent::Paused {
                session_id: sid,
                reason: s(&stop, "reason"),
                pc: u32f(&stop, "pc"),
                cycles: u64f(&stop, "cycles"),
            }
        }
        "debug/stopped" => {
            let stop = params.get("stop").cloned().unwrap_or(Value::Null);
            RuntimeEvent::Stopped {
                session_id: sid,
                reason: s(&stop, "reason"),
                pc: u32f(&stop, "pc"),
                cycles: u64f(&stop, "cycles"),
            }
        }
        "debug/breakpoint_hit" => RuntimeEvent::BreakpointHit {
            session_id: sid,
            pc: u32f(params, "pc"),
            num: u32f(params, "num"),
        },
        "debug/observer_hit" => RuntimeEvent::ObserverHit {
            session_id: sid,
            name: s(params, "name"),
        },
        "debug/observer_log" => RuntimeEvent::ObserverLog {
            session_id: sid,
            message: s(params, "message"),
        },
        "debug/checkpoint_restored" => RuntimeEvent::CheckpointRestored {
            session_id: sid,
            id: s(params, "id"),
        },
        "debug/control" => RuntimeEvent::ControlChanged {
            session_id: sid,
            control_owner: s(params, "controlOwner"),
        },
        "audio/flush" => RuntimeEvent::AudioFlush { session_id: sid },
        "media/cart_persisted" => RuntimeEvent::MediaChanged { session_id: sid },
        "batch/progress" => RuntimeEvent::BatchProgress {
            params_json: params.to_string(),
        },
        other => RuntimeEvent::Other {
            method: other.to_string(),
            params_json: params.to_string(),
        },
    }
}

/// Parse one `NotifyHub` wire message → `(method, params)`. Only `Message::Text`
/// carries notifications; binary frames (A/V) never arrive on the FFI channel.
fn parse_envelope(msg: Message) -> Option<(String, Value)> {
    let text = match msg {
        Message::Text(t) => t.to_string(),
        _ => return None,
    };
    let v: Value = serde_json::from_str(&text).ok()?;
    let method = v.get("method")?.as_str()?.to_string();
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    Some((method, params))
}

/// The forwarder loop body — runs on a dedicated OS thread. Blocks on the channel,
/// maps each envelope to a typed event, calls the listener. Exits when the channel
/// closes (subscription dropped) or `stop` is set.
pub(crate) fn forward_loop(
    mut rx: UnboundedReceiver<Message>,
    listener: Arc<dyn EventListener>,
    stop: Arc<AtomicBool>,
) {
    while let Some(msg) = rx.blocking_recv() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if let Some((method, params)) = parse_envelope(msg) {
            listener.on_event(map_event(&method, &params));
        }
    }
}
