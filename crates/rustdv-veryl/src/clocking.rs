//! Clocking blocks: read and write signals as plain struct fields.
//!
//! A `#[clocking]` struct is a clocking block, like SystemVerilog's
//! `clocking cb @(posedge clk)`. Its fields are values: input fields hold
//! the value sampled at the last clock edge, and output fields are driven
//! with the value last assigned. `tick!(cb)` synchronizes them: it drives
//! the outputs changed since the last tick, waits for the rising edge of the
//! clock, and samples every input.
//!
//! ```ignore
//! use rustdv_veryl::{clocking, modport, tick};
//!
//! /// The `tb` modport of `adder_if`.
//! #[modport]
//! struct AdderTb {
//!     #[output] start: bool,
//!     #[output] a: u8,
//!     #[output] b: u8,
//!     #[input]  done: bool,
//!     #[input]  result: u16,
//! }
//!
//! #[clocking]
//! struct Tb {
//!     #[modport] bus: AdderTb,
//!     #[input]   state: u8,
//! }
//!
//! let mut cb = Tb::new(&dut, dut.signal("clk")?, TbPaths::default())?;
//! cb.bus.start = true;
//! tick!(cb);
//! cb.posedge(|c| c.bus.done).within(10).await?;
//! ```
//!
//! Fields are bound by name under a scope given to `new`; the generated
//! `<Name>Paths` struct overrides the path of any field. A `#[modport]`
//! field binds its members under the scope it names.
//!
//! The clocking blocks only use rustdv's public API, so they work on any
//! rustdv backend.

use std::fmt;
use std::future::{Future, IntoFuture};
use std::pin::Pin;

use rustdv::prelude::*;
use rustdv::sim::handle::HandleBase;

/// A Rust type a signal can be read into and written from.
///
/// Implemented for `bool`, the unsigned integers up to `u128`, `BigUint` and
/// `LogicArray`. Following SystemVerilog's conversion to two-state types,
/// every type but `LogicArray` reads X and Z bits as 0.
pub trait SignalValue: Clone + PartialEq {
    /// Whether a signal of `width` bits fits the type.
    fn fits(width: u32) -> bool;
    /// The value of `signal`.
    fn load(signal: &LogicHandle) -> Self;
    /// Schedules a write of `self` to `signal`, committed like any rustdv
    /// write.
    fn store(&self, signal: &LogicHandle);
}

/// The two-state value of `signal` as 32-bit words, least significant
/// first: X and Z bits read as 0.
fn two_state_words(signal: &LogicHandle) -> Vec<u32> {
    match signal.get_logic() {
        Ok(value) => value
            .to_vpi_words()
            .into_iter()
            .map(|(aval, bval)| aval & !bval)
            .collect(),
        Err(_) => vec![],
    }
}

fn words_to_u128(words: &[u32]) -> u128 {
    words
        .iter()
        .take(4)
        .enumerate()
        .fold(0, |acc, (i, w)| acc | (u128::from(*w) << (32 * i)))
}

impl SignalValue for bool {
    fn fits(width: u32) -> bool {
        width == 1
    }
    fn load(signal: &LogicHandle) -> Self {
        words_to_u128(&two_state_words(signal)) & 1 != 0
    }
    fn store(&self, signal: &LogicHandle) {
        signal.set_u64(u64::from(*self));
    }
}

macro_rules! unsigned_signal_value {
    ($($ty:ty),*) => {$(
        impl SignalValue for $ty {
            fn fits(width: u32) -> bool {
                width <= <$ty>::BITS
            }
            fn load(signal: &LogicHandle) -> Self {
                words_to_u128(&two_state_words(signal)) as $ty
            }
            fn store(&self, signal: &LogicHandle) {
                signal.set_u128(u128::from(*self));
            }
        }
    )*};
}

unsigned_signal_value!(u8, u16, u32, u64, u128);

impl SignalValue for BigUint {
    fn fits(_width: u32) -> bool {
        true
    }
    fn load(signal: &LogicHandle) -> Self {
        BigUint::from_slice(&two_state_words(signal))
    }
    fn store(&self, signal: &LogicHandle) {
        signal.set_bigint(self);
    }
}

