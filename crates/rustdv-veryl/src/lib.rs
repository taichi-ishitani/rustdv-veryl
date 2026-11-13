//! Run rustdv testbenches on Veryl's built-in simulator.
//!
//! A component crate holding `#[rustdv::test]`s invokes [`export!`] once,
//! which makes it a Veryl verification component (`$comp::<name>`). The test
//! module instantiates it without ports and hands it the design through its
//! methods — clocks, resets, signals and modports, passed by reference — then
//! calls `start`, which hands control to rustdv like UVM's `run_test`. The
//! regression of every `#[rustdv::test]` in the crate then runs; when it ends
//! the Veryl test finishes, and a failing regression fails it.
//!
//! ```ignore
//! rustdv_veryl::export!("rustdv_tb");
//!
//! #[rustdv::test]
//! async fn add(ctx: RustdvCtx) -> Result<(), TestError> { /* ... */ }
//! ```
//!
//! ```veryl
//! inst tb: $comp::rustdv_tb;
//!
//! initial {
//!     tb.set_clock("clk", clk);
//!     tb.set_interface("bus", bus.tb);
//!     tb.start();
//!     clk.next(1000);
//!     $finish();
//! }
//! ```
//!
//! rustdv sees the design as one scope, `dut`, holding what was registered:
//! `dut.clk`, `dut.bus.start`, ... A dotted name (`"core.bus"`) nests scopes.
//! [Clocking blocks](mod@clocking) read and write those signals as struct
//! fields.
//!
//! The component crate depends on `veryl-component`, `rustdv` (without its
//! default `vpi` feature) and this crate directly: the generated code and the
//! macros of both frameworks refer to their crates by name.
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
//! - a clock is modeled as a toggling signal: each `on_clock` it fires raises
//!   it, preceded by a falling edge half-way since the previous rise
//! - during a `rst.assert()` window the component gets no `on_clock` (only
//!   `on_reset`, for a registered reset), so rustdv's clocks do not tick until
//!   the reset is released
//!
//! A testbench therefore samples and drives on rising edges, as a clocking
//! block does. rustdv sees only what the test module registered.
//!
//! One time step is 1 us, the timescale of Veryl's waveform dumps.
//!
//! The backend state is per thread: one [`Host`] per Veryl test.

pub mod clocking;

pub use clocking::{ClockingTimeout, SignalValue};
pub use rustdv_veryl_macros::{
    changed, changed_within, clocking, edge, edge_within, modport, negedge, negedge_within,
    posedge, posedge_within,
};

use std::borrow::Cow;
use std::cell::{Cell, RefCell};

use rustdv_gpi::ValueError;
use rustdv_gpi::backend::{self, Backend, CbData, CbToken, ObjHandle, ObjKind, Reason};
use rustdv_gpi_sys as sys;
use smallvec::SmallVec;
use veryl_component::{
    ClockPort, InputPort, MemberPort, ModportRef, OutputPort, ResetPort, SimCtx, Value,
};

/// Simulation time precision: one Veryl time unit is 1 us.
const TIME_PRECISION: i32 = -6;

// ===========================================================================
// The testbench component
// ===========================================================================

