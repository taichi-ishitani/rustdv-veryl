//! Tests of `rustdv_veryl::random`.

use std::collections::HashSet;

use num_bigint::BigUint;
use rustdv_veryl::random::{
    ConstraintSet, RandEnum, RandErrorKind, constraint_fn, constraints, randcase, randomize,
    randomize_with, set_seed, std_randomize, urandom_range,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash, RandEnum)]
enum Op {
    #[default]
    Add,
    Sub,
    Mul = 5,
}

#[randomize]
#[derive(Clone, Debug, Default)]
struct AluCmd {
    #[rand]
    op: Op,
    #[rand]
    a: u8,
    #[rand]
    b: u8,
    limit: u8,
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

#[test]
fn constraints_hold() {
    set_seed(1);
    let mut cmd = AluCmd {
        limit: 8,
        ..Default::default()
    };
    let mut ops = HashSet::new();
    for _ in 0..200 {
        cmd.randomize().unwrap();
        ops.insert(cmd.op);
        assert_ne!(cmd.a, 0);
        assert!((1..=15).contains(&cmd.b) || cmd.b >= 240);
        if cmd.op == Op::Mul {
            assert!(cmd.a < 16 && cmd.b < 8);
        }
        assert_eq!(cmd.limit, 8);
    }
    assert_eq!(ops.len(), 3, "every variant appears: {ops:?}");
}

#[test]
fn values_vary_and_reproduce() {
    let run = |seed| {
        let mut cmd = AluCmd {
            limit: 100,
            ..Default::default()
        };
        cmd.srandom(seed);
        (0..20)
            .map(|_| {
                cmd.randomize().unwrap();
                (cmd.op, cmd.a, cmd.b)
            })
            .collect::<Vec<_>>()
    };
    let x = run(7);
    assert_eq!(x, run(7));
    assert_ne!(x, run(8));
    let distinct: HashSet<_> = x.iter().map(|v| v.1).collect();
    assert!(distinct.len() > 10, "a varies: {distinct:?}");
}

#[test]
fn unsat_reports_and_keeps_the_object() {
    set_seed(2);
    let mut cmd = AluCmd {
        limit: 0,
        a: 42,
        ..Default::default()
    };
    let e = randomize_with!(cmd, {
        op == Op::Mul;
    })
    .unwrap_err();
    assert_eq!(e.kind(), RandErrorKind::Unsatisfiable);
    let r = e.report();
    assert!(r.contains("randomize() failed: AluCmd"), "{r}");
    assert!(r.contains("AluCmd::small_mul"), "{r}");
    assert!(r.contains("b < limit"), "{r}");
    assert_eq!(cmd.a, 42, "unchanged on failure");
}

#[test]
fn inline_constraints_and_locals() {
    set_seed(3);
    let mut cmd = AluCmd {
        limit: 200,
        ..Default::default()
    };
    let max_a = 5u8;
    for _ in 0..50 {
        randomize_with!(cmd, {
            a < local::max_a;
            b == { 2 * 7 };
        })
        .unwrap();
        assert!(cmd.a < 5);
        assert_eq!(cmd.b, 14);
    }
}

#[test]
fn rand_off_and_constraint_off() {
    set_seed(4);
    let mut cmd = AluCmd {
        limit: 0,
        a: 3,
        ..Default::default()
    };
    cmd.rand_off(AluCmd::A);
    for _ in 0..20 {
        cmd.randomize().unwrap();
        assert_eq!(cmd.a, 3);
    }
    cmd.rand_on(AluCmd::A);
    cmd.constraint_off(AluCmd::NO_ZERO);
    let mut zero = false;
    for _ in 0..300 {
        randomize_with!(cmd, {
            a < 4;
            op != Op::Mul;
        })
        .unwrap();
        zero |= cmd.a == 0;
    }
    assert!(zero, "a == 0 is allowed with no_zero off");
    assert!(!cmd.is_constraint_on(AluCmd::NO_ZERO));
}

// ---- constraint sets ----

#[derive(Clone)]
#[constraints(for AluCmd)]
struct Corners;

#[constraints(for AluCmd)]
impl Corners {
    fn corners() {
        a.inside([1, 255]);
    }
    #[replaces(small_mul)]
    fn small_mul() {
        if op == Op::Mul {
            a < 2;
        }
    }
}

#[constraints(for AluCmd)]
struct MaxB {
    max: u8,
}

#[constraints(for AluCmd)]
impl MaxB {
    fn max_b() {
        b <= self.max;
    }
}

#[test]
fn attached_sets_and_replaces() {
    set_seed(5);
    let mut cmd = AluCmd {
        limit: 0,
        ..Default::default()
    }
    .with_constraints(Corners);
    for _ in 0..100 {
        // small_mul of AluCmd (b < limit = 0) would make Mul impossible;
        // the set replaces it.
        randomize_with!(cmd, {
            op == Op::Mul;
        })
        .unwrap();
        assert_eq!(cmd.a, 1);
    }
    let mut cmd = cmd.with_constraints(MaxB { max: 3 });
    for _ in 0..50 {
        cmd.randomize().unwrap();
        assert!(cmd.a == 1 || cmd.a == 255);
        assert!((1..=3).contains(&cmd.b));
    }
    cmd.clear_constraints();
    cmd.randomize_with((MaxB { max: 1 }, Corners)).unwrap();
    assert_eq!(cmd.b, 1);
    let set = ConstraintSet::new(MaxB { max: 2 });
    let mut cmd = AluCmd::default().with_constraints(set.clone());
    cmd.randomize().unwrap();
    assert!(cmd.b <= 2);
}

// ---- semantics ----

#[randomize]
#[derive(Default)]
struct Arith {
    #[rand]
    a: u8,
    #[rand]
    b: u8,
    #[rand]
    s: i8,
    #[rand]
    flag: bool,
}

#[test]
fn arithmetic_does_not_wrap() {
    set_seed(6);
    let mut x = Arith::default();
    for _ in 0..100 {
        randomize_with!(x, {
            a + b == 10;
        })
        .unwrap();
        assert_eq!(x.a as u32 + x.b as u32, 10);
        randomize_with!(x, {
            a * b == 300;
        })
        .unwrap();
        assert_eq!(x.a as u32 * x.b as u32, 300);
        randomize_with!(x, {
            a - b == -3;
        })
        .unwrap();
        assert_eq!(x.a as i32 - x.b as i32, -3);
        randomize_with!(x, {
            s < 0;
            s / 3 == -2;
        })
        .unwrap();
        assert_eq!(x.s / 3, -2);
        randomize_with!(x, {
            s % 5 == -4;
        })
        .unwrap();
        assert_eq!(x.s % 5, -4);
        randomize_with!(x, {
            100 / b == 7;
        })
        .unwrap();
        assert_eq!(100 / x.b, 7);
    }
}

#[test]
fn bitwise_logical_and_casts() {
    set_seed(7);
    let mut x = Arith::default();
    for _ in 0..50 {
        randomize_with!(x, {
            bit_not(a) == 0xf0;
        })
        .unwrap();
        assert_eq!(x.a, 0x0f);
        randomize_with!(x, {
            !a;
        })
        .unwrap();
        assert_eq!(x.a, 0);
        randomize_with!(x, {
            (a & 0x0f) == 3;
            (a >> 4) == 0xa;
        })
        .unwrap();
        assert_eq!(x.a, 0xa3);
        randomize_with!(x, {
            (a << 1) == 0;
            a != 0;
        })
        .unwrap();
        assert_eq!(x.a, 0x80);
        randomize_with!(x, {
            s >> 1 == -3;
        })
        .unwrap();
        assert_eq!(x.s >> 1, -3);
        randomize_with!(x, {
            (a as i8) == -1;
        })
        .unwrap();
        assert_eq!(x.a, 255);
        randomize_with!(x, {
            -1 < (a as u8);
            s < -100;
        })
        .unwrap();
        assert!(x.s < -100);
        randomize_with!(x, {
            flag == (a > 128);
            a > 200;
        })
        .unwrap();
        assert!(x.flag);
    }
}

// ---- arrays ----

#[randomize]
#[derive(Default, Debug)]
struct Arrays {
    #[rand]
    fixed: [u8; 4],
    #[rand]
    list: Vec<u16>,
    #[rand]
    idx: u8,
}

#[constraints]
impl Arrays {
    fn sizes() {
        list.len() >= 2;
        list.len() <= 6;
    }
    fn increasing() {
        for i in 0..list.len() {
            if i + 1 < list.len() {
                list[i] < list[i + 1];
            }
        }
    }
    fn distinct() {
        unique(fixed);
        for i in 0..4 {
            fixed[i] < 8;
        }
    }
}

#[test]
fn fixed_and_dynamic_arrays() {
    set_seed(8);
    let mut x = Arrays::default();
    let mut lens = HashSet::new();
    for _ in 0..100 {
        x.randomize().unwrap();
        lens.insert(x.list.len());
        assert!((2..=6).contains(&x.list.len()));
        assert!(x.list.windows(2).all(|w| w[0] < w[1]), "{:?}", x.list);
        let set: HashSet<_> = x.fixed.iter().collect();
        assert_eq!(set.len(), 4);
        assert!(x.fixed.iter().all(|v| *v < 8));
    }
    assert!(lens.len() > 2, "lengths vary: {lens:?}");
    for _ in 0..20 {
        randomize_with!(x, {
            list.len() == 3;
            list.sum() == 6;
        })
        .unwrap();
        assert_eq!(x.list.iter().sum::<u16>(), 6);
        assert!(x.list.windows(2).all(|w| w[0] < w[1]), "{:?}", x.list);
    }
    randomize_with!(x, {
        idx < 4;
        fixed[idx] == 7;
    })
    .unwrap();
    assert_eq!(x.fixed[x.idx as usize], 7);
    randomize_with!(x, {
        list.len() == 4;
        for i in 0..list.len() {
            list[i] == i * 10;
        }
    })
    .unwrap();
    assert_eq!(x.list, vec![0, 10, 20, 30]);
}

#[test]
fn array_errors() {
    set_seed(9);
    let mut x = Arrays::default();
    x.constraint_off(Arrays::SIZES);
    let e = x.randomize().unwrap_err();
    assert_eq!(e.kind(), RandErrorKind::Invalid);
    assert!(e.report().contains("not bounded"), "{e}");
    x.constraint_on(Arrays::SIZES);
    let e = randomize_with!(x, {
        fixed[4] == 0;
    })
    .unwrap_err();
    assert!(e.report().contains("index 4 is outside"), "{e}");
}

// ---- nested objects ----

#[randomize]
#[derive(Default, Debug, Clone)]
struct Delay {
    #[rand]
    min: u8,
    #[rand]
    max: u8,
}

#[constraints]
impl Delay {
    fn order() {
        min <= max;
        max < 20;
    }
}

#[randomize]
#[derive(Default, Debug)]
struct Packet {
    #[rand]
    delay: Delay,
    #[rand]
    beats: Vec<Delay>,
    cfg: Delay,
    #[rand]
    len: u8,
}

#[constraints]
impl Packet {
    fn shape() {
        delay.min > 5;
        beats.len() == len;
        len.inside([1..=3]);
        for i in 0..beats.len() {
            beats[i].max == delay.max;
        }
        len <= cfg.max;
    }
}

#[test]
fn nested_objects_are_solved_together() {
    set_seed(10);
    let mut p = Packet {
        cfg: Delay {
            min: 0,
            max: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    for _ in 0..50 {
        p.randomize().unwrap();
        assert!(p.delay.min > 5 && p.delay.min <= p.delay.max && p.delay.max < 20);
        assert_eq!(p.beats.len(), p.len as usize);
        assert!(p.len >= 1 && p.len <= 2);
        for d in &p.beats {
            assert_eq!(d.max, p.delay.max);
            assert!(d.min <= d.max);
        }
        assert_eq!((p.cfg.min, p.cfg.max), (0, 2), "state is not randomized");
    }
    p.delay.constraint_off(Delay::ORDER);
    let mut seen = false;
    for _ in 0..100 {
        p.randomize().unwrap();
        seen |= p.delay.min > p.delay.max;
    }
    assert!(seen, "the nested object's own constraint is off");
}

// ---- hooks, soft, dist, solve_before ----

#[randomize]
#[derive(Default)]
struct Hooked {
    #[rand]
    x: u8,
    pre_calls: u32,
    post_x: u8,
}

#[constraints]
impl Hooked {
    fn soft_small() {
        soft(x < 10);
    }
    #[pre_randomize]
    fn pre(&mut self) {
        self.pre_calls += 1;
    }
    #[post_randomize]
    fn post(&mut self) {
        self.post_x = self.x;
    }
}

#[test]
fn hooks_and_soft() {
    set_seed(11);
    let mut h = Hooked::default();
    h.randomize().unwrap();
    assert!(h.x < 10);
    assert_eq!(h.pre_calls, 1);
    assert_eq!(h.post_x, h.x);
    randomize_with!(h, {
        x > 100;
    })
    .unwrap();
    assert!(h.x > 100, "the hard constraint wins");
    randomize_with!(h, {
        soft(x == 50);
    })
    .unwrap();
    assert_eq!(h.x, 50, "a later soft wins over an earlier one");
}

#[randomize]
#[derive(Default)]
struct Dist {
    #[rand]
    x: u8,
    #[rand]
    y: u8,
}

#[test]
fn dist_weights() {
    set_seed(12);
    let mut d = Dist::default();
    let mut zero = 0;
    let mut low = 0;
    let n = 2000;
    for _ in 0..n {
        randomize_with!(d, {
            dist!(x, 0 => each 1, 1..=3 => total 3);
        })
        .unwrap();
        assert!(d.x <= 3);
        if d.x == 0 {
            zero += 1;
        } else {
            low += 1;
        }
    }
    // 1 : 3
    assert!((350..650).contains(&zero), "zero {zero} of {n}");
    assert!(low > zero);
}

#[test]
fn solve_before_spreads_the_first_variable() {
    set_seed(13);
    let mut d = Dist::default();
    let mut zeros = 0;
    for _ in 0..400 {
        randomize_with!(d, {
            x.inside([0, 1]);
            if x == 0 {
                y == 0;
            }
            solve_before([x], [y]);
        })
        .unwrap();
        zeros += (d.x == 0) as u32;
    }
    assert!(
        (120..280).contains(&zeros),
        "x == 0 about half the time: {zeros}"
    );
}

// ---- constraint functions ----

#[constraint_fn]
fn aligned(addr: u32, size: u32) -> bool {
    addr % size == 0
}

#[constraint_fn]
fn bytes(len: u8, size: u8) -> u16 {
    ((len as u16) + 1) << size
}

#[constraint_fn]
fn in_window(v: u32) -> bool {
    if v > 100 {
        v < 200;
    }
    aligned(v, 2)
}

#[randomize]
#[derive(Default)]
struct Req {
    #[rand]
    addr: u32,
    #[rand]
    len: u8,
    #[rand]
    size: u8,
}

#[constraints]
impl Req {
    fn align() {
        aligned(addr, 16);
        size < 4;
    }
    fn limit() {
        bytes(len, size) <= 64;
        in_window(addr);
    }
}

#[test]
fn constraint_functions_expand() {
    set_seed(14);
    let mut r = Req::default();
    for _ in 0..100 {
        r.randomize().unwrap();
        assert_eq!(r.addr % 16, 0);
        assert!(((r.len as u32 + 1) << r.size) <= 64);
        assert!(r.addr <= 100 || r.addr < 200);
    }
    randomize_with!(r, {
        !aligned(addr, 32);
    })
    .unwrap();
    assert_eq!(r.addr % 32, 16);
}

// ---- wide values ----

#[randomize]
#[derive(Default)]
struct Wide {
    #[rand(width = 200)]
    w: BigUint,
}

#[test]
fn wide_values() {
    set_seed(15);
    let mut w = Wide::default();
    for _ in 0..20 {
        randomize_with!(w, {
            w > { BigUint::from(1u8) << 199 };
            (w & 0xff) == 0x5a;
        })
        .unwrap();
        assert!(w.w.bits() == 200);
        assert_eq!(&w.w & BigUint::from(0xffu8), BigUint::from(0x5au8));
    }
}

// ---- other facilities ----

#[test]
fn std_randomize_randcase_urandom() {
    set_seed(16);
    let mut x: u8 = 0;
    let mut y: u16 = 0;
    for _ in 0..50 {
        std_randomize!([x, y], {
            x < 10;
            y == x * 3;
        })
        .unwrap();
        assert!(x < 10);
        assert_eq!(y, x as u16 * 3);
    }
    let mut hits = [0u32; 2];
    for _ in 0..1000 {
        randcase! {
            1 => hits[0] += 1,
            3 => hits[1] += 1,
            0 => panic!("zero weight"),
        }
    }
    assert!((150..350).contains(&hits[0]), "{hits:?}");
    for _ in 0..100 {
        let v = urandom_range(10, 3);
        assert!((3..=10).contains(&v));
    }
}
