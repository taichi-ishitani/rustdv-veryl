//! Run rustdv testbenches on Veryl's built-in simulator.
//!
//! The testbench crate is a Veryl verification component: it declares the
//! ports and interface ports it connects to the design, registers them with a
//! [`Host`] in `on_build`, and forwards its hooks to the host. The host
//! installs a rustdv [`Backend`] serving those ports. A component method
//! calling [`Host::start`] hands control to rustdv, like UVM's `run_test`:
//! the test module calls it, and the host runs the regression of every
//! `#[rustdv::test]` linked into the crate. When the regression ends the
//! Veryl test finishes, and a failing regression fails it.
//!
//! The component crate depends on `veryl-component`, `rustdv` (without its
//! default `vpi` feature) and this crate directly: the derives of both
//! frameworks refer to their crates by name.
//!
//! The example `examples/adder` of this crate's repository verifies an adder
//! through the `tb` modport of its interface `adder_if`. Its component
//! declares the modport, registers it with the host and forwards the hooks:
//!
//! ```ignore
//! use rustdv_veryl::Host;
//! use veryl_component::{
//!     BuildCtx, ClockPort, Component, InputPort, OutputPort, Result, SimCtx, VerylInterface,
//!     component_impl, veryl_component_export,
//! };
//!
//! /// The `tb` modport of `adder_if`.
//! #[derive(VerylInterface)]
//! #[interface(path = "adder_if", modport = "tb")]
//! struct AdderTb {
//!     start: OutputPort,
//!     a: OutputPort,
//!     b: OutputPort,
//!     done: InputPort,
//!     result: InputPort,
//! }
//!
//! /// Hosts the rustdv tests: the DUT scope `dut` holds `clk` and the bus
//! /// members under `bus`.
//! #[derive(Component)]
//! #[component(kind = clocked)]
//! pub struct RustdvTb {
//!     clk: ClockPort,
//!     #[interface]
//!     bus: AdderTb,
//!     host: Host,
//! }
//!
//! #[component_impl]
//! impl RustdvTb {
//!     fn on_build(&mut self, _ctx: &mut BuildCtx) -> Result<()> {
//!         self.host.clock("clk", self.clk);
//!         self.host
//!             .scope("bus")
//!             .output("start", self.bus.start)
//!             .output("a", self.bus.a)
//!             .output("b", self.bus.b)
//!             .input("done", self.bus.done)
//!             .input("result", self.bus.result);
//!         Ok(())
//!     }
//!
//!     fn on_init(&mut self, ctx: &mut SimCtx) -> Result<()> {
//!         self.host.init(ctx);
//!         Ok(())
//!     }
//!
//!     fn on_clock(&mut self, ctx: &mut SimCtx) -> Result<()> {
//!         self.host.clock_edge(ctx);
//!         Ok(())
//!     }
//!
//!     fn on_finish(&mut self, ctx: &mut SimCtx) -> Result<()> {
//!         self.host.finish(ctx);
//!         Ok(())
//!     }
//!
//!     /// Hands control to rustdv: runs the rustdv tests of this crate.
//!     fn start(&mut self, ctx: &mut SimCtx) -> Result<()> {
//!         self.host.start(ctx)
//!     }
//! }
//!
//! veryl_component_export!("rustdv_tb" => RustdvTb);
//! ```
//!
//! The test module connects the component next to the design and starts
//! rustdv from an `initial` block:
//!
//! ```veryl
//! #[test(adder_test)]
//! module adder_test {
//!     inst clk: $tb::clock_gen;
//!     inst bus: adder_if;
//!     inst dut: adder (
//!         i_clk: clk,
//!         bus       ,
//!     );
//!     inst tb: $comp::rustdv_tb (
//!         clk        ,
//!         bus: bus.tb,
//!     );
//!
//!     // tb.start() hands control to rustdv. The rustdv regression finishes the
//!     // test when it is done; clk.next bounds the run if it never does.
//!     initial {
//!         tb.start();
//!         clk.next(1000);
//!         $finish();
//!     }
//! }
//! ```
//!
//! rustdv then sees the design as one scope (`dut` by default) holding the
//! registered signals, with a nested scope per [`Scope::scope`]:
//! `ctx.dut().hierarchy("bus")?.signal("start")?`.
//!
//! # Timing
//!
//! Veryl's simulator is cycle based and a component only runs at its hooks,
//! so rustdv's simulator callbacks are emulated from them:
//!
//! - reads return the value seen by the hook: for `on_clock`, the value just
//!   before the edge
//! - writes commit together with the design's flip-flops at the current edge
//!   (NBA semantics), whatever delay mode rustdv asks for
//! - value changes of signals other than clocks are detected by comparing
//!   their values at each hook, so they are seen once per hook
//! - timers fire at the first hook at or after their deadline
//! - a clock port is modeled as a toggling signal: each `on_clock` raises it,
//!   preceded by a falling edge half-way since the previous rise
//!
//! A testbench therefore samples and drives on rising edges. Hierarchical
//! references into the design beyond the registered ports are not supported.
//!
//! One time step is 1 us, the timescale of Veryl's waveform dumps.
//!
//! The backend state is per thread: one [`Host`] per Veryl test.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::ops::{Deref, DerefMut};

