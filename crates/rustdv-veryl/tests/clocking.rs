//! Clocking blocks on a component driven by `MockSim`, with the design
//! modeled here: `q` and `bus.ack` return `d` and `bus.req` one clock later,
//! and `cnt` counts the clocks.
//!
//! The `#[rustdv::test]`s of this binary form one rustdv regression; each
//! checks one behavior and fails with what it saw.

use rustdv::prelude::*;
use rustdv_veryl::{
    ClockingTimeout, Host, changed, changed_within, clocking, edge, modport, negedge, posedge,
    posedge_within, tick,
};
use smallvec::smallvec;
use veryl_component::testing::MockSim;
use veryl_component::{
    BuildCtx, ClockPort, Component, InputPort, OutputPort, Result, SimCtx, Value, component_impl,
};

#[derive(Component)]
#[component(kind = clocked)]
struct Tb {
    clk: ClockPort,
    d: OutputPort,
    q: InputPort,
    cnt: InputPort,
    flag: InputPort,
    wide: InputPort,
    req: OutputPort,
    ack: InputPort,
    host: Host,
}

#[component_impl]
impl Tb {
    fn on_build(&mut self, _ctx: &mut BuildCtx) -> Result<()> {
        self.host
            .clock("clk", self.clk)
            .output("d", self.d)
            .input("q", self.q)
            .input("cnt", self.cnt)
            .input("flag", self.flag)
            .input("wide", self.wide);
        self.host
            .scope("bus")
            .output("req", self.req)
            .input("ack", self.ack);
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

    fn start(&mut self, ctx: &mut SimCtx) -> Result<()> {
        self.host.start(ctx)
    }
}

#[modport]
struct Bus {
    #[output]
    req: u8,
    #[input]
    ack: u8,
}

#[clocking]
struct Cb {
    #[output]
    d: u8,
    #[input]
    q: u8,
    #[input]
    cnt: u8,
    #[modport]
    bus: Bus,
}

/// Binds the clocking block under the DUT scope.
fn cb(ctx: &RustdvCtx) -> std::result::Result<Cb, TestError> {
    let dut = ctx.dut();
    Cb::new(&dut, dut.signal("clk")?, CbPaths::default())
}

fn check(ok: bool, what: impl FnOnce() -> String) -> std::result::Result<(), TestError> {
    if ok { Ok(()) } else { Err(what().into()) }
}

#[rustdv::test]
async fn tick_drives_and_samples(ctx: RustdvCtx) -> std::result::Result<(), TestError> {
    let mut cb = cb(&ctx)?;
    cb.d = 5;
    cb.bus.req = 7;
    // Driven at this edge, returned by the design at the next one.
    tick!(cb);
    tick!(cb);
    check(cb.q == 5, || format!("q = {}, expected 5", cb.q))?;
    check(cb.bus.ack == 7, || {
        format!("bus.ack = {}, expected 7", cb.bus.ack)
    })?;
    // An output keeps the value last assigned.
    check(cb.d == 5, || format!("d = {}, expected 5", cb.d))
}

#[rustdv::test]
async fn waits_return_at_the_matching_edge(ctx: RustdvCtx) -> std::result::Result<(), TestError> {
    let mut cb = cb(&ctx)?;
    tick!(cb);

    posedge!(cb.cnt % 4 == 0);
    check(cb.cnt % 4 == 0, || format!("posedge: cnt = {}", cb.cnt))?;
    negedge!(cb.cnt % 4 == 0);
    check(cb.cnt % 4 == 1, || format!("negedge: cnt = {}", cb.cnt))?;
    let before = cb.cnt;
    edge!(cb.cnt % 2 == 0);
    check(cb.cnt == before.wrapping_add(1), || {
        format!("edge: cnt = {}", cb.cnt)
    })?;
    let before = cb.cnt;
    changed!(cb.cnt / 2);
    check(cb.cnt / 2 != before / 2, || {
        format!("changed: cnt = {}", cb.cnt)
    })?;

    // A condition already met ends `wait` without a tick.
    let now = cb.cnt;
    cb.wait(|c| c.cnt == now).await;
    check(cb.cnt == now, || format!("wait: cnt moved to {}", cb.cnt))?;
    let target = now.wrapping_add(3);
    cb.wait(|c| c.cnt == target).await;
    check(cb.cnt == target, || format!("wait: cnt = {}", cb.cnt))
}

#[rustdv::test]
async fn within_times_out(ctx: RustdvCtx) -> std::result::Result<(), TestError> {
    let mut cb = cb(&ctx)?;
    let start = cb.cnt;
    let err = cb
        .wait(|c| c.cnt == start.wrapping_sub(1))
        .within(3)
        .label("never")
        .await;
    check(
        err == Err(ClockingTimeout {
            label: Some("never".into()),
            edges: 3,
        }),
        || format!("got {err:?}"),
    )?;
    check(cb.cnt == start.wrapping_add(3), || {
        format!(
            "timed out at cnt = {}, expected 3 edges after {start}",
            cb.cnt
        )
    })?;
    let msg = err.unwrap_err().to_string();
    check(msg == "`never` not seen within 3 cycles", || msg.clone())?;
    // A match within the limit is Ok.
    cb.changed(|c| c.cnt).within(1).await?;

    // The `_within` macros name the wait by its expression.
    let never = cb.cnt.wrapping_sub(1);
    let err = posedge_within!(cb.cnt == never, 2);
    check(
        err == Err(ClockingTimeout {
            label: Some("cb.cnt == never".into()),
            edges: 2,
        }),
        || format!("got {err:?}"),
    )?;
    changed_within!(cb.cnt, 1)?;
    Ok(())
}

#[clocking]
struct Renamed {
    #[input]
    count: u8,
}

#[rustdv::test]
async fn paths_override_field_names(ctx: RustdvCtx) -> std::result::Result<(), TestError> {
    let dut = ctx.dut();
    let mut cb = Renamed::new(
        &dut,
        dut.signal("clk")?,
        RenamedPaths {
            count: Some("cnt"),
            ..Default::default()
        },
    )?;
    let before = cb.count;
    tick!(cb);
    check(cb.count == before.wrapping_add(1), || {
        format!("count = {}, expected {}", cb.count, before.wrapping_add(1))
    })
}

#[clocking]
struct TooNarrow {
    #[input]
    wide: u8,
}

#[rustdv::test]
async fn new_rejects_a_field_too_narrow(ctx: RustdvCtx) -> std::result::Result<(), TestError> {
    let dut = ctx.dut();
    match TooNarrow::new(&dut, dut.signal("clk")?, TooNarrowPaths::default()) {
        Ok(_) => Err("a 9-bit signal was bound to a u8 field".into()),
        Err(e) => check(
            e.to_string()
                .contains("is 9 bits wide, which does not fit `u8`"),
            || e.to_string(),
        ),
    }
}

#[clocking]
struct Unknown {
    #[input]
    flag: bool,
    #[input]
    wide: u16,
}

#[clocking]
struct Raw {
    #[input]
    flag: LogicArray,
}

#[rustdv::test]
async fn x_and_z_read_as_zero(ctx: RustdvCtx) -> std::result::Result<(), TestError> {
    let dut = ctx.dut();
    let mut cb = Unknown::new(&dut, dut.signal("clk")?, UnknownPaths::default())?;
    let mut raw = Raw::new(&dut, dut.signal("clk")?, RawPaths::default())?;
    tick!(cb);
    tick!(raw);
    // `flag` is X, and `wide` is 0x1ff with its bit 0 Z (see `clocking_blocks`).
    check(!cb.flag, || "X read as 1".into())?;
    check(cb.wide == 0x1fe, || {
        format!("wide = {:#x}, expected 0x1fe", cb.wide)
    })?;
    check(raw.flag.to_binstr() == "x", || {
        format!("LogicArray read {}", raw.flag.to_binstr())
    })
}

/// Runs the regression on `MockSim`, modeling the design each clock.
#[test]
fn clocking_blocks() {
    let mut sim = MockSim::new()
        .four_state(true)
        .clock_port("clk")
        .output("d", 8)
        .input("q", 8)
        .input("cnt", 8)
        .input("flag", 1)
        .input("wide", 9)
        .output("req", 8)
        .input("ack", 8);
    sim.set_fired_clock("clk");
    let mut tb = sim.build::<Tb>().unwrap();
    // X on `flag`; 0x1ff on `wide` with bit 0 Z.
    sim.set("flag", Value::from_bits(smallvec![0], smallvec![1], 1));
    sim.set("wide", Value::from_bits(smallvec![0x1ff], smallvec![1], 9));
    sim.init(&mut tb).unwrap();
    sim.call(&mut tb, "start", &[]).unwrap();

    let mut cnt: u64 = 0;
    for _ in 0..200 {
        if sim.finish_requested() || sim.failed() {
            break;
        }
        sim.time += 2;
        sim.clock(&mut tb).unwrap();
        // The design: registered loopback and a free-running counter.
        let d = sim.get("d").as_u64().unwrap();
        let req = sim.get("req").as_u64().unwrap();
        cnt = (cnt + 1) % 256;
        sim.set("q", d);
        sim.set("ack", req);
        sim.set("cnt", Value::from_u64(cnt, 8));
    }
    sim.finish(&mut tb).unwrap();

    let logs = sim.logs().join("\n");
    assert!(
        !sim.failed() && logs.contains("REGRESSION: PASS"),
        "failures: {:?}\n{logs}",
        sim.failures()
    );
}