/// Exports the rustdv testbench component as `$comp::<name>`. Invoke it once
/// in the component crate, next to the `#[rustdv::test]`s it runs:
///
/// ```ignore
/// rustdv_veryl::export!("rustdv_tb");
/// ```
///
/// The component has no ports. The test module hands it the design through
/// its methods, then starts rustdv:
///
/// ```veryl
/// inst tb: $comp::rustdv_tb;
///
/// initial {
///     tb.set_clock("clk", clk);
///     tb.set_interface("bus", bus.tb);
///     tb.start();
///     clk.next(1000);
///     $finish();
/// }
/// ```
///
/// - `set_clock(name, clk)`, `set_reset(name, rst)`: a clock or a reset
/// - `set_input(name, x)`, `set_output(name, y)`: a signal rustdv reads or
///   drives
/// - `set_interface(name, bus.modport)`: the members of a modport, under the
///   scope `name`
/// - `start()`: hands control to rustdv, like UVM's `run_test`
///
/// rustdv sees each one as `dut.<name>`; a dotted name (`"core.bus"`) nests
/// it in scopes. Everything is registered before `start`. The crate depends
/// on `veryl-component` directly, which the generated code names.
#[macro_export]
macro_rules! export {
    ($name:literal) => {
        /// The rustdv testbench: runs the crate's `#[rustdv::test]`s on the
        /// clocks, resets, signals and interfaces the test module passes it.
        #[derive(::veryl_component::Component)]
        #[component(kind = clocked)]
        pub struct Rustdv {
            host: $crate::Host,
        }

        #[::veryl_component::component_impl]
        impl Rustdv {
            fn on_init(
                &mut self,
                ctx: &mut ::veryl_component::SimCtx,
            ) -> ::veryl_component::Result<()> {
                self.host.init(ctx);
                ::core::result::Result::Ok(())
            }

            fn on_clock(
                &mut self,
                ctx: &mut ::veryl_component::SimCtx,
            ) -> ::veryl_component::Result<()> {
                self.host.clock_edge(ctx);
                ::core::result::Result::Ok(())
            }

            fn on_reset(
                &mut self,
                ctx: &mut ::veryl_component::SimCtx,
            ) -> ::veryl_component::Result<()> {
                self.host.reset_edge(ctx);
                ::core::result::Result::Ok(())
            }

            fn on_finish(
                &mut self,
                ctx: &mut ::veryl_component::SimCtx,
            ) -> ::veryl_component::Result<()> {
                self.host.finish(ctx);
                ::core::result::Result::Ok(())
            }

            /// Registers a clock as `dut.<name>`; rustdv runs on its edges.
            fn set_clock(
                &mut self,
                _ctx: &mut ::veryl_component::SimCtx,
                name: ::std::string::String,
                clk: ::veryl_component::ClockPort,
            ) -> ::veryl_component::Result<()> {
                self.host.add_clock(&name, clk)
            }

            /// Registers a reset as `dut.<name>`.
            fn set_reset(
                &mut self,
                _ctx: &mut ::veryl_component::SimCtx,
                name: ::std::string::String,
                rst: ::veryl_component::ResetPort,
            ) -> ::veryl_component::Result<()> {
                self.host.add_reset(&name, rst)
            }

            /// Registers a signal rustdv reads as `dut.<name>`.
            fn set_input(
                &mut self,
                _ctx: &mut ::veryl_component::SimCtx,
                name: ::std::string::String,
                sig: ::veryl_component::InputPort,
            ) -> ::veryl_component::Result<()> {
                self.host.add_input(&name, sig)
            }

            /// Registers a signal rustdv drives as `dut.<name>`.
            fn set_output(
                &mut self,
                _ctx: &mut ::veryl_component::SimCtx,
                name: ::std::string::String,
                sig: ::veryl_component::OutputPort,
            ) -> ::veryl_component::Result<()> {
                self.host.add_output(&name, sig)
            }

            /// Registers the members of a modport under the scope
            /// `dut.<name>`.
            fn set_interface(
                &mut self,
                _ctx: &mut ::veryl_component::SimCtx,
                name: ::std::string::String,
                bus: ::veryl_component::ModportRef,
            ) -> ::veryl_component::Result<()> {
                self.host.add_interface(&name, &bus)
            }

            /// Hands control to rustdv: runs the crate's `#[rustdv::test]`s.
            fn start(
                &mut self,
                ctx: &mut ::veryl_component::SimCtx,
            ) -> ::veryl_component::Result<()> {
                self.host.start(ctx)
            }
        }

        ::veryl_component::veryl_component_export!($name => Rustdv);
    };
}

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
    Signal {
        name: String,
        port: Port,
        width: u32,
    },
    Scope(Scope),
}

/// A scope of the design as rustdv sees it: named signals backed by the
/// component's ports, and nested scopes.
struct Scope {
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

    fn has(&self, name: &str) -> bool {
        self.items.iter().any(|item| match item {
            Item::Signal { name: n, .. } => n == name,
            Item::Scope(scope) => scope.name == name,
        })
    }

    /// The nested scope `name`, created if missing; `None` if a signal
    /// already has the name.
    fn scope_mut(&mut self, name: &str) -> Option<&mut Scope> {
        let pos = self.items.iter().position(|item| match item {
            Item::Signal { name: n, .. } => n == name,
            Item::Scope(scope) => scope.name == name,
        });
        let pos = match pos {
            Some(pos) => pos,
            None => {
                self.items.push(Item::Scope(Scope::new(name)));
                self.items.len() - 1
            }
        };
        match &mut self.items[pos] {
            Item::Scope(scope) => Some(scope),
            Item::Signal { .. } => None,
        }
    }
}

