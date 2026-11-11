//! rustdv tests of the adder, run by Veryl's built-in simulator.
//!
//! The `rustdv_tb` component connects to the `tb` modport of the adder's
//! bus and hosts every `#[rustdv::test]` of this crate; the Veryl test
//! `adder_test` instantiates it next to the adder.

use rustdv::prelude::*;
use rustdv_veryl::Host;
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

/// Sends each pair through the bus on rising edges and checks the sums.
#[rustdv::test]
async fn add(ctx: RustdvCtx) -> std::result::Result<(), TestError> {
    let dut = ctx.dut();
    let clk = dut.signal("clk")?;
    let bus = dut.hierarchy("bus")?;
    let (start, a, b) = (bus.signal("start")?, bus.signal("a")?, bus.signal("b")?);
    let (done, result) = (bus.signal("done")?, bus.signal("result")?);

    for (x, y) in [(1u64, 2u64), (100, 27), (200, 100), (255, 255)] {
        clk.rising_edge().await;
        a.set_u64(x);
        b.set_u64(y);
        start.set_u64(1);
        clk.rising_edge().await;
        start.set_u64(0);
        loop {
            clk.rising_edge().await;
            if done.is_high() {
                break;
            }
        }
        let sum = result.get_u64()?;
        ctx.info(&format!("{x} + {y} = {sum}"));
        if sum != x + y {
            return Err(format!("{x} + {y}: expected {}, got {sum}", x + y).into());
        }
    }
    Ok(())
}
