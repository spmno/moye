//! Waterfall listener registry (todo 11) — full register/unregister/emit/
//! waterfall/serial listener system layered on top of rig 0.41's `AgentHook`
//! before/after callbacks.
//!
//! Two-layer design (GAP-1 fix):
//! 1. **rig hook layer**: `HitlHook`/`ContextHook` dispatch lifecycle events to
//!    a `WaterfallRegistry`. `on_tool_call` (pre) and `on_tool_result` (post)
//!    dispatch via `emit` (fire-and-forget) + `waterfall` (sequential, short-
//!    circuit eligible). rig 0.41's `AgentHook` is NOT next()-style middleware.
//! 2. **around-execute layer**: `TimeoutRetryTool` (todo 9, untouched).
//!
//! Three listener kinds:
//! - `emit`: observe-only, no return value (logging / SessionLog).
//! - `waterfall`: sequential; `next()` delegates to the next listener; returning
//!   without calling `next()` short-circuits. ONLY for pre/post stages.
//! - `serial`: `await`-based, for async listeners.
//!
//! Thread-safe: interior mutability via `Mutex<Vec<...>>`. Listeners stored as
//! `Arc<dyn Fn...>` so the registry can snapshot under the lock and iterate
//! without re-entrant locking (the recursive `next()` chain would otherwise
//! deadlock).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use serde_json::Value;

// ---------------------------------------------------------------------------
// Event / action types
// ---------------------------------------------------------------------------

/// Lifecycle event points where listeners can observe or short-circuit the
/// agent loop. `ToolsPreExecute` / `ToolsPostExecute` are waterfall-eligible
/// (pre/post stages); the rest are emit/serial only.
#[derive(Debug, Clone, PartialEq)]
pub enum WaterfallEvent {
    /// Agent step is starting (role + goal).
    AgentPreStep { role: String, goal: String },
    /// Agent is making an API request.
    AgentRequest { message: String },
    /// Tool is about to execute (pre stage — short-circuit eligible).
    ToolsPreExecute { tool_name: String, args: Value },
    /// Tool finished executing (post stage — short-circuit eligible).
    ToolsPostExecute {
        tool_name: String,
        result: String,
        ok: bool,
    },
    /// Agent turn is stopping.
    AgentTurnStopping { reason: String },
}

/// Action returned by waterfall listeners. `Continue` = call `next()` to
/// delegate; `ShortCircuit` = return without calling `next()`, halting the
/// chain (later listeners do NOT run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // infrastructure for future phases
pub enum WaterfallAction {
    Continue,
    ShortCircuit,
}

/// Opaque listener handle — returned by `register_*`, accepted by `unregister`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenerId(usize);

// ---------------------------------------------------------------------------
// Listener function type aliases (Arc — clonable for lock-free snapshot)
// ---------------------------------------------------------------------------

type EmitFn = Arc<dyn Fn(&WaterfallEvent) + Send + Sync>;
type WaterfallFn =
    Arc<dyn Fn(&WaterfallEvent, &dyn Fn() -> WaterfallAction) -> WaterfallAction + Send + Sync>;
type SerialFn =
    Arc<dyn Fn(&WaterfallEvent) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

struct EmitListener {
    #[allow(dead_code)] // used in tests
    id: usize,
    f: EmitFn,
}
struct WaterfallListener {
    #[allow(dead_code)] // used in tests
    id: usize,
    f: WaterfallFn,
}
struct SerialListener {
    #[allow(dead_code)] // used in tests
    id: usize,
    f: SerialFn,
}

struct RegistryInner {
    emit: Vec<EmitListener>,
    waterfall: Vec<WaterfallListener>,
    serial: Vec<SerialListener>,
}

/// Thread-safe registry of emit / waterfall / serial listeners.
///
/// Simplified: `Vec` + `register`/`unregister` (LIFO unwind — callers typically
/// unregister in reverse registration order). NOT a full Cordis fiber runtime.
pub struct WaterfallRegistry {
    inner: Mutex<RegistryInner>,
    counter: Mutex<usize>,
}

