//! The generative testing layer for the manifest engine (prior-art item R10).
//!
//! The hand-written matrices next door assert the rows a human thought of. This
//! module asserts the *properties* — Dropbox found its reconciler bugs with
//! CanopyCheck (generate a tree, perturb it into three, run the planner,
//! demand convergence and termination, minimize the counterexample) and Trinity
//! (reorder, delay, and fail every network call; crash and restart at random
//! points). Both translate almost directly onto this engine because the
//! reconciler is already a pure decision over three inputs and the driver
//! already takes its clock and its transport as seams.
//!
//! Everything is seeded from a compile-time constant so CI is reproducible;
//! system randomness and wall-clock seeds are deliberately absent. Two
//! environment variables exist for local exploration only:
//!
//! - `BOWLINE_SIM_SEED` — base seed, printed in every failure message.
//! - `BOWLINE_SIM_CASES` — cases per property (defaults are sized for CI).

mod chaos_transport;
mod convergence_tests;
mod cost_property_tests;
mod fleet;
mod rng;
mod scenario;
mod shrink;
mod simulation_tests;
mod tree;

use rng::Seed;

/// The default base seed. Changing it changes which cases CI explores, so treat
/// it as a knob to turn deliberately (to sweep a new region) rather than a value
/// to churn.
const DEFAULT_SEED: Seed = Seed::new(0xB01D_5EED);
const SEED_ENV: &str = "BOWLINE_SIM_SEED";
const CASES_ENV: &str = "BOWLINE_SIM_CASES";

fn base_seed() -> Seed {
    std::env::var(SEED_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map_or(DEFAULT_SEED, Seed::new)
}

/// The environment that replays a failing case.
///
/// Cases are derived from the base seed by index, so a case seed alone does not
/// reproduce anything: replay means re-running the same base seed through the
/// same number of cases. Printing the recipe rather than a bare number is the
/// difference between a reproducible failure and a number in a log.
fn replay_hint(base: Seed, index: u32) -> String {
    format!(
        "{SEED_ENV}={} {CASES_ENV}={}",
        base.get(),
        index.saturating_add(1)
    )
}

/// Cases to run for one property: the CI-sized default unless `BOWLINE_SIM_CASES`
/// asks for more.
fn case_count(default: u32) -> u32 {
    std::env::var(CASES_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|count| *count > 0)
        .unwrap_or(default)
}
