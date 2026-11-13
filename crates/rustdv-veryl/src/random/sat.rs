//! The SAT solver interface. The bit-blaster and the solving stages use
//! only [`SatSolver`], so another solver can be plugged in with
//! [`set_solver_factory`](super::set_solver_factory).

use std::time::Instant;

/// A variable of the SAT solver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Var(pub u32);

/// A literal: a variable or its negation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Lit {
    pub var: Var,
    pub positive: bool,
}

impl Lit {
    pub fn pos(var: Var) -> Lit {
        Lit {
            var,
            positive: true,
        }
    }
}

impl std::ops::Not for Lit {
    type Output = Lit;

    fn not(self) -> Lit {
        Lit {
            var: self.var,
            positive: !self.positive,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SolveResult {
    Sat,
    Unsat,
    /// Stopped by the deadline.
    Interrupted,
}

/// A SAT solver with assumptions and unsat cores.
pub trait SatSolver {
    /// A new variable; `polarity` is the value the solver tries first.
    fn new_var(&mut self, polarity: Option<bool>) -> Var;
    fn add_clause(&mut self, lits: &[Lit]);
    fn solve(&mut self, assumptions: &[Lit]) -> SolveResult;
    /// The value of a variable in the last model.
    fn value(&self, var: Var) -> bool;
    /// After `Unsat`: whether the assumption on `var` is in the core.
    fn in_unsat_core(&self, var: Var) -> bool;
    /// Stops a running `solve` once the deadline passes.
    fn set_deadline(&mut self, deadline: Option<Instant>);
}

/// The default solver: batsat (MiniSat 2.2).
#[cfg(feature = "batsat")]
pub mod batsat_impl {
    use super::*;
    use batsat::{BasicCallbacks, SolverInterface, SolverOpts, lbool};

    pub struct Batsat {
        solver: batsat::BasicSolver,
        model: Vec<bool>,
        core: Vec<u32>,
    }

    impl Batsat {
        pub fn new(seed: u64) -> Batsat {
            let opts = SolverOpts {
                // batsat wants a seed in (0, 1e9) as a float.
                random_seed: (seed % 999_999_937 + 1) as f64,
                rnd_init_act: true,
                ..SolverOpts::default()
            };
            Batsat {
                solver: batsat::Solver::new(opts, BasicCallbacks::new()),
                model: Vec::new(),
                core: Vec::new(),
            }
        }

        fn lit(l: Lit) -> batsat::Lit {
            batsat::Lit::new(
                // Variables are created in order, so the index is the id.
                batsat_var(l.var.0),
                l.positive,
            )
        }
    }

    fn batsat_var(i: u32) -> batsat::Var {
        batsat::Var::unsafe_from_idx(i)
    }

    impl SatSolver for Batsat {
        fn new_var(&mut self, polarity: Option<bool>) -> Var {
            let pol = match polarity {
                None => lbool::UNDEF,
                Some(b) => lbool::new(b),
            };
            let v = self.solver.new_var(pol, true);
            Var(v.idx())
        }

        fn add_clause(&mut self, lits: &[Lit]) {
            let mut c: Vec<batsat::Lit> = lits.iter().map(|l| Self::lit(*l)).collect();
            self.solver.add_clause_reuse(&mut c);
        }

        fn solve(&mut self, assumptions: &[Lit]) -> SolveResult {
            let a: Vec<batsat::Lit> = assumptions.iter().map(|l| Self::lit(*l)).collect();
            let r = self.solver.solve_limited(&a);
            if r == lbool::TRUE {
                let n = self.solver.num_vars();
                self.model = (0..n)
                    .map(|i| self.solver.value_var(batsat_var(i)) == lbool::TRUE)
                    .collect();
                SolveResult::Sat
            } else if r == lbool::FALSE {
                self.core = self
                    .solver
                    .unsat_core()
                    .iter()
                    .map(|l| l.var().idx())
                    .collect();
                SolveResult::Unsat
            } else {
                SolveResult::Interrupted
            }
        }

        fn value(&self, var: Var) -> bool {
            self.model.get(var.0 as usize).copied().unwrap_or(false)
        }

        fn in_unsat_core(&self, var: Var) -> bool {
            self.core.contains(&var.0)
        }

        fn set_deadline(&mut self, deadline: Option<Instant>) {
            if let Some(d) = deadline {
                self.solver.cb_mut().set_stop(move || Instant::now() >= d);
            }
        }
    }
}