impl Default for WaterfallRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WaterfallRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                emit: Vec::new(),
                waterfall: Vec::new(),
                serial: Vec::new(),
            }),
            counter: Mutex::new(0),
        }
    }

    fn next_id(&self) -> usize {
        let mut g = self.counter.lock().unwrap();
        *g += 1;
        *g
    }

    /// Register an emit (observe-only) listener. Returns a handle for
    /// `unregister`. Emit listeners fire on every `emit(event)` call.
    #[allow(dead_code)] // infrastructure for future phases
    pub fn register_emit<F>(&self, f: F) -> ListenerId
    where
        F: Fn(&WaterfallEvent) + Send + Sync + 'static,
    {
        let id = self.next_id();
        self.inner
            .lock()
            .unwrap()
            .emit
            .push(EmitListener { id, f: Arc::new(f) });
        ListenerId(id)
    }

    /// Register a waterfall listener. Each listener receives `(event, next)`.
    /// Call `next()` to delegate to the next listener; return without calling
    /// `next()` to short-circuit. ONLY for pre/post stages.
    #[allow(dead_code)] // infrastructure for future phases
    pub fn register_waterfall<F>(&self, f: F) -> ListenerId
    where
        F: Fn(&WaterfallEvent, &dyn Fn() -> WaterfallAction) -> WaterfallAction
            + Send
            + Sync
            + 'static,
    {
        let id = self.next_id();
        self.inner
            .lock()
            .unwrap()
            .waterfall
            .push(WaterfallListener { id, f: Arc::new(f) });
        ListenerId(id)
    }

    /// Register a serial (async) listener. `serial(event)` awaits each in order.
    pub fn register_serial<F>(&self, f: F) -> ListenerId
    where
        F: Fn(&WaterfallEvent) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync + 'static,
    {
        let id = self.next_id();
        self.inner
            .lock()
            .unwrap()
            .serial
            .push(SerialListener { id, f: Arc::new(f) });
        ListenerId(id)
    }

    /// Unregister a listener by id. Returns true if found and removed.
    /// LIFO unwind: callers typically unregister in reverse order; `remove(pos)`
    /// is O(1) when the target is at the tail.
    #[allow(dead_code)] // used in tests
    pub fn unregister(&self, id: ListenerId) -> bool {
        let mut g = self.inner.lock().unwrap();
        if let Some(pos) = g.emit.iter().position(|l| l.id == id.0) {
            g.emit.remove(pos);
            return true;
        }
        if let Some(pos) = g.waterfall.iter().position(|l| l.id == id.0) {
            g.waterfall.remove(pos);
            return true;
        }
        if let Some(pos) = g.serial.iter().position(|l| l.id == id.0) {
            g.serial.remove(pos);
            return true;
        }
        false
    }

    /// Fire-and-forget: call all emit listeners with the event.
    /// Snapshot under the lock, iterate without holding it (safe for listeners
    /// that re-enter the registry — e.g., via `unregister`).
    pub fn emit(&self, event: &WaterfallEvent) {
        let snapshot: Vec<EmitFn> = {
            let g = self.inner.lock().unwrap();
            g.emit.iter().map(|l| l.f.clone()).collect()
        };
        for f in &snapshot {
            f(event);
        }
    }

    /// Sequential waterfall: each listener gets `(event, next)`. If it calls
    /// `next()`, the rest of the chain runs. If it returns without calling
    /// `next()`, the chain short-circuits. Default (no listeners) → `Continue`.
    pub fn waterfall(&self, event: &WaterfallEvent) -> WaterfallAction {
        let snapshot: Vec<WaterfallFn> = {
            let g = self.inner.lock().unwrap();
            g.waterfall.iter().map(|l| l.f.clone()).collect()
        };
        walk_waterfall(&snapshot, event)
    }

    /// Async serial dispatch: await each serial listener in order.
    pub async fn serial(&self, event: &WaterfallEvent) {
        let snapshot: Vec<SerialFn> = {
            let g = self.inner.lock().unwrap();
            g.serial.iter().map(|l| l.f.clone()).collect()
        };
        for f in &snapshot {
            f(event).await;
        }
    }

    /// Counts: `(emit, waterfall, serial)`.
    #[allow(dead_code)] // used in tests
    pub fn len(&self) -> (usize, usize, usize) {
        let g = self.inner.lock().unwrap();
        (g.emit.len(), g.waterfall.len(), g.serial.len())
    }

    #[allow(dead_code)] // used in tests
    pub fn is_empty(&self) -> bool {
        let (e, w, s) = self.len();
        e == 0 && w == 0 && s == 0
    }
}

/// Recursive chain walker: listener[0] gets `(event, next)` where `next` runs
/// the tail. Listener's return value IS the final action of the chain.
fn walk_waterfall(listeners: &[WaterfallFn], event: &WaterfallEvent) -> WaterfallAction {
    if listeners.is_empty() {
        return WaterfallAction::Continue;
    }
    let (head, tail) = listeners.split_first().expect("non-empty checked");
    let next = || walk_waterfall(tail, event);
    head(event, &next)
}

/// Shared state for `AgentPreStep` listeners to communicate with the calling
/// pipeline (todo 12). Listeners read/write these fields; `run_autonomous`
/// checks them after dispatching `AgentPreStep` to decide whether to
/// short-circuit (escape hatch) or use a modified goal (plan injection).
pub struct PreStepState {
    pub escape: Arc<AtomicBool>,
    pub investigation: Arc<Mutex<Option<String>>>,
    pub plan: Arc<Mutex<Option<String>>>,
    pub goal_override: Arc<Mutex<Option<String>>>,
    pub error: Arc<Mutex<Option<String>>>,
}

