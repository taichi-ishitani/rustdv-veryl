# rustdv-veryl

Run [rustdv](https://github.com/rustdv/rustdv) testbenches on
[Veryl](https://veryl-lang.org)'s built-in simulator.

With this crate rustdv testbench code — `#[rustdv::test]` functions,
components, sequences — runs inside a Veryl verification component, driven
by `veryl test`. One line, `rustdv_veryl::export!("rustdv_tb");`, makes the
crate holding the tests that component; the Veryl test module passes it the
clocks, signals and interfaces to work on.

> **Status:** experimental. It needs two features no release provides yet:
> passing references to component methods in Veryl
> ([veryl-lang/veryl#3701](https://github.com/veryl-lang/veryl/pull/3701)),
> and the rustdv backend interface
> ([rustdv/rustdv#25](https://github.com/rustdv/rustdv/pull/25)).

## Documentation

The [guide](docs/guide.md) walks through the example step by step: the
interface and the design, the test module, the component crate, the
testbench component and the rustdv testbench written with a clocking block,
then timing, clocking blocks in detail and limitations.

## Example

[`examples/adder`](examples/adder) verifies a small adder through an
interface:

```sh
cd examples/adder
veryl test
```

`veryl test` has to be a Veryl built from veryl-lang/veryl#3701 until that
is released.

## License

Licensed under the [MIT license](LICENSE).
