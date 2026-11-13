# rustdv-veryl

Run [rustdv](https://github.com/rustdv/rustdv) testbenches on
[Veryl](https://veryl-lang.org)'s built-in simulator.

With this crate rustdv testbench code — `#[rustdv::test]` functions,
components, sequences — runs inside a Veryl verification component, driven
by `veryl test`.

> **Status:** experimental. It needs the rustdv backend interface
> (`rustdv_gpi::backend`), which no released rustdv provides yet.

## Documentation

The [guide](docs/guide.md) walks through the example step by step: the
interface and the design, the test module, the component crate, the wiring
and the rustdv testbench written with a clocking block, then timing, clocking
blocks in detail and limitations.

## Example

[`examples/adder`](examples/adder) verifies a small adder through an
interface:

```sh
cd examples/adder
veryl test
```

## License

Licensed under the [MIT license](LICENSE).
