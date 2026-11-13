# Constrained random (`rustdv_veryl::random`)

> This is the user-facing specification: how to write random structs and
> constraints, and what they mean. How it is implemented, and why the
> design is as it is, is in [random-design.md](random-design.md).

SystemVerilog verification depends on `rand` variables, `constraint`
blocks and `randomize()`. This module gives a rustdv testbench the same
model on plain Rust structs:

- a struct declares random fields and state fields,
- constraints are written as functions whose body uses Rust syntax,
- `randomize()` solves all enabled constraints together and writes the
  solution back, or returns an error and leaves the object unchanged.

The model follows IEEE 1800-2023 (chapter 18), with simpler rules in a
few places (see [semantics](#semantics)).

## Contents

- [A first example](#a-first-example)
- [Declaring a random struct](#declaring-a-random-struct)
- [Writing constraints](#writing-constraints)
- [Inline constraints: `randomize_with!`](#inline-constraints-randomize_with)
- [Constraint sets: reuse and variation](#constraint-sets-reuse-and-variation)
- [Controlling randomization](#controlling-randomization)
- [Seeds and random stability](#seeds-and-random-stability)
- [Use in rustdv sequences](#use-in-rustdv-sequences)
- [Other random facilities](#other-random-facilities)
- [Errors and diagnostics](#errors-and-diagnostics)
- [Semantics](#semantics)
- [Not supported](#not-supported)

## A first example

```rust
use rustdv_veryl::random::{constraints, randomize, randomize_with};

#[derive(Clone, Copy, PartialEq, Eq, Default, RandEnum)]
enum Op { #[default] Add, Sub, Mul }

#[randomize]
#[derive(Clone, Default)]
struct AluCmd {
    #[rand] op: Op,
    #[rand] a: u8,
    #[rand] b: u8,
    limit: u8,                        // state: a constant while solving
}

#[constraints]
impl AluCmd {
    fn small_mul() {
        if op == Op::Mul {
            a < 16;
            b < limit;
        }
    }
    fn no_zero() {
        a != 0;
        b.inside([1..=15, 240..=255]);
    }
}

let mut cmd = AluCmd { limit: 8, ..Default::default() };
cmd.randomize()?;
randomize_with!(cmd, { a < local::max_a; })?;
```

## Declaring a random struct

`#[randomize]` is an attribute macro on the struct. It adds a hidden
field holding the per-object random state:

- the object's random number generator (see [seeds](#seeds-and-random-stability)),
- the `rand_on`/`rand_off` and `constraint_on`/`constraint_off` flags,
- the attached constraint sets (see [constraint sets](#constraint-sets-reuse-and-variation)),

Because of the hidden field, the struct cannot be built with a struct
literal that lists every field; build it with `Default` (`..Default::default()`)
or a constructor of your own.

### Fields

| Field | Meaning |
|---|---|
| `#[rand] x: T` | a random variable (SV `rand`) |
| no attribute | state: read as a constant when `randomize()` is called |
| `#[randc] x: T` | *later:* SV `randc` |

### Value types

| Rust type | SV type |
|---|---|
| `bool` | `bit` |
| `u8`, `u16`, `u32`, `u64`, `u128` | `bit [N-1:0]` |
| `i8`, `i16`, `i32`, `i64`, `i128` | `bit signed [N-1:0]` |
| `BigUint` with `#[rand(width = N)]` | `bit [N-1:0]` wider than 128 bits; the width is required |
| an enum with `#[derive(RandEnum)]` | an enum: the value is limited to the listed variants |
| `[T; N]` | fixed-size unpacked array |
| `Vec<T>` | dynamic array: the length is random too |
| a `#[randomize]` struct | a nested random object |

Signed values wider than 128 bits are not supported until needed.

### Arrays

The element type `T` may be any value type above, including a
`#[randomize]` struct.

A `Vec<T>` field has a random length: its size is solved first, from
the constraints on `v.len()`, and then its elements (as in SV
§18.5.8.1). The vector is truncated or extended to the new size; new
elements are made with `T::default()`, so `T: Default` is required.

Rules:

- A `Vec` field needs an upper bound on its length in the constraints
  (`v.len() <= 16`), at most 65536; without one, `randomize()` fails with
  an error instead of unrolling without limit.
- As in SV, a constraint cannot choose the size from the element values
  (e.g. picking the length so that `v.sum() == 100`); the size is
  fixed before the elements are solved.
- A constant index that falls outside the array is an error:
  `randomize()` fails and names the index. Guard it in the constraint:
  `for i in 0..v.len() { if i + 1 < v.len() { v[i] < v[i + 1]; } }`.
- An index may itself be random (`v[idx] == 3` with `#[rand] idx`) when
  the elements are values, not structs; the index is then also kept in
  range.
- Multi-dimensional arrays (an array whose element is an array) are not
  supported.

### Nested objects

A `#[rand]` field whose type is a `#[randomize]` struct is flattened into
the parent's problem: its variables and enabled constraints are solved
together with the parent's, and the parent's constraints may refer to
its fields (`cfg.min_delay < 4`). Each nested object keeps its own
on/off flags and random state. A nested object without `#[rand]` is
state, read as constants.

This is also how a variation that carries more data than an existing
item is written (what SV does with `extends` and new fields): embed the
item as a `#[rand]` field (`#[rand] base: AluCmd`) and add the new
fields beside it. A constraint of the embedded item that the new type
does not want is disabled with `constraint_off`.

## Writing constraints

```rust
#[constraints]
impl AluCmd {
    fn name() { /* constraint items */ }
}
```

- Each function is one named constraint block (SV `constraint name { }`).
  The functions have no parameters and are never executed as Rust code.
- The macro parses each body and generates code that builds the
  constraint IR when `randomize()` is called. The body must be valid
  Rust syntax, so the usual editor support (highlighting, formatting)
  keeps working.
- For each block the macro generates a name constant
  (`AluCmd::SMALL_MUL`) used by `constraint_on`/`constraint_off` and
  `#[replaces]`. For each field `#[randomize]` generates a constant
  (`AluCmd::A`) used by `rand_on`/`rand_off`.

### Names in a constraint body

| Written as | Meaning |
|---|---|
| a single identifier (`a`, `limit`) | a field of the struct; a name that is not a field is a compile error |
| a path (`Op::Mul`, `Self::MAX`, `crate::LIMIT`) | a constant, evaluated in Rust when `randomize()` is called |
| `{ expr }` | any Rust expression, evaluated once as a constant when `randomize()` is called; inside it, names are plain Rust names, not fields |
| `f(args)` | a call of a `#[constraint_fn]` function, expanded in place; other functions are called inside `{ }` |
| `local::x` (in `randomize_with!` only) | the variable `x` of the caller (SV `local::`) |
| `self.x` (in a constraint set only) | the field `x` of the set, as a constant (see [parameterized sets](#parameterized-sets)) |
| `field.sub` | a field of a nested object |

A misspelled field name is a compile error.

### Constraint items

A statement `expr;` states that the expression is true. The other forms
are:

| SV | Rust |
|---|---|
| `a < b;` | `a < b;` |
| `x inside {1, [10:20]};` | `x.inside([1, 10..=20]);` |
| `x inside {q}` (a queue or array) | `x.inside({ expr })`, where `expr` is a Rust collection of values or ranges |
| `!(x inside {...});` | `!x.inside([...]);` |
| `x dist {0 := 6, [1:3] :/ 3};` | `dist!(x, 0 => each 6, 1..=3 => total 3);` |
| `a -> { ... }` | `if a { ... }` |
| `if (a) ... else ...` | `if a { ... } else { ... }` |
| `~a`, `!a` | `bit_not(a)`, `!a` (`!` is logical, as in SV) |
| `soft x == 3;` | `soft(x == 3);` |
| `unique {a, b, c};` | `unique([a, b, c]);` or `unique(arr);` |
| `foreach (arr[i]) arr[i] < 10;` | `for i in 0..arr.len() { arr[i] < 10; }` |
| `solve a before b;` | `solve_before([a], [b]);` |
| `arr.size()` | `arr.len()` |
| `arr.sum()`, `product()`, `and()`, `or()`, `xor()` | the same names |

Notes:

- **Implication** has no separate form: SV `a -> { ... }` means the same
  as an `if` without `else`.
- **`dist` weights** use keywords instead of `:=` and `:/`, which do not
  read well outside SV:
  - `each w` — every value in the range gets weight `w` (SV `:=`),
  - `total w` — the range as a whole gets weight `w`, divided among its
    values (SV `:/`).
- Ranges use Rust syntax: `lo..=hi` (inclusive) and `lo..hi` (exclusive).
- **Built-in functions** (`bit_not`, `soft`, `unique`, `solve_before`)
  are recognized by name and called outside `{ }` like `#[constraint_fn]`
  functions. `bit_not(a)` is the bitwise complement at the width of `a`;
  Rust has no `~` operator, and `!` is the logical NOT here (see
  [semantics](#semantics)). `bit_not` is also a public function of
  `rustdv_veryl::random`, for Rust code.

### Functions in constraints

- **`#[constraint_fn]` functions are expanded.** A function marked
  `#[constraint_fn]` has a body written in the same constraint syntax,
  and a call to it is expanded in place. The solver sees through the
  call, so its arguments may be random variables and are solved together
  with the rest.

  ```rust
  #[constraint_fn]
  fn aligned(addr: u32, size: u32) -> bool {
      addr % size == 0
  }

  #[constraint_fn]
  fn burst_bytes(len: u8, size: u8) -> u16 {
      ((len as u16) + 1) << size
  }

  #[constraints]
  impl AxiReq {
      fn align() { aligned(addr, 4); }
      fn limit() { burst_bytes(len, size) <= 4096; }
  }
  ```

  - A function returning `bool` is used as a constraint; its body is
    constraint items (`expr;`, `if`, ...). A function returning another
    type is expanded as an expression; its body is that expression.
  - Arguments are resized to the parameter types, as `as` does, so the
    widths of bitwise operators and shifts in the body follow the
    parameter types. The result is resized to the return type only at
    the end: in `burst_bytes`, `len` is widened first, because a shift
    of `len + 1` (9 bits) would drop the bits shifted out of 9 bits.
  - The body follows the same rules as a `#[constraints]` function:
    outside `{ }` it can call only `#[constraint_fn]` functions, and other
    functions are called inside `{ }`. Inside `{ }`, the function's
    parameters cannot be used (compile error), since they may be random
    variables; only expressions independent of the arguments, such as
    constants, are allowed there.
  - Recursion is not supported. A function calling itself is a compile
    error; mutual recursion is caught by a limit on the expansion depth
    when the IR is built, and `randomize()` fails.
  - A `#[constraint_fn]` function can be called from other modules and
    crates, so a VIP can export constraint functions for its users.

- **Other Rust functions are ordinary Rust expressions.** Outside
  `{ }`, only `#[constraint_fn]` functions can be called; calling any
  other function there is a compile error. Any other function is called
  inside `{ expr }`, which is Rust code evaluated once as a constant when
  `randomize()` is called, so it can never see a random variable:

  ```rust
  fn bounds() {
      len < { max_len(&cfg) };
      addr.inside({ legal_addrs() });   // a collection computed in Rust
  }
  ```

## Inline constraints: `randomize_with!`

`randomize_with!` adds constraints for one call (SV `randomize() with`):

```rust
let max_a = 100;
randomize_with!(cmd, {
    a < local::max_a;          // a variable of the caller
    b == { cfg.width * 2 };    // any Rust expression, as a constant
})?;
```

Single identifiers refer to fields of the object being randomized, as
in `#[constraints]`; since the macro does not know the object's type, a
misspelled field is reported by `randomize()` (an `Invalid` error), not
at compile time. Variables of the caller are written `local::x` (SV
`local::`), and other expressions in `{ }`, evaluated in the caller.

## Constraint sets: reuse and variation

SV testbenches use class inheritance mostly to *add or replace
constraints without changing the fields*. Here that is done with
constraint sets: named groups of constraints for a type, defined
outside it.

```rust
#[constraints(for AluCmd)]
struct Corners;

#[constraints(for AluCmd)]
impl Corners {
    fn corners() {
        a.inside([0, 255]);            // fields of AluCmd, written directly
    }
    #[replaces(small_mul)]             // replaces AluCmd::SMALL_MUL while applied
    fn small_mul() {
        if op == Op::Mul { a < 4; }
    }
}
```

- The body refers to the fields of the target type directly, checked
  at compile time against the field list of that type.
- `#[replaces(name)]` disables the target type's constraint of that name
  while the set is applied (the SV rule that a constraint in a subclass
  overrides one of the same name). `#[replaces(Set::name)]` disables the
  constraint `name` of another set `Set` attached to the same object.
  Constraints of different sets never clash by name, since a set's name
  qualifies them.
- A tuple of sets is a set: `(Corners, NoZero)`.

### Parameterized sets

A set is a struct, so it can have fields. In its constraints, `self.x`
is a field of the set, read as a constant when `randomize()` is called,
while single identifiers stay fields of the target type:

```rust
#[constraints(for AxiItem)]
struct MaxLen { max: u8 }

#[constraints(for AxiItem)]
impl MaxLen {
    fn max_len() { len <= self.max; }   // len: AxiItem's field, self.max: the set's
}

let item = AxiItem::master().with_constraints(MaxLen { max: 8 });
```

One set type then covers variations that differ only in values, instead
of a set type per value.

### Constraint layers

The constraints of a type and the constraint sets are the same kind of
thing: named constraint blocks that are turned into the IR when
`randomize()` is called. They differ in where they apply:

| Layer | Defined by | Applies to |
|---|---|---|
| the type's constraints | `#[constraints] impl AluCmd` | every object of the type (SV class constraints) |
| attached sets | `#[constraints(for AluCmd)]`, attached with `with_constraints` | the object they are attached to, on every call |
| per-call constraints | `randomize_with(set)`, `randomize_with!` | that call only |

All layers are solved together as one problem; a later layer does not
take priority over an earlier one, except that `#[replaces]` removes a
named block of an earlier layer and `soft` constraints follow the SV
priority rules (later blocks win). `constraint_off` disables a block of
any layer by its name (`AluCmd::SMALL_MUL`, `Corners::CORNERS`).

The type's constraints are always present, in the way the methods of
a type are. A set expresses a variation that some objects have and
others do not.

### Applying a set

```rust
// 1. Attached to the object: applied on every randomize().
let mut cmd = AluCmd::default().with_constraints(Corners);
cmd.randomize()?;
cmd.set_constraints(NoZero);                 // replace all attached sets
cmd.clear_constraints();                     // detach all sets

// 2. For one call only.
cmd.randomize_with(Corners)?;

// 3. Inline, for one call only.
randomize_with!(cmd, { a != 0; })?;
```

- `with_constraints(self, set) -> Self` is a builder method that *adds*
  a set to those already attached, so it works with `Default` and with
  your own constructors, and a constructor's sets are kept when more
  are added.
- The attached sets are kept in the hidden field as
  `ConstraintSet<T>` values (`Arc<dyn Constraints<T>>`). The type of the
  object does not change (`AluCmd` with or without sets), and `clone()`
  shares them.

Default constraints per role are given by constructors:

```rust
impl AxiItem {
    pub fn master() -> Self { Self::default().with_constraints(MasterDefaults) }
    pub fn slave() -> Self { Self::default().with_constraints(SlaveDefaults) }
}

#[constraints(for AxiItem)]
impl ShortBurst {
    #[replaces(MasterDefaults::len)]
    fn len() { len < 4; }
}

// MasterDefaults (without its `len`) + ShortBurst
let item = AxiItem::master().with_constraints(ShortBurst);
```

A sequence can therefore choose the variation at run time, for example
from a test argument, without generics:

```rust
struct AluSeq { cons: ConstraintSet<AluCmd> }

let seq = AluSeq { cons: ConstraintSet::new(Corners) };
// in the body: AluCmd::default().with_constraints(self.cons.clone())
```

## Controlling randomization

```rust
cmd.rand_off(AluCmd::A);               // SV a.rand_mode(0)
cmd.rand_on(AluCmd::A);
cmd.rand_off_all();                    // SV obj.rand_mode(0)
cmd.rand_on_all();                     // SV obj.rand_mode(1)
cmd.is_rand_on(AluCmd::A);
cmd.constraint_off(AluCmd::SMALL_MUL); // SV small_mul.constraint_mode(0)
cmd.constraint_on(AluCmd::SMALL_MUL);
cmd.is_constraint_on(AluCmd::SMALL_MUL);
```

A field turned off is read as a constant. Names are the generated
constants, so a wrong name is a compile error.

### Hooks

`pre_randomize` and `post_randomize` are called before solving and
after a successful solve, as in SV:

```rust
#[constraints]
impl AluCmd {
    #[pre_randomize]
    fn pre(&mut self) { self.limit = self.limit.max(1); }
    #[post_randomize]
    fn post(&mut self) { /* ... */ }
}
```

These two are ordinary Rust methods, run as Rust code.

## Seeds and random stability

`randomize()` takes no random number generator argument. Results are
reproducible from the test seed, and independent per object, as in SV:

- The module keeps a seed source per thread (SV: the thread's RNG).
  Seed it at the start of a test with the test's seed, so that the test
  reproduces on its own:

  ```rust
  #[rustdv::test]
  async fn random_test(ctx: RustdvCtx) -> Result<(), TestError> {
      random::set_seed(ctx.seed());
      // ...
  }
  ```

  Unseeded, the source starts from `RUSTDV_RANDOM_SEED` (or 1), once
  per thread.
- On its first `randomize()`, an object takes a seed from that source
  and creates its own generator in the hidden field. Later calls use
  the object's generator only, so randomizing one object does not
  change the values another gets (random stability).
- `obj.srandom(seed)` sets the object's seed explicitly.
  `obj.get_randstate()` / `obj.set_randstate(state)` save and restore it.

The order in which objects take their seeds is the order of their first
`randomize()`. Changing that order in a test changes the results, as
changing the order of object creation does in SV.

## Use in rustdv sequences

```rust
impl Sequence for AluSeq {
    type Req = AluCmd;
    type Rsp = AluResult;

    async fn body(&mut self, ctx: &mut SeqCtx<AluCmd, AluResult>) -> Result<(), SeqError> {
        for _ in 0..10 {
            let mut cmd = AluCmd::default().with_constraints(self.cons.clone());
            ctx.start_item(&mut cmd).await?;
            cmd.randomize()?;
            ctx.finish_item(cmd).await?;
        }
        Ok(())
    }
}
```

`RandError` converts into rustdv's error types, so `?` works in a
sequence body and in a test.

## Other random facilities

| SV | Rust |
|---|---|
| `$urandom`, `$urandom_range(hi, lo)` | `random::urandom()`, `random::urandom_range(lo, hi)` |
| `std::randomize(x, y) with { ... }` | `std_randomize!([x, y], { ... })` — randomizes local variables |
| `randcase` | `randcase! { 3 => { ... }, 1 => { ... } }` |
| `randsequence` | *later* |

These use the same per-thread seed source.

Solving settings, per thread:

| Function | Effect |
|---|---|
| `random::set_seed(seed)` | seeds the per-thread seed source |
| `random::set_timeout(Some(d))` | the time limit of one `randomize()` (default 10 s; `None`: no limit) |
| `random::set_solver_factory(f)` | replaces the SAT solver (see [random-design.md](random-design.md#sat-solver)) |

## Errors and diagnostics

`randomize()` returns `Result<(), RandError>`. On failure, no field and
no hidden state changes.

`RandError` carries a report that names the conflicting constraint
items, with their block (or set), source location and text:

Constraints given with `randomize_with!` are listed as `with`.

```text
randomize() failed: AluCmd
  rand variables: op, a, b
  rand off: limit
  constraints: AluCmd::small_mul, AluCmd::no_zero
  constraint off: Corners::corners
  unsatisfied:
    AluCmd::small_mul (tests/alu.rs:42): b < limit
    with (tests/alu.rs:97): op == Op::Mul
```

`RandError::kind()` tells the cases apart:

| Kind | Cause |
|---|---|
| `Unsatisfiable` | the constraints conflict (the report above) |
| `Timeout` | the solver ran out of time |
| `Invalid` | the constraints could not be built: a `Vec` without a length bound, an index outside an array, an unknown field (in `randomize_with!`), a field whose type cannot be used |

## Semantics

These rules are simpler than SV's in places; matching SV bit for bit is
not a goal.

- **Arithmetic does not overflow.** `+`, `-` and `*` are computed as
  mathematical integers. The width of each result is decided bottom-up
  so that it cannot overflow: `+`/`-` take the wider operand plus one
  bit, `*` the sum of the operand widths. So `a + b == 10` with
  `a, b: u8` is not satisfied by `a = 250, b = 16`.
- **Comparisons compare values.** Signed and unsigned operands may be
  mixed and compare as integers: `-1 < 0u8` is true.
- **Logical operators** (`!`, `&&`, `||`) and conditions (`if a`) take an
  integer as a truth value, as in SV: non-zero is true. `!a` for `a: u8`
  is `a == 0`, not Rust's bitwise complement.
- **Bitwise operators and shifts** (`&`, `|`, `^`, `<<`, `>>`) work at
  the width of their operands, as with Rust types; bits shifted out of
  that width are dropped. When the operands differ in width, the
  narrower one is extended (sign-extended if signed). The bitwise
  complement, which Rust writes `!a` and SV `~a`, is written
  `bit_not(a)` (`rustdv_veryl::random::bit_not`).
- **Integer literals** take the smallest width that holds their value;
  there is no SV 32-bit default for unsized literals.
- **`/` and `%`** truncate toward zero, as in Rust. An assignment that
  makes the divisor zero is not a solution.
- Rust's overflow checks never apply inside constraints, since the
  bodies are not run as Rust code.
- **Enums** take only their listed variants.
- **Uniformity:** the distribution of solutions is not guaranteed to be
  uniform over the whole solution space. `dist` weights are honored for
  the variable they apply to. `solve_before` changes the order in which
  variables are chosen, as in SV.
- **Atomicity:** `randomize()` either writes all random fields and the
  updated hidden state, or changes nothing.

## Not supported

- `randc` (planned)
- multi-dimensional arrays
- signed values wider than 128 bits
- `randsequence`
- plain Rust functions taking random variables (use `#[constraint_fn]`)
- generic `#[randomize]` structs
- more than one `#[constraints] impl` block per type
- a random index into an array of structs
