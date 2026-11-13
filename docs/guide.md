# rustdv-veryl guide

This guide walks through [`examples/adder`](../examples/adder), a small
adder verified by a rustdv testbench on Veryl's built-in simulator. Each
step shows what the example does and what to write in your own project.

## The example at a glance

```text
examples/adder/
|-- Veryl.toml          the Veryl project; registers the component
|-- src/adder.veryl     the interface, the design and the #[test] module
`-- component/          the component crate: the rustdv testbench
    |-- Cargo.toml
    `-- src/lib.rs
```

The `#[test]` module instantiates the design and the testbench component
side by side, and passes the component the clock and the `tb` modport of
the interface instance the design is connected to:

```text
#[test] module adder_test
 |-- inst bus: adder_if                      interface instance
 |-- inst dut: adder (clk, bus)              design, on modport dut
 `-- inst tb : $comp::rustdv_tb              component, without ports
       |-- tb.set_clock("clk", clk)          the clock
       |-- tb.set_interface("bus", bus.tb)   modport tb
       `-- component crate (cdylib)
            |-- rustdv_veryl::export!        the component
            `-- #[rustdv::test] add          the testbench
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

The `#[test]` module connects the design to the interface instance, and
hands the testbench component the clock and the `tb` modport through its
methods:

```veryl
#[test(adder_test)]
module adder_test {
    inst clk: $tb::clock_gen;
    inst bus: adder_if;
    inst dut: adder (
        i_clk: clk,
        bus       ,
    );
    inst tb : $comp::rustdv_tb;

    initial {
        tb.set_clock("clk", clk);
        tb.set_interface("bus", bus.tb);
        tb.start();
        clk.next(1000);
        $finish();
    }
}
```

The component has no ports; what it works on is what the test module passes
it, so each test module can pass something else. Each method takes the name
rustdv sees and a reference:

| Method | Passes | rustdv sees |
|---|---|---|
| `tb.set_clock(name, clk)` | a clock; rustdv runs on its edges | `dut.<name>` |
| `tb.set_reset(name, rst)` | a reset | `dut.<name>` |
| `tb.set_input(name, x)` | a signal rustdv reads | `dut.<name>` |
| `tb.set_output(name, y)` | a signal rustdv drives | `dut.<name>` |
| `tb.set_interface(name, bus.tb)` | a modport of any interface | `dut.<name>.<member>` |

A dotted name nests scopes: `tb.set_interface("core.bus", ...)` makes
`dut.core.bus.<member>`, so a test can use the same paths as on a simulator
where the design hierarchy is visible. A name is registered once, and all
of them before `start`.

`tb.start()` hands control to rustdv, like UVM's `run_test`: the rustdv
tests start running from here. The test module decides when, e.g. after a
reset sequence. The rustdv regression finishes the test when it is done;
`clk.next(1000)` keeps the clock running and bounds the run if it never
does. A test that never calls `start` fails when it ends.

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
veryl-component = { git = "https://github.com/taichi-ishitani/veryl", branch = "feat/component-method-ref-args" }
rustdv = { git = "https://github.com/taichi-ishitani/rustdv", branch = "gpi-backend-trait", default-features = false }
rustdv-veryl = { git = "https://github.com/taichi-ishitani/rustdv-veryl" }
```

None of them is released with what rustdv-veryl needs yet:

- passing references to component methods, proposed in
  [veryl-lang/veryl#3701](https://github.com/veryl-lang/veryl/pull/3701):
  take `veryl-component` from that branch, and run `veryl test` with a Veryl
  built from it
- the rustdv backend interface, proposed in
  [rustdv/rustdv#25](https://github.com/rustdv/rustdv/pull/25): take rustdv
  from that branch

Use the same `veryl-component` and rustdv sources as rustdv-veryl does, or
the crates see two different copies of them. The example takes
`rustdv-veryl` by path, to build against the local sources.

If cargo fails to fetch a git repository, for example because git rewrites
GitHub URLs to SSH, let it use the git command line in
`~/.cargo/config.toml`:

```toml
[net]
git-fetch-with-cli = true
```

- `rustdv` is a direct dependency because the testbench uses it, and its
  macros refer to the `rustdv` crate by name. Disable its default
  features, which are not used with Veryl.
- `veryl-component` is direct for the same reason: the component
  `rustdv_veryl::export!` generates refers to the `veryl_component` crate by
  name.

## Step 4: the testbench component

`component/src/lib.rs` makes the crate the component with one line:

```rust
rustdv_veryl::export!("rustdv_tb");
```

The name is the one the test module instantiates (`$comp::rustdv_tb`). The
generated component has the methods of [Step 2](#step-2-the-test-module),
and from `start` it runs every `#[rustdv::test]` linked into the crate.
When the regression ends the Veryl test finishes, and a failing regression
fails it.

## Step 5: the rustdv testbench

The testbench reads and writes the bus through a clocking block, like a
SystemVerilog `clocking cb @(posedge clk)`: signals are plain struct fields,
synchronized with the clock edges.

The test declares the `tb` modport as values for rustdv. `#[modport]` gives
each member its direction, and functions of the interface become methods:

```rust
#[rustdv_veryl::modport]
struct AdderBus {
    #[output] start: bool,
    #[output] a: u8,
    #[output] b: u8,
    #[input]  done: bool,
    #[input]  result: u16,
}

impl AdderBus {
    fn request(&mut self, x: u8, y: u8) {
        self.start = true;
        self.a = x;
        self.b = y;
    }
}
```

`#[clocking]` makes a struct the clocking block. Its `#[modport]` field binds
the members under the scope of the same name, `dut.bus`:

```rust
#[rustdv_veryl::clocking]
struct AdderCb {
    #[modport]
    bus: AdderBus,
}
```