use rustdv_gpi::ValueError;
use rustdv_gpi::backend::{self, Backend, CbData, CbToken, ObjHandle, ObjKind, Reason};
use rustdv_gpi_sys as sys;
use smallvec::SmallVec;
use veryl_component::{ClockPort, InputPort, OutputPort, ResetPort, SimCtx, Value};

/// Simulation time precision: one Veryl time unit is 1 us.
const TIME_PRECISION: i32 = -6;

// ===========================================================================
// Host
// ===========================================================================

#[derive(Copy, Clone)]
enum Port {
    Input(InputPort),
    Output(OutputPort),
    Clock(ClockPort),
    Reset(ResetPort),
}

enum Item {
    Signal { name: String, port: Port, width: u32 },
    Scope(Scope),
}

/// A scope of the design as rustdv sees it: named signals backed by the
/// component's ports, and nested scopes.
pub struct Scope {
    name: String,
    items: Vec<Item>,
}

impl Scope {
    fn new(name: impl Into<String>) -> Self {
        Scope {
            name: name.into(),
            items: Vec::new(),
        }
    }

    fn signal(&mut self, name: impl Into<String>, port: Port, width: u32) -> &mut Self {
        self.items.push(Item::Signal {
            name: name.into(),
            port,
            width,
        });
        self
    }

    /// Expose an input port as the signal `name`. rustdv can read it but not
    /// write it.
    pub fn input(&mut self, name: impl Into<String>, port: InputPort) -> &mut Self {
        self.signal(name, Port::Input(port), port.width())
    }

    /// Expose an output port as the signal `name`.
    pub fn output(&mut self, name: impl Into<String>, port: OutputPort) -> &mut Self {
        self.signal(name, Port::Output(port), port.width())
    }

    /// Expose a clock port as the signal `name`.
    pub fn clock(&mut self, name: impl Into<String>, port: ClockPort) -> &mut Self {
        self.signal(name, Port::Clock(port), 1)
    }

    /// Expose a reset port as the signal `name`.
    pub fn reset(&mut self, name: impl Into<String>, port: ResetPort) -> &mut Self {
        self.signal(name, Port::Reset(port), 1)
    }

    /// Add the nested scope `name`, e.g. for the members of an interface
    /// port, and return it.
    pub fn scope(&mut self, name: impl Into<String>) -> &mut Scope {
        self.items.push(Item::Scope(Scope::new(name)));
        let Some(Item::Scope(scope)) = self.items.last_mut() else {
            unreachable!("just pushed")
        };
        scope
    }
}

/// Hosts the rustdv regression inside a Veryl component: register the
/// component's ports in `on_build` (it derefs to the top [`Scope`]), forward
/// each hook to the matching method, and call [`Host::start`] from a
/// component method to run rustdv.
pub struct Host {
    top: Scope,
}

impl Default for Host {
    fn default() -> Self {
        Host::new("dut")
    }
}