/// Hosts the rustdv regression inside a Veryl component. The component
/// [`export!`] generates owns one: the test module registers clocks, resets,
/// signals and interfaces through its methods, then calls `start`.
pub struct Host {
    top: Scope,
}

impl Default for Host {
    fn default() -> Self {
        Host {
            top: Scope::new("dut"),
        }
    }
}

impl Host {
    /// Registers a clock as the signal `name` under the `dut` scope; a dotted
    /// name (`"core.clk"`) nests it in scopes. Only before [`Host::start`].
    pub fn add_clock(&mut self, name: &str, port: ClockPort) -> veryl_component::Result<()> {
        self.add(name, Port::Clock(port), 1)
    }

    /// Registers a reset as the signal `name`; see [`Host::add_clock`].
    pub fn add_reset(&mut self, name: &str, port: ResetPort) -> veryl_component::Result<()> {
        self.add(name, Port::Reset(port), 1)
    }

    /// Registers a signal rustdv reads as `name`; see [`Host::add_clock`].
    pub fn add_input(&mut self, name: &str, port: InputPort) -> veryl_component::Result<()> {
        self.add(name, Port::Input(port), port.width())
    }

    /// Registers a signal rustdv drives as `name`; see [`Host::add_clock`].
    pub fn add_output(&mut self, name: &str, port: OutputPort) -> veryl_component::Result<()> {
        self.add(name, Port::Output(port), port.width())
    }

    /// Registers the members of a modport as signals of the scope `name`,
    /// each under its member name; see [`Host::add_clock`].
    pub fn add_interface(&mut self, name: &str, bus: &ModportRef) -> veryl_component::Result<()> {
        for member in bus.members() {
            let path = format!("{name}.{}", member.name);
            match member.port {
                MemberPort::Input(port) => self.add_input(&path, port)?,
                MemberPort::Output(port) => self.add_output(&path, port)?,
            }
        }
        Ok(())
    }

    fn add(&mut self, name: &str, port: Port, width: u32) -> veryl_component::Result<()> {
        if with_state(|s| s.started).unwrap_or(false) {
            veryl_component::bail!("cannot register `{name}` after the rustdv regression started");
        }
        let mut parts: Vec<&str> = name.split('.').collect();
        let Some(leaf) = parts.pop().filter(|leaf| !leaf.is_empty()) else {
            veryl_component::bail!("invalid signal name `{name}`");
        };
        let mut scope = &mut self.top;
        for part in parts {
            if part.is_empty() {
                veryl_component::bail!("invalid signal name `{name}`");
            }
            scope = match scope.scope_mut(part) {
                Some(scope) => scope,
                None => veryl_component::bail!("`{part}` of `{name}` is already a signal"),
            };
        }
        if scope.has(leaf) {
            veryl_component::bail!("`{name}` is already registered");
        }
        scope.items.push(Item::Signal {
            name: leaf.to_string(),
            port,
            width,
        });
        Ok(())
    }

    /// Forward `on_init`. Nothing runs before [`Host::start`].
    pub fn init(&self, _ctx: &mut SimCtx) {}

    /// Hand control to rustdv, like UVM's `run_test`: binds the registered
    /// signals and starts the regression of every `#[rustdv::test]` linked
    /// into the crate. Fails if the regression already started.
    pub fn start(&self, ctx: &mut SimCtx) -> veryl_component::Result<()> {
        if with_state(|s| s.started).unwrap_or(false) {
            veryl_component::bail!("the rustdv regression has already started");
        }
        self.setup();
        with_state(|s| s.started = true);
        let now = ctx.time();
        let _hook = HookGuard::enter(ctx);
        advance(now);
        rustdv::runner::vpi_startup();
        fire(|w| matches!(w, Wait::StartOfSimulation));
        settle();
        Ok(())
    }

    /// Create the backend state and install the backend.
    fn setup(&self) {
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

    /// Forward `on_clock`. Ignored before [`Host::start`].
    pub fn clock_edge(&self, ctx: &mut SimCtx) {
        if with_state(|_| ()).is_none() {
            return;
        }
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

    /// Forward `on_reset`. Ignored before [`Host::start`].
    pub fn reset_edge(&self, ctx: &mut SimCtx) {
        if with_state(|_| ()).is_none() {
            return;
        }
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
            ctx.fail("the rustdv regression was not started: call `start`");
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
    // SAFETY: node IDs are this backend's own; it resolves them by index and
    // checks them against the current design (`valid_node`).
    unsafe { ObjHandle::from_raw(index + 1) }
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