impl SignalValue for LogicArray {
    fn fits(_width: u32) -> bool {
        true
    }
    fn load(signal: &LogicHandle) -> Self {
        signal
            .get_logic()
            .unwrap_or_else(|_| LogicArray::from_u64(0, signal.size() as usize))
    }
    fn store(&self, signal: &LogicHandle) {
        signal.set_logic(self);
    }
}

/// A field of a clocking block or a modport, bound to a signal. Generated
/// code holds one per field.
#[doc(hidden)]
pub struct Member<T> {
    signal: LogicHandle,
    /// The value last driven, for an output: unchanged outputs are not
    /// written again.
    last: T,
}

impl<T> fmt::Debug for Member<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Member")
    }
}

impl<T: SignalValue> Member<T> {
    /// Binds the signal at `path` under `scope`, checking that its width
    /// fits `T`, and returns the member and the signal's current value.
    pub fn bind(scope: &HierarchyHandle, path: &str) -> Result<(Self, T), TestError> {
        let signal = scope.signal(path)?;
        let width = signal.size();
        if !T::fits(width) {
            return Err(format!(
                "`{}` is {width} bits wide, which does not fit `{}`",
                signal.full_name(),
                std::any::type_name::<T>()
            )
            .into());
        }
        let value = T::load(&signal);
        Ok((
            Self {
                signal,
                last: value.clone(),
            },
            value,
        ))
    }

    /// The current value of the signal.
    pub fn load(&self) -> T {
        T::load(&self.signal)
    }

    /// Drives `value` if it differs from the value last driven.
    pub fn store(&mut self, value: &T) {
        if *value != self.last {
            value.store(&self.signal);
            self.last = value.clone();
        }
    }
}

/// Items the generated code names, independent of the user's imports.
#[doc(hidden)]
pub mod __private {
    pub use rustdv::prelude::{HierarchyHandle, LogicHandle, TestError};
}

/// The scope a `#[modport]` field binds its members under.
#[doc(hidden)]
pub fn modport_scope(scope: &HierarchyHandle, path: &str) -> Result<HierarchyHandle, TestError> {
    Ok(scope.hierarchy(path)?)
}

/// Implemented by `#[modport]` for its struct.
#[doc(hidden)]
pub trait Modport: Sized {
    fn __bind(scope: &HierarchyHandle) -> Result<Self, TestError>;
    fn __load(&mut self);
    fn __store(&mut self);
}

/// Implemented by `#[clocking]` for its struct.
#[doc(hidden)]
pub trait ClockingBlock {
    fn __clock(&self) -> LogicHandle;
    fn __load(&mut self);
    fn __store(&mut self);
}

/// Waits for the next clock edge of a clocking block, like SystemVerilog's
/// `@(cb)`: drives the outputs assigned since the last tick, waits for the
/// rising edge of the clock, and samples every input. Use it in an `async`
/// block or function; it stands for `cb.tick().await`.
///
/// ```ignore
/// rustdv_veryl::tick!(cb);
/// ```
#[macro_export]
macro_rules! tick {
    ($cb:expr) => {
        ($cb).tick().await
    };
}

/// Drives the changed outputs, waits for the rising clock edge and samples
/// every input.
#[doc(hidden)]
pub async fn tick<C: ClockingBlock>(c: &mut C) {
    c.__store();
    c.__clock().rising_edge().await;
    c.__load();
}

/// A wait on a clocking block (`posedge`, `negedge`, `edge`, `changed`,
/// `wait`). Await it, or give it an edge limit with [`Wait::within`].
#[must_use = "a wait does nothing unless awaited"]
pub struct Wait<'a, C> {
    c: &'a mut C,
    /// Whether a condition already met ends the wait without a tick.
    immediate: bool,
    /// Called on the samples of each tick (and before the first one when
    /// `immediate`); true ends the wait.
    hit: Box<dyn FnMut(&C) -> bool + 'a>,
    label: Option<String>,
}