impl Deref for Host {
    type Target = Scope;
    fn deref(&self) -> &Scope {
        &self.top
    }
}

impl DerefMut for Host {
    fn deref_mut(&mut self) -> &mut Scope {
        &mut self.top
    }
}

impl Host {
    /// A host whose top scope is named `top`.
    pub fn new(top: impl Into<String>) -> Self {
        Host {
            top: Scope::new(top),
        }
    }

    /// Forward `on_init`: install the backend on this thread. rustdv does not
    /// run until [`Host::start`].
    pub fn init(&self, _ctx: &mut SimCtx) {
        self.setup();
    }

    /// Hand control to rustdv: start the regression of every
    /// `#[rustdv::test]` linked into the crate. Call it from a component
    /// method that the test module calls, like UVM's `run_test`, after the
    /// testbench is set up. Fails if the regression already started.
    pub fn start(&self, ctx: &mut SimCtx) -> veryl_component::Result<()> {
        // A method may be called before `on_init`.
        self.setup();
        if with_state(|s| std::mem::replace(&mut s.started, true)).unwrap_or(false) {
            veryl_component::bail!("the rustdv regression has already started");
        }
        let now = ctx.time();
        let _hook = HookGuard::enter(ctx);
        advance(now);
        rustdv::runner::vpi_startup();
        fire(|w| matches!(w, Wait::StartOfSimulation));
        settle();
        Ok(())
    }

    /// Create the backend state and install the backend, once per test.
    fn setup(&self) {
        if with_state(|_| ()).is_some() {
            return;
        }
        let mut nodes = Vec::new();
        flatten(&self.top, None, &mut nodes);
        STATE.with(|s| {
            *s.borrow_mut() = Some(State {
                nodes,
                time: 0,
                pending: Vec::new(),
                started: false,
                finish: false,
                failure: None,
                ctx: std::ptr::null_mut(),
            })
        });
        backend::install(&VerylBackend);
    }

