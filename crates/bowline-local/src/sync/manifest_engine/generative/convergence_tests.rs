//! The CanopyCheck analogue: generated three-way cases must converge.
//!
//! Each case generates one base tree, perturbs it independently on two devices,
//! and drives both real engines against one shared remote until a fixpoint. The
//! three assertions are termination, agreement, and durability of authored
//! bytes; a failure is minimized before it is reported, and always prints the
//! environment that replays it.

use super::rng::{Rng, Seed};
use super::scenario::{Scenario, run};
use super::shrink::{minimize, report};
use super::tree::{TreeSpec, generate_mutations};
use super::{base_seed, case_count, replay_hint};

/// Cases per property in CI. Each case builds two real temp workspaces, two
/// SQLite stores, and several full sync rounds, so the default is sized to keep
/// the suite in the seconds range; `BOWLINE_SIM_CASES` cranks it up locally.
const CI_CASES: u32 = 12;
const CI_CONFLICT_CASES: u32 = 12;

#[test]
fn generated_three_way_cases_converge() {
    check_property("gen-converge", CI_CASES, independent_scenario);
}

/// The same property with the conflict density turned up: both devices are
/// forced to mutate paths drawn from the SAME small set, so nearly every case
/// lands on a genuine (Changed, Changed) / (Deleted, Changed) / (Untracked,
/// Created) row rather than on disjoint subtrees.
#[test]
fn concurrent_edits_to_shared_paths_converge() {
    check_property("gen-conflict", CI_CONFLICT_CASES, colliding_scenario);
}

fn check_property(label: &str, cases: u32, generate: fn(&mut Rng) -> Scenario) {
    let base = base_seed();
    for index in 0..case_count(cases) {
        let seed = base.case(index);
        let scenario = generate(&mut Rng::from_seed(seed));
        let case_label = format!("{label}-{}", seed.get());
        if let Err(violation) = run(&scenario, &case_label) {
            let minimized = minimize(scenario, violation, &case_label);
            panic!("{}", report(&replay_hint(base, index), &minimized));
        }
    }
}

fn independent_scenario(rng: &mut Rng) -> Scenario {
    let base = TreeSpec::generate(rng, 6);
    let local_count = rng.in_range(1, 4);
    let remote_count = rng.in_range(1, 4);
    Scenario {
        local: generate_mutations(rng, &base, local_count),
        remote: generate_mutations(rng, &base, remote_count),
        base,
    }
}

fn colliding_scenario(rng: &mut Rng) -> Scenario {
    // A two-file base keeps the path space tiny, which is what forces both
    // mutation lists onto the same paths.
    let base = TreeSpec::generate(rng, 2);
    let local_count = rng.in_range(1, 3);
    let remote_count = rng.in_range(1, 3);
    Scenario {
        local: generate_mutations(rng, &base, local_count),
        remote: generate_mutations(rng, &base, remote_count),
        base,
    }
}

/// A hand-built regression of the property's own machinery: the empty scenario
/// (no perturbation at all) must converge in one round, proving the harness
/// itself does not manufacture work.
#[test]
fn an_unperturbed_pair_converges_immediately() {
    let mut rng = Rng::from_seed(Seed::new(1));
    let scenario = Scenario {
        base: TreeSpec::generate(&mut rng, 4),
        local: Vec::new(),
        remote: Vec::new(),
    };

    if let Err(violation) = run(&scenario, "gen-converge-identity") {
        panic!("an unperturbed pair must converge: {violation:?}");
    }
}
