//! rustdv tests of the adder, run by Veryl's built-in simulator.
//!
//! The `rustdv_tb` component connects to the `tb` modport of the adder's
//! bus and hosts every `#[rustdv::test]` of this crate; the Veryl test
//! `adder_test` instantiates it next to the adder.

use rustdv::prelude::*;
use rustdv_veryl::{Host, posedge_within, tick};
use veryl_component::{
    BuildCtx, ClockPort, Component, InputPort, OutputPort, Result, SimCtx, VerylInterface,
    component_impl, veryl_component_export,
};

/// The `tb` modport of `adder_if`.
#[derive(VerylInterface)]
#[interface(path = "adder_if", modport = "tb")]
struct AdderTb {
    start: OutputPort,
    a: OutputPort,
    b: OutputPort,
    done: InputPort,
    result: InputPort,
}

/// Hosts the rustdv tests: the DUT scope `dut` holds `clk` and the bus
/// members under `bus`.
#[derive(Component)]
#[component(kind = clocked)]
pub struct RustdvTb {
    clk: ClockPort,
    #[interface]
    bus: AdderTb,
    host: Host,
}

#[component_impl]
impl RustdvTb {
    fn on_build(&mut self, _ctx: &mut BuildCtx) -> Result<()> {
        self.host.clock("clk", self.clk);
        self.host
            .scope("bus")
            .output("start", self.bus.start)
            .output("a", self.bus.a)
            .output("b", self.bus.b)
            .input("done", self.bus.done)
            .input("result", self.bus.result);
        Ok(())
    }

    fn on_init(&mut self, ctx: &mut SimCtx) -> Result<()> {
        self.host.init(ctx);
        Ok(())
    }

    fn on_clock(&mut self, ctx: &mut SimCtx) -> Result<()> {
        self.host.clock_edge(ctx);
        Ok(())
    }

    fn on_finish(&mut self, ctx: &mut SimCtx) -> Result<()> {
        self.host.finish(ctx);
        Ok(())
    }

    /// Hands control to rustdv: runs the rustdv tests of this crate.
    fn start(&mut self, ctx: &mut SimCtx) -> Result<()> {
        self.host.start(ctx)
    }
}

veryl_component_export!("rustdv_tb" => RustdvTb);

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