    /// Forward `on_clock`.
    pub fn clock_edge(&self, ctx: &mut SimCtx) {
        let now = ctx.time();
        let fired: Vec<usize> = with_state(|s| {
            s.nodes
                .iter()
                .enumerate()
                .filter_map(|(i, node)| match node.signal.as_ref()?.port {
                    Port::Clock(port) if ctx.fired(port) => Some(i),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
        let _hook = HookGuard::enter(ctx);

        // A falling edge half-way since the previous rise, so it gets a time
        // step of its own.
        for &i in &fired {
            let Some(rise) = with_state(|s| signal(s, i).last_rise).flatten() else {
                continue;
            };
            let fall = rise + (now - rise) / 2;
            advance(fall);
            set_clock(i, false);
            settle();
        }

        advance(now);
        sample_changes();
        settle();
        for &i in &fired {
            set_clock(i, true);
            with_state(|s| signal(s, i).last_rise = Some(now));
            settle();
        }
        fire(|w| matches!(w, Wait::AtEndOfSimTime));
    }

    /// Forward `on_reset`.
    pub fn reset_edge(&self, ctx: &mut SimCtx) {
        let now = ctx.time();
        let _hook = HookGuard::enter(ctx);
        advance(now);
        sample_changes();
        settle();
    }

    /// Forward `on_finish`: end the simulation and remove the backend. Fails
    /// the test if rustdv was never started.
    pub fn finish(&self, ctx: &mut SimCtx) {
        if !with_state(|s| s.started).unwrap_or(false) {
            ctx.fail("the rustdv regression was not started: call `Host::start`");
        } else {
            let _hook = HookGuard::enter(ctx);
            fire(|w| matches!(w, Wait::EndOfSimulation));
        }
        STATE.with(|s| *s.borrow_mut() = None);
        backend::uninstall();
    }
}

/// Number the nodes of `scope` depth-first; returns the scope's index.
fn flatten(scope: &Scope, parent: Option<&str>, nodes: &mut Vec<Node>) -> usize {
    let full_name = match parent {
        Some(parent) => format!("{parent}.{}", scope.name),
        None => scope.name.clone(),
    };
    let index = nodes.len();
    nodes.push(Node {
        name: scope.name.clone(),
        full_name: full_name.clone(),
        children: Vec::new(),
        signal: None,
    });
    for item in &scope.items {
        let (name, child) = match item {
            Item::Signal { name, port, width } => {
                let child = nodes.len();
                nodes.push(Node {
                    name: name.clone(),
                    full_name: format!("{full_name}.{name}"),
                    children: Vec::new(),
                    signal: Some(Signal {
                        port: *port,
                        width: *width,
                        value: None,
                        seen: None,
                        last_rise: None,
                    }),
                });
                (name, child)
            }
            Item::Scope(inner) => (&inner.name, flatten(inner, Some(&full_name), nodes)),
        };
        assert!(
            !nodes[index]
                .children
                .iter()
                .any(|&c| nodes[c].name == *name),
            "'{full_name}.{name}' is registered twice"
        );
        nodes[index].children.push(child);
    }
    index
}

// ===========================================================================
// Backend state
// ===========================================================================

/// A scope (no signal) or a signal of the design.
struct Node {
    name: String,
    full_name: String,
    children: Vec<usize>,
    signal: Option<Signal>,
}

struct Signal {
    port: Port,
    width: u32,
    /// Last value written (outputs) or the modeled level (clocks).
    value: Option<Vec<(u32, u32)>>,
    /// Value at the previous hook, for value-change detection.
    seen: Option<Vec<(u32, u32)>>,
    /// Time of the last rising edge (clocks).
    last_rise: Option<u64>,
}

/// What a pending callback waits for.
#[derive(Copy, Clone)]
enum Wait {
    At(u64),
    ValueChange(usize),
    ReadWrite,
    ReadOnly,
    NextSimTime,
    AtEndOfSimTime,
    StartOfSimulation,
    EndOfSimulation,
}

struct Pending {
    token: CbToken,
    wait: Wait,
    data: CbData,
}

struct State {
    nodes: Vec<Node>,
    time: u64,
    pending: Vec<Pending>,
    /// Whether [`Host::start`] has run.
    started: bool,
    /// `finish`/`report_failure` called outside a hook, applied at the next.
    finish: bool,
    failure: Option<String>,
    /// The context of the hook being run, or null between hooks.
    ctx: *mut SimCtx,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    // Outlives `State`, so that a stale handle from a previous test cannot
    // remove a callback of the next one.
    static NEXT_TOKEN: Cell<CbToken> = const { Cell::new(1) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

/// The signal of node `index`, which must be one.
fn signal(s: &mut State, index: usize) -> &mut Signal {
    s.nodes[index]
        .signal
        .as_mut()
        .expect("node is not a signal")
}

fn node_handle(index: usize) -> ObjHandle {
    ObjHandle::from_raw(index + 1)
}

fn node_index(h: ObjHandle) -> Option<usize> {
    h.as_raw().checked_sub(1)
}

/// The index of `h` if it names a node of the current design.
fn valid_node(h: ObjHandle) -> Option<usize> {
    let index = node_index(h)?;
    with_state(|s| index < s.nodes.len()).filter(|ok| *ok)?;
    Some(index)
}

/// The index of the signal `h`, which must be one.
fn signal_index(h: ObjHandle) -> usize {
    valid_node(h)
        .filter(|&i| with_state(|s| s.nodes[i].signal.is_some()).unwrap_or(false))
        .unwrap_or_else(|| panic!("'{}' is not a signal", VerylBackend.full_name(h)))
}

/// Run `f` with the context of the hook being run. `None` between hooks.
fn with_ctx<R>(f: impl FnOnce(&mut SimCtx) -> R) -> Option<R> {
    let ctx = with_state(|s| s.ctx).unwrap_or(std::ptr::null_mut());
    if ctx.is_null() {
        None
    } else {
        // The pointer is only set by `HookGuard` for the duration of a hook,
        // whose `&mut SimCtx` it was taken from.
        Some(f(unsafe { &mut *ctx }))
    }
}

/// Publishes the hook's context to the backend for the duration of the hook.
struct HookGuard;

impl HookGuard {
    fn enter(ctx: &mut SimCtx) -> HookGuard {
        let (finish, failure) = with_state(|s| {
            s.ctx = ctx;
            (std::mem::take(&mut s.finish), s.failure.take())
        })
        .expect("rustdv Veryl backend is not initialized: call Host::init from on_init");
        if let Some(msg) = failure {
            ctx.fail(msg);
        }
        if finish {
            ctx.finish();
        }
        HookGuard
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        with_state(|s| s.ctx = std::ptr::null_mut());
    }
}

/// Current value of a signal as VPI (aval, bval) words.
fn read_signal(index: usize) -> Option<Vec<(u32, u32)>> {
    let (port, width, value) = with_state(|s| {
        let sig = signal(s, index);
        (sig.port, sig.width, sig.value.clone())
    })?;
    let words = match port {
        Port::Input(port) => from_value(&with_ctx(|ctx| ctx.read(port))?, width),
        Port::Reset(port) => from_value(&with_ctx(|ctx| ctx.read(port))?, width),
        Port::Clock(_) | Port::Output(_) => {
            value.unwrap_or_else(|| vec![(0, 0); width.div_ceil(32) as usize])
        }
    };
    Some(words)
}

/// Veryl marks X as (payload 0, mask 1) and Z as (payload 1, mask 1); VPI
/// marks X as (aval 1, bval 1) and Z as (aval 0, bval 1).
fn from_value(value: &Value, width: u32) -> Vec<(u32, u32)> {
    let payload = value.as_slice();
    let mask = value.mask_xz();
    let half =
        |words: &[u64], i: usize| words.get(i / 2).map_or(0, |w| (w >> (32 * (i % 2))) as u32);
    (0..width.div_ceil(32) as usize)
        .map(|i| {
            let (p, m) = (half(payload, i), half(mask, i));
            (p ^ m, m)
        })
        .collect()
}

fn to_value(words: &[(u32, u32)], width: u32) -> Value {
    let mut payload: SmallVec<[u64; 2]> = SmallVec::new();
    let mut mask: SmallVec<[u64; 2]> = SmallVec::new();
    for pair in words.chunks(2) {
        let word = |f: fn(&(u32, u32)) -> u32| {
            pair.iter()
                .enumerate()
                .fold(0u64, |acc, (i, w)| acc | ((f(w) as u64) << (32 * i)))
        };
        payload.push(word(|&(aval, bval)| aval ^ bval));
        mask.push(word(|&(_, bval)| bval));
    }
    Value::from_bits(payload, mask, width)
}

// ===========================================================================
// Scheduling
// ===========================================================================

/// Run every pending callback matching `pred`, in registration order.
/// Returns whether any ran. Callbacks registered meanwhile wait for the next
/// call.
fn fire(pred: impl Fn(&Wait) -> bool) -> bool {
    let due: Vec<(CbToken, CbData)> = with_state(|s| {
        s.pending
            .iter()
            .filter(|p| pred(&p.wait))
            .map(|p| (p.token, p.data))
            .collect()
    })
    .unwrap_or_default();
    for &(token, data) in &due {
        // An earlier callback of this batch may have removed it.
        let live = with_state(|s| s.pending.iter().any(|p| p.token == token)).unwrap_or(false);
        if live {
            // Still registered, so its data is live.
            unsafe { data.dispatch() };
        }
    }
    !due.is_empty()
}

/// Run ReadWrite, then ReadOnly callbacks until none is pending.
fn settle() {
    loop {
        if fire(|w| matches!(w, Wait::ReadWrite)) {
            continue;
        }
        if fire(|w| matches!(w, Wait::ReadOnly)) {
            continue;
        }
        break;
    }
}

/// Move time forward to `now`, running each timer due by then at its own
/// deadline.
fn advance(now: u64) {
    loop {
        let next = with_state(|s| {
            s.pending
                .iter()
                .filter_map(|p| match p.wait {
                    Wait::At(t) if t <= now => Some(t),
                    _ => None,
                })
                .min()
        })
        .flatten();
        let Some(t) = next else { break };
        step_to(t);
        fire(|w| matches!(w, Wait::At(d) if *d == t));
        settle();
    }
    step_to(now);
}

fn step_to(t: u64) {
    let moved = with_state(|s| {
        let moved = t > s.time;
        s.time = s.time.max(t);
        moved
    })
    .unwrap_or(false);
    if moved {
        fire(|w| matches!(w, Wait::NextSimTime));
        settle();
    }
}

fn set_clock(index: usize, level: bool) {
    with_state(|s| signal(s, index).value = Some(vec![(level as u32, 0)]));
    fire(|w| matches!(w, Wait::ValueChange(i) if *i == index));
}

/// Run value-change callbacks of the non-clock signals that changed since
/// the previous hook.
fn sample_changes() {
    let watched: Vec<usize> = with_state(|s| {
        let mut watched: Vec<usize> = s
            .pending
            .iter()
            .filter_map(|p| match p.wait {
                Wait::ValueChange(i) => Some(i),
                _ => None,
            })
            .filter(|&i| {
                !matches!(
                    s.nodes[i].signal.as_ref().map(|sig| sig.port),
                    Some(Port::Clock(_))
                )
            })
            .collect();
        watched.sort_unstable();
        watched.dedup();
        // A signal no longer watched restarts from its value when watched
        // again.
        for (i, node) in s.nodes.iter_mut().enumerate() {
            if let Some(sig) = node.signal.as_mut()
                && watched.binary_search(&i).is_err()
            {
                sig.seen = None;
            }
        }
        watched
    })
    .unwrap_or_default();
    for i in watched {
        let Some(now) = read_signal(i) else { continue };
        let changed = with_state(|s| {
            let sig = signal(s, i);
            let changed = sig.seen.as_ref().is_some_and(|seen| *seen != now);
            sig.seen = Some(now);
            changed
        })
        .unwrap_or(false);
        if changed {
            fire(|w| matches!(w, Wait::ValueChange(j) if *j == i));
        }
    }
}

// ===========================================================================
// Backend
// ===========================================================================

/// The rustdv backend served by [`Host`].
struct VerylBackend;

impl Backend for VerylBackend {
    fn top_modules(&self) -> Vec<ObjHandle> {
        with_state(|s| s.nodes.len())
            .filter(|&n| n > 0)
            .map(|_| vec![node_handle(0)])
            .unwrap_or_default()
    }

    fn handle_by_name(&self, name: &str) -> Option<ObjHandle> {
        with_state(|s| s.nodes.iter().position(|n| n.full_name == name))
            .flatten()
            .map(node_handle)
    }

    fn kind(&self, h: ObjHandle) -> ObjKind {
        let Some(index) = valid_node(h) else {
            return ObjKind::Other;
        };
        with_state(|s| match &s.nodes[index].signal {
            Some(sig) => ObjKind::Logic { width: sig.width },
            None => ObjKind::Hierarchy,
        })
        .unwrap_or(ObjKind::Other)
    }

    fn name(&self, h: ObjHandle) -> String {
        valid_node(h)
            .and_then(|i| with_state(|s| s.nodes[i].name.clone()))
            .unwrap_or_default()
    }

    fn full_name(&self, h: ObjHandle) -> String {
        valid_node(h)
            .and_then(|i| with_state(|s| s.nodes[i].full_name.clone()))
            .unwrap_or_default()
    }

    fn type_desc(&self, h: ObjHandle) -> String {
        match self.kind(h) {
            ObjKind::Hierarchy => "scope".to_owned(),
            ObjKind::Logic { .. } => "signal".to_owned(),
            _ => "null handle".to_owned(),
        }
    }

    /// `name` may be a dotted path into nested scopes.
    fn child(&self, scope: ObjHandle, name: &str) -> Option<ObjHandle> {
        let mut index = valid_node(scope)?;
        for part in name.split('.') {
            index = with_state(|s| {
                s.nodes[index]
                    .children
                    .iter()
                    .copied()
                    .find(|&c| s.nodes[c].name == part)
            })
            .flatten()?;
        }
        Some(node_handle(index))
    }

    fn children(&self, scope: ObjHandle) -> Vec<ObjHandle> {
        valid_node(scope)
            .and_then(|i| with_state(|s| s.nodes[i].children.clone()))
            .unwrap_or_default()
            .into_iter()
            .map(node_handle)
            .collect()
    }

    fn get_vector(
        &self,
        h: ObjHandle,
        width: u32,
    ) -> Result<Cow<'_, [sys::t_vpi_vecval]>, ValueError> {
        let index = signal_index(h);
        let words = read_signal(index).ok_or(ValueError::Unavailable)?;
        let mut words: Vec<sys::t_vpi_vecval> = words
            .into_iter()
            .map(|(aval, bval)| sys::t_vpi_vecval { aval, bval })
            .collect();
        words.resize(width.div_ceil(32) as usize, sys::t_vpi_vecval::default());
        Ok(Cow::Owned(words))
    }

    /// Writes commit together with the design's flip-flops, whatever `flags`
    /// asks.
    fn put_vector(&self, h: ObjHandle, words: &mut [sys::t_vpi_vecval], _flags: i32) {
        let index = signal_index(h);
        let words: Vec<(u32, u32)> = words.iter().map(|w| (w.aval, w.bval)).collect();
        let (port, width) = with_state(|s| {
            let sig = signal(s, index);
            (sig.port, sig.width)
        })
        .expect("rustdv Veryl backend is not initialized");
        let Port::Output(port) = port else {
            panic!(
                "cannot write '{}': it is an input of the component",
                self.full_name(h)
            );
        };
        with_ctx(|ctx| ctx.write(port, to_value(&words, width)))
            .unwrap_or_else(|| panic!("cannot write '{}' outside a hook", self.full_name(h)));
        with_state(|s| signal(s, index).value = Some(words));
    }

    fn sim_time_steps(&self) -> u64 {
        with_state(|s| s.time).unwrap_or(0)
    }

    fn time_precision(&self) -> i32 {
        TIME_PRECISION
    }

    /// Request the end of the test at the end of the current cycle.
    fn finish(&self) {
        if with_ctx(|ctx| ctx.finish()).is_none() {
            with_state(|s| s.finish = true);
        }
    }

    /// Mark the Veryl test as failed.
    fn report_failure(&self, msg: &str) {
        if with_ctx(|ctx| ctx.fail(msg)).is_none() {
            with_state(|s| s.failure = Some(msg.to_owned()));
        }
    }

    /// Write one line to the Veryl test output.
    fn console(&self, line: &str) {
        if with_ctx(|ctx| ctx.log(line)).is_none() {
            println!("{line}");
        }
    }

    fn register_cb(&self, reason: Reason, data: CbData) -> CbToken {
        let token = NEXT_TOKEN.with(|n| {
            let token = n.get();
            n.set(token + 1);
            token
        });
        let wait = match reason {
            Reason::AfterDelay(steps) => Wait::At(self.sim_time_steps() + steps),
            Reason::ValueChange(sig) => Wait::ValueChange(signal_index(sig)),
            Reason::ReadWrite => Wait::ReadWrite,
            Reason::ReadOnly => Wait::ReadOnly,
            Reason::NextSimTime => Wait::NextSimTime,
            Reason::AtEndOfSimTime => Wait::AtEndOfSimTime,
            Reason::StartOfSimulation => Wait::StartOfSimulation,
            Reason::EndOfSimulation => Wait::EndOfSimulation,
        };
        with_state(|s| s.pending.push(Pending { token, wait, data }))
            .expect("rustdv Veryl backend is not initialized: call Host::init from on_init");
        // Start watching from the current value.
        if let Wait::ValueChange(index) = wait
            && with_state(|s| signal(s, index).seen.is_none()).unwrap_or(false)
        {
            let now = read_signal(index);
            with_state(|s| signal(s, index).seen = now);
        }
        token
    }

    fn remove_cb(&self, token: CbToken) {
        with_state(|s| s.pending.retain(|p| p.token != token));
    }
}