impl Default for PreStepState {
    fn default() -> Self {
        Self::new()
    }
}

impl PreStepState {
    pub fn new() -> Self {
        Self {
            escape: Arc::new(AtomicBool::new(false)),
            investigation: Arc::new(Mutex::new(None)),
            plan: Arc::new(Mutex::new(None)),
            goal_override: Arc::new(Mutex::new(None)),
            error: Arc::new(Mutex::new(None)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn pre_event() -> WaterfallEvent {
        WaterfallEvent::ToolsPreExecute {
            tool_name: "read_file".into(),
            args: serde_json::json!({"path": "x"}),
        }
    }

    fn post_event() -> WaterfallEvent {
        WaterfallEvent::ToolsPostExecute {
            tool_name: "read_file".into(),
            result: "ok".into(),
            ok: true,
        }
    }

    // ==================== emit ====================

    #[test]
    fn register_emit_triggers_listener() {
        let reg = WaterfallRegistry::new();
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        reg.register_emit(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        reg.emit(&pre_event());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn multiple_emit_listeners_all_fire() {
        let reg = WaterfallRegistry::new();
        let a = Arc::new(AtomicU32::new(0));
        let b = Arc::new(AtomicU32::new(0));
        let a1 = a.clone();
        let b1 = b.clone();
        reg.register_emit(move |_| {
            a1.fetch_add(1, Ordering::SeqCst);
        });
        reg.register_emit(move |_| {
            b1.fetch_add(1, Ordering::SeqCst);
        });
        reg.emit(&pre_event());
        assert_eq!(a.load(Ordering::SeqCst), 1);
        assert_eq!(b.load(Ordering::SeqCst), 1);
    }

    // ==================== waterfall ====================

    #[test]
    fn waterfall_empty_returns_continue() {
        let reg = WaterfallRegistry::new();
        assert_eq!(reg.waterfall(&pre_event()), WaterfallAction::Continue);
    }

    #[test]
    fn waterfall_calls_next_delegates_to_end() {
        let reg = WaterfallRegistry::new();
        let order = Arc::new(Mutex::new(Vec::<u32>::new()));
        let o1 = order.clone();
        reg.register_waterfall(move |_e, next| {
            o1.lock().unwrap().push(1);
            next()
        });
        let o2 = order.clone();
        reg.register_waterfall(move |_e, next| {
            o2.lock().unwrap().push(2);
            next()
        });
        let action = reg.waterfall(&pre_event());
        let recorded = order.lock().unwrap().clone();
        assert_eq!(recorded, vec![1, 2], "both listeners must run in order");
        assert_eq!(action, WaterfallAction::Continue);
    }

    #[test]
    fn waterfall_short_circuits_without_next() {
        let reg = WaterfallRegistry::new();
        let second_called = Arc::new(AtomicU32::new(0));
        let s = second_called.clone();
        reg.register_waterfall(move |_e, _next| WaterfallAction::ShortCircuit);
        reg.register_waterfall(move |_e, _next| {
            s.fetch_add(1, Ordering::SeqCst);
            WaterfallAction::Continue
        });
        let action = reg.waterfall(&pre_event());
        assert_eq!(action, WaterfallAction::ShortCircuit);
        assert_eq!(
            second_called.load(Ordering::SeqCst),
            0,
            "later listeners must NOT run after short-circuit"
        );
    }

    #[test]
    fn waterfall_short_circuit_does_not_override_return() {
        // A short-circuiting first listener must prevent the second listener's
        // return value from propagating. The chain's result is ShortCircuit.
        let reg = WaterfallRegistry::new();
        reg.register_waterfall(|_e, _next| WaterfallAction::ShortCircuit);
        reg.register_waterfall(|_e, _next| WaterfallAction::Continue);
        assert_eq!(reg.waterfall(&pre_event()), WaterfallAction::ShortCircuit);
    }

    // ==================== unregister ====================

    #[test]
    fn unregister_stops_emit_triggering() {
        let reg = WaterfallRegistry::new();
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let id = reg.register_emit(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        reg.emit(&pre_event());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(reg.unregister(id));
        reg.emit(&pre_event());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "listener must NOT fire after unregister"
        );
    }

    #[test]
    fn unregister_waterfall_passthrough() {
        let reg = WaterfallRegistry::new();
        let id = reg.register_waterfall(|_e, _next| WaterfallAction::ShortCircuit);
        assert_eq!(reg.waterfall(&pre_event()), WaterfallAction::ShortCircuit);
        assert!(reg.unregister(id));
        assert_eq!(
            reg.waterfall(&pre_event()),
            WaterfallAction::Continue,
            "after unregister, default is Continue (passthrough)"
        );
    }

    #[test]
    fn unregister_missing_returns_false() {
        let reg = WaterfallRegistry::new();
        assert!(!reg.unregister(ListenerId(999)));
    }

    // ==================== serial ====================

    #[tokio::test]
    async fn serial_listener_awaits() {
        let reg = WaterfallRegistry::new();
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        reg.register_serial(move |_e| {
            let c = c.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        });
        reg.serial(&pre_event()).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn serial_multiple_listeners_run_in_order() {
        let reg = WaterfallRegistry::new();
        let order = Arc::new(Mutex::new(Vec::<u32>::new()));
        let o1 = order.clone();
        reg.register_serial(move |_e| {
            let o1 = o1.clone();
            Box::pin(async move {
                o1.lock().unwrap().push(1);
            })
        });
        let o2 = order.clone();
        reg.register_serial(move |_e| {
            let o2 = o2.clone();
            Box::pin(async move {
                o2.lock().unwrap().push(2);
            })
        });
        reg.serial(&post_event()).await;
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    }

    // ── todo 12: AgentPreStep + PreStepState tests ──

    fn agent_pre_step(role: &str, goal: &str) -> WaterfallEvent {
        WaterfallEvent::AgentPreStep {
            role: role.into(),
            goal: goal.into(),
        }
    }

    #[tokio::test]
    async fn agent_pre_step_serial_listener_fires() {
        let reg = WaterfallRegistry::new();
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        reg.register_serial(move |e| {
            let is_pre_step = matches!(e, WaterfallEvent::AgentPreStep { .. });
            let c = c.clone();
            Box::pin(async move {
                if is_pre_step {
                    c.fetch_add(1, Ordering::SeqCst);
                }
            })
        });
        reg.serial(&agent_pre_step("builder", "do task")).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn agent_pre_step_listener_filters_by_role() {
        let reg = WaterfallRegistry::new();
        let builder_hits = Arc::new(AtomicU32::new(0));
        let investigator_hits = Arc::new(AtomicU32::new(0));
        let bh = builder_hits.clone();
        reg.register_serial(move |e| {
            let is_builder = matches!(
                e,
                WaterfallEvent::AgentPreStep { role, .. } if role == "builder"
            );
            let bh = bh.clone();
            Box::pin(async move {
                if is_builder {
                    bh.fetch_add(1, Ordering::SeqCst);
                }
            })
        });
        let ih = investigator_hits.clone();
        reg.register_serial(move |e| {
            let is_investigator = matches!(
                e,
                WaterfallEvent::AgentPreStep { role, .. } if role == "investigator"
            );
            let ih = ih.clone();
            Box::pin(async move {
                if is_investigator {
                    ih.fetch_add(1, Ordering::SeqCst);
                }
            })
        });
        reg.serial(&agent_pre_step("builder", "task")).await;
        reg.serial(&agent_pre_step("investigator", "explore")).await;
        assert_eq!(builder_hits.load(Ordering::SeqCst), 1);
        assert_eq!(investigator_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pre_step_state_escape_and_goal_override() {
        let ps = PreStepState::new();
        assert!(!ps.escape.load(Ordering::Relaxed));
        assert!(ps.goal_override.lock().unwrap().is_none());

        ps.escape.store(true, Ordering::Relaxed);
        *ps.goal_override.lock().unwrap() = Some("overridden goal".into());

        assert!(ps.escape.load(Ordering::Relaxed));
        assert_eq!(
            ps.goal_override.lock().unwrap().as_deref(),
            Some("overridden goal")
        );
    }

    #[tokio::test]
    async fn pre_step_state_error_take() {
        let ps = PreStepState::new();
        assert!(ps.error.lock().unwrap().is_none());
        *ps.error.lock().unwrap() = Some("investigation failed".into());
        let taken = ps.error.lock().unwrap().take();
        assert_eq!(taken, Some("investigation failed".to_string()));
        assert!(ps.error.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn agent_pre_step_serial_listeners_in_order_investigator_then_planner() {
        let reg = WaterfallRegistry::new();
        let order = Arc::new(Mutex::new(Vec::<&str>::new()));
        let o1 = order.clone();
        reg.register_serial(move |_e| {
            let o1 = o1.clone();
            Box::pin(async move {
                o1.lock().unwrap().push("investigator");
            })
        });
        let o2 = order.clone();
        reg.register_serial(move |_e| {
            let o2 = o2.clone();
            Box::pin(async move {
                o2.lock().unwrap().push("planner");
            })
        });
        reg.serial(&agent_pre_step("builder", "implement feature"))
            .await;
        assert_eq!(*order.lock().unwrap(), vec!["investigator", "planner"]);
    }
}