The operands are made by constrained randomization
([`rustdv_veryl::random`](random.md)), like a SystemVerilog class with `rand`
members and a `constraint` block. The constraint keeps `a` in the upper half
and `b` in the lower half (up to 128):

```rust
use rustdv_veryl::random::{self, constraints, randomize};

#[randomize]
#[derive(Default)]
struct Operands {
    #[rand]
    a: u8,
    #[rand]
    b: u8,
}

#[constraints]
impl Operands {
    fn carry() {
        a >= 128;
        b <= 128;
    }
}
```

The test binds the clocking block to the `dut` scope and its clock, then
for each random pair drives a request and waits for the response:

```rust
use rustdv_veryl::{posedge_within, tick};

#[rustdv::test]
async fn add(ctx: RustdvCtx) -> std::result::Result<(), TestError> {
    random::set_seed(ctx.seed());
    let dut = ctx.dut();
    let mut cb = AdderCb::new(&dut, dut.signal("clk")?, AdderCbPaths::default())?;

    let mut ops = Operands::default();
    for _ in 0..20 {
        ops.randomize()?;
        let (x, y) = (ops.a, ops.b);
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
```

`tick!(cb)`, the counterpart of SystemVerilog's `@(cb)`, drives the outputs
assigned since the last tick, waits for the rising edge of the clock and
samples every input. It stands for `cb.tick().await`, so it is used in async
code like any rustdv wait. Between ticks the fields do not change on their
own: an input holds the value sampled at the last edge, an output the value
last assigned. `posedge_within!(cb.bus.done, 8)` ticks until `done` rises,
like `@(posedge cb.done)`, and fails the test if it does not within 8 edges.

`random::set_seed(ctx.seed())` seeds the randomization with the test's seed,
which rustdv takes from `RUSTDV_RANDOM_SEED` (or the time when it is unset):
`RUSTDV_RANDOM_SEED=1 veryl test` reproduces a run.

`veryl test` shows the rustdv log as the output of the component:

```text
[tb] cycle 3:    4000.00ns INFO     [add]: 188 + 114 = 302
[tb] cycle 6:   10000.00ns INFO     [add]: 209 + 120 = 329
[tb] cycle 9:   16000.00ns INFO     [add]: 207 + 38 = 245
[tb] cycle 12:   22000.00ns INFO     [add]: 182 + 127 = 309
...
[tb] cycle 60:  118000.00ns INFO     add PASSED
...
[tb] cycle 60: REGRESSION: PASS
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
- while `rst.assert()` holds a reset, the component gets no `on_clock` (only
  `on_reset`, for a reset passed with `set_reset`), so rustdv's clocks do not
  tick until it is released; start rustdv after the reset sequence

A clocking block samples and drives on rising edges, so a testbench written
with one sees the same cycles as on an event-driven simulator. One time step
is 1 us.

## Clocking blocks

A `#[clocking]` struct mixes `#[modport]` fields with single signals:

```rust
#[rustdv_veryl::clocking]
struct Cb {
    #[modport] bus: AdderBus,             // members under `bus`
    #[output]  irq_ack: bool,             // the signal `irq_ack`
    #[input]   state: u8,                 // the signal `state`
}
```

`new(scope, clock, paths)` binds each field under `scope` by its name. The
generated `<Name>Paths` struct overrides that path, field by field; the rest
keep their names:

```rust
let cb = Cb::new(
    &dut,
    dut.signal("clk")?,
    CbPaths {
        bus: Some("harness.bus"),
        state: Some("u_core.state"),
        ..Default::default()
    },
)?;
```

`new` fails if a signal is missing or wider than its field's type. Fields can
be `bool`, `u8` to `u128`, `BigUint` or `LogicArray`. As SystemVerilog does
for two-state types, all but `LogicArray` read X and Z bits as 0; use
`LogicArray` for a signal whose X/Z matters.

Besides `tick!`, a clocking block waits on its fields. Each wait ticks until
an expression over the clocking block matches:

| Wait | Returns at the edge where | SystemVerilog |
|---|---|---|
| `posedge!(cb.bus.done)` | the `bool` goes from false to true | `@(posedge cb.done)` |
| `negedge!(cb.bus.done)` | the `bool` goes from true to false | `@(negedge cb.done)` |
| `edge!(cb.bus.done)` | the `bool` changes either way | `@(edge cb.done)` |
| `changed!(cb.state)` | the value changes (any `PartialEq`; `(cb.a, cb.b)` for several) | `@(cb.state)` |
| `cb.wait(\|c\| c.state == 3).await` | the condition holds; at once if it already does | `wait (cb.state == 3)` |

The expression starts from the clocking block variable and may be any
expression over it (`posedge!(cb.cnt % 4 == 0)`). The macros stand for the
methods of the same name, which take a closure instead
(`cb.posedge(|c| c.bus.done).await`).

These waits never time out. The `_within` forms — `posedge_within!`,
`negedge_within!`, `edge_within!` and `changed_within!` — take a number of
edges after the expression and return `Result<(), ClockingTimeout>`, which `?`
turns into a test failure naming the expression:

```rust
posedge_within!(cb.bus.done, 8)?; // "`cb.bus.done` not seen within 8 cycles"
```

The methods take the limit as `.within(n)`, and `.label(...)` to name the
wait in the message: `cb.posedge(|c| c.bus.done).within(8).label("done").await?`.

The clocking blocks use only rustdv's handles, which stay available for
anything else: `dut.signal("x")?` gives a `LogicHandle` to read and write
directly.

## Limitations

- rustdv sees only what the test module registers through the methods.
- Falling edges are emulated, not real: driving on them is not supported.
- One `rustdv_tb` per Veryl test.
- Clocks and resets inside a child instance cannot be passed yet; pass them
  through the test module.
