//! rustdv tests of the adder, run by Veryl's built-in simulator.
//!
//! `export!` makes this crate the `$comp::rustdv_tb` component, which runs
//! every `#[rustdv::test]` here; the Veryl test `adder_test` passes it the
//! clock and the `tb` modport of the adder's bus.

use rustdv::prelude::*;
use rustdv_veryl::{posedge_within, tick};

rustdv_veryl::export!("rustdv_tb");

/// The `tb` modport of `adder_if`, as values.
#[rustdv_veryl::modport]
struct AdderBus {
    #[output]
    start: bool,
    #[output]
    a: u8,
    #[output]
    b: u8,
    #[input]
    done: bool,
    #[input]
    result: u16,
}

impl AdderBus {
    fn request(&mut self, x: u8, y: u8) {
        self.start = true;
        self.a = x;
        self.b = y;
    }
}

/// The testbench's view of the design, synchronized with `clk`.
#[rustdv_veryl::clocking]
struct AdderCb {
    #[modport]
    bus: AdderBus,
}

/// Sends each pair through the bus on rising edges and checks the sums.
#[rustdv::test]
async fn add(ctx: RustdvCtx) -> std::result::Result<(), TestError> {
    let dut = ctx.dut();
    let mut cb = AdderCb::new(&dut, dut.signal("clk")?, AdderCbPaths::default())?;

    for (x, y) in [(1u8, 2u8), (100, 27), (200, 100), (255, 255)] {
        tick!(cb);
        cb.bus.request(x, y);
        tick!(cb);
        cb.bus.start = false;
        posedge_within!(cb.bus.done, 8)?;
        let sum = u64::from(cb.bus.result);
        let expected = u64::from(x) + u64::from(y);
        ctx.info(&format!("{x} + {y} = {sum}"));
        if sum != expected {
            return Err(format!("{x} + {y}: expected {expected}, got {sum}").into());
        }
    }
    Ok(())
}
