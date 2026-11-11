# rustdv-veryl guide

This guide walks through [`examples/adder`](../examples/adder), a small
adder verified by a rustdv testbench on Veryl's built-in simulator. Each
step shows what the example does and what to write in your own project.

## The example at a glance

```text
examples/adder/
|-- Veryl.toml          the Veryl project; registers the component
|-- src/adder.veryl     the interface, the design and the #[test] module
`-- component/          the component crate: wiring + rustdv testbench
    |-- Cargo.toml
    `-- src/lib.rs
```

The `#[test]` module instantiates the design and a component side by side,
both connected to one interface instance. The component hosts the rustdv
testbench:

```text
#[test] module adder_test
 |-- inst bus: adder_if                             interface instance
 |-- inst dut: adder (clk, bus)                     design, on modport dut
 `-- inst tb : $comp::rustdv_tb (clk, bus: bus.tb)  component, on modport tb
                 `-- component crate (cdylib)
                      |-- rustdv_veryl::Host        ports -> rustdv signals
                      `-- #[rustdv::test] add       the testbench
```

Run it with:

```sh
cd examples/adder
veryl test
```

## Step 1: the interface and the design

The testbench reaches the design through an interface, like a UVM virtual
interface. `adder_if` has one modport for the design and one for the
testbench:

```veryl
interface adder_if {
    var start : logic   ;
    var a     : logic<8>;
    var b     : logic<8>;
    var done  : logic   ;
    var result: logic<9>;

    modport dut {
        start : input ,
        a     : input ,
        b     : input ,
        done  : output,
        result: output,
    }

    modport tb {
        start : output,
        a     : output,
        b     : output,
        done  : input ,
        result: input ,
    }
}
```

The design answers each `start` with the sum on the next cycle:

```veryl
module adder (
    i_clk: input   clock        ,
    bus  : modport adder_if::dut,
) {
    always_ff {
        bus.done = bus.start;
        if bus.start {
            bus.result = bus.a + bus.b;
        }
    }
}
```

## Step 2: the test module

The `#[test]` module connects the design and the component to the same
interface instance, the component on the `tb` modport:

```veryl
#[test(adder_test)]
module adder_test {
    inst clk: $tb::clock_gen;
    inst bus: adder_if;
    inst dut: adder (
        i_clk: clk,
        bus       ,
    );
    inst tb: $comp::rustdv_tb (
        clk        ,
        bus: bus.tb,
    );

    initial {
        tb.start();
        clk.next(1000);
        $finish();
    }
}
```

`tb.start()` hands control to rustdv, like UVM's `run_test`: the rustdv
tests start running from here. The test module decides when, e.g. after a
reset sequence. The rustdv regression finishes the test when it is done;
`clk.next(1000)` keeps the clock running and bounds the run if it never
does.

## Step 3: the component crate

`$comp::rustdv_tb` comes from the component crate, registered in
`Veryl.toml`:

```toml
[[components]]
path = "component"
```

In your own project, `veryl new --component rustdv_tb` creates the crate and
registers it. Then add the dependencies to its `Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
veryl-component = "0.1.2"
rustdv = { git = "https://github.com/taichi-ishitani/rustdv", branch = "gpi-backend-trait", default-features = false }
rustdv-veryl = { git = "https://github.com/taichi-ishitani/rustdv-veryl" }
```

rustdv-veryl is not on crates.io yet. It needs the rustdv backend interface,
proposed in [rustdv/rustdv#25](https://github.com/rustdv/rustdv/pull/25) and
not in any released rustdv; until it is, take rustdv from that branch. Use
the same rustdv source as rustdv-veryl does, or the two see different rustdv
crates. Take rustdv-veryl from its repository. The example takes
`rustdv-veryl` by path, to build against the local sources.

If cargo fails to fetch the git repository, for example because git rewrites
GitHub URLs to SSH, let it use the git command line in
`~/.cargo/config.toml`:

```toml
[net]
git-fetch-with-cli = true
```

- `rustdv` is a direct dependency because the testbench uses it, and its
  macros refer to the `rustdv` crate by name. Disable its default
  features, which are not used with Veryl.
- `veryl-component` is direct for the same reason: its derives refer to the
  `veryl_component` crate by name.

## Step 4: wiring the component

`component/src/lib.rs` first declares the `tb` modport as Rust ports:

```rust
#[derive(VerylInterface)]
#[interface(path = "adder_if", modport = "tb")]
struct AdderTb {
    start: OutputPort,
    a: OutputPort,
    b: OutputPort,
    done: InputPort,
    result: InputPort,
}
```

The component embeds it as the interface port `bus` — the name the test
module connects (`bus: bus.tb`) — next to the clock and a `Host`:

```rust
#[derive(Component)]
#[component(kind = clocked)]
pub struct RustdvTb {
    clk: ClockPort,
    #[interface]
    bus: AdderTb,
    host: Host,
}
```

In `on_build` it registers its ports with the host. The host exposes them to
rustdv as one DUT scope, `dut`, with a nested scope per `scope(...)`:
`dut.clk`, `dut.bus.start`, `dut.bus.done`, ... The other hooks are
forwarded to the host as they are, and the `start` method called by the test
module starts rustdv:

```rust
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

    fn start(&mut self, ctx: &mut SimCtx) -> Result<()> {
        self.host.start(ctx)
    }
}

veryl_component_export!("rustdv_tb" => RustdvTb);
```

From `start` the host runs every `#[rustdv::test]` linked into the crate.
When the regression ends the Veryl test finishes, and a failing regression
fails it. A test that never calls `start` fails when it ends.

Only registered signals are visible to rustdv: an input port can be read,
an output port can be driven.

## Step 5: the rustdv testbench

The test itself is plain rustdv. It looks the signals up under the `dut`
scope, drives a request on a rising edge and samples the response on later
rising edges:

```rust
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
```

`veryl test` shows the rustdv log as the output of the component:

```text
[tb] cycle 3:    4000.00ns INFO     [add]: 1 + 2 = 3
[tb] cycle 6:   10000.00ns INFO     [add]: 100 + 27 = 127
[tb] cycle 9:   16000.00ns INFO     [add]: 200 + 100 = 300
[tb] cycle 12:   22000.00ns INFO     [add]: 255 + 255 = 510
[tb] cycle 12:   22000.00ns INFO     add PASSED
...
[tb] cycle 12: REGRESSION: PASS
[INFO ]    Succeeded test (adder_test)
```

Each request takes three cycles: the request driven at one edge is sampled
by the adder at the next, and its `done` is seen by the testbench at the
edge after. The [timing](#timing) section explains why.

## Timing

Veryl's simulator is cycle based, and a component runs only at its hooks on
clock edges. The rustdv simulator callbacks are emulated from them:

- reads return the value just before the edge
- writes commit together with the design's flip-flops at the current edge
  (NBA semantics), so the design samples them at the next edge
- value changes of signals other than clocks are seen once per edge
- timers fire at the first edge at or after their deadline

Write testbenches that sample and drive on rising edges, as `add` does; they
then see the same cycles as on an event-driven simulator. One time step is
1 us.

## Limitations

- No hierarchical access into the design beyond the registered ports.
- Falling edges are emulated, not real: driving on them is not supported.
- One host per Veryl test.