impl<'a, C: ClockingBlock + 'a> Wait<'a, C> {
    #[doc(hidden)]
    pub fn new(c: &'a mut C, immediate: bool, hit: Box<dyn FnMut(&C) -> bool + 'a>) -> Self {
        Self {
            c,
            immediate,
            hit,
            label: None,
        }
    }

    /// Names the wait in the timeout error of [`Wait::within`].
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Gives up after `edges` clock edges, returning [`ClockingTimeout`].
    pub fn within(self, edges: u64) -> WaitWithin<'a, C> {
        WaitWithin { wait: self, edges }
    }

    async fn run(mut self, limit: Option<u64>) -> Result<(), ClockingTimeout> {
        if self.immediate && (self.hit)(self.c) {
            return Ok(());
        }
        let mut edges = 0;
        loop {
            if limit.is_some_and(|limit| edges >= limit) {
                return Err(ClockingTimeout {
                    label: self.label,
                    edges,
                });
            }
            tick(self.c).await;
            edges += 1;
            if (self.hit)(self.c) {
                return Ok(());
            }
        }
    }
}

impl<'a, C: ClockingBlock + 'a> IntoFuture for Wait<'a, C> {
    type Output = ();
    type IntoFuture = Pin<Box<dyn Future<Output = ()> + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            // Without a limit the wait cannot time out.
            let _ = self.run(None).await;
        })
    }
}

/// A [`Wait`] limited to a number of clock edges.
#[must_use = "a wait does nothing unless awaited"]
pub struct WaitWithin<'a, C> {
    wait: Wait<'a, C>,
    edges: u64,
}

impl<'a, C: ClockingBlock + 'a> WaitWithin<'a, C> {
    /// Names the wait in its timeout error.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.wait.label = Some(label.into());
        self
    }
}

impl<'a, C: ClockingBlock + 'a> IntoFuture for WaitWithin<'a, C> {
    type Output = Result<(), ClockingTimeout>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.wait.run(Some(self.edges)))
    }
}

/// A wait limited by [`Wait::within`] saw no match within its edges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockingTimeout {
    pub label: Option<String>,
    pub edges: u64,
}

impl fmt::Display for ClockingTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.label {
            Some(label) => write!(f, "`{label}` not seen within {} cycles", self.edges),
            None => write!(f, "condition not seen within {} cycles", self.edges),
        }
    }
}

impl std::error::Error for ClockingTimeout {}

impl From<ClockingTimeout> for TestError {
    fn from(e: ClockingTimeout) -> Self {
        TestError::new(e.to_string())
    }
}

/// The wait constructors the `#[clocking]` methods call.
#[doc(hidden)]
pub mod waits {
    use super::{ClockingBlock, Wait};

    pub fn posedge<'a, C: ClockingBlock + 'a>(
        c: &'a mut C,
        mut f: impl FnMut(&C) -> bool + 'a,
    ) -> Wait<'a, C> {
        let mut prev = f(c);
        Wait::new(
            c,
            false,
            Box::new(move |c| {
                let now = f(c);
                let hit = !prev && now;
                prev = now;
                hit
            }),
        )
    }

    pub fn negedge<'a, C: ClockingBlock + 'a>(
        c: &'a mut C,
        mut f: impl FnMut(&C) -> bool + 'a,
    ) -> Wait<'a, C> {
        let mut prev = f(c);
        Wait::new(
            c,
            false,
            Box::new(move |c| {
                let now = f(c);
                let hit = prev && !now;
                prev = now;
                hit
            }),
        )
    }

    pub fn edge<'a, C: ClockingBlock + 'a>(
        c: &'a mut C,
        f: impl FnMut(&C) -> bool + 'a,
    ) -> Wait<'a, C> {
        changed(c, f)
    }

    pub fn changed<'a, C: ClockingBlock + 'a, T: PartialEq + 'a>(
        c: &'a mut C,
        mut f: impl FnMut(&C) -> T + 'a,
    ) -> Wait<'a, C> {
        let mut prev = f(c);
        Wait::new(
            c,
            false,
            Box::new(move |c| {
                let now = f(c);
                let hit = now != prev;
                prev = now;
                hit
            }),
        )
    }

    pub fn wait<'a, C: ClockingBlock + 'a>(
        c: &'a mut C,
        f: impl FnMut(&C) -> bool + 'a,
    ) -> Wait<'a, C> {
        Wait::new(c, true, Box::new(f))
    }
}
