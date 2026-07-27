//! Failing-case minimization: iteratively remove nodes until nothing more can
//! go without the failure disappearing.
//!
//! A raw generated counterexample is unreadable — a dozen files and a dozen
//! mutations, of which two matter. Greedy deletion of one node at a time (a
//! base file, a local mutation, a remote mutation) reduces it to the smallest
//! case that still fails with the SAME violation kind, which is the case a human
//! can actually reason about.
//!
//! Each candidate re-runs the whole scenario against fresh workspaces, so the
//! step budget is deliberately small: minimization is a debugging aid, not a
//! second search.

use super::scenario::{Scenario, Violation, run};

/// Candidate re-runs one minimization may spend. Each is a full two-device
/// convergence run over real temp workspaces.
const MAX_SHRINK_STEPS: u32 = 48;

/// The minimized scenario and the violation it still produces.
pub(crate) struct Minimized {
    pub(crate) scenario: Scenario,
    pub(crate) violation: Violation,
}

pub(crate) fn minimize(scenario: Scenario, violation: Violation, label: &str) -> Minimized {
    let mut best = Minimized {
        scenario,
        violation,
    };
    let mut steps = 0;
    while steps < MAX_SHRINK_STEPS {
        let Some(improved) = shrink_once(&best, label, &mut steps) else {
            break;
        };
        best = improved;
    }
    best
}

/// Try every single-node deletion of `best` in turn; return the first that
/// still fails the same way.
fn shrink_once(best: &Minimized, label: &str, steps: &mut u32) -> Option<Minimized> {
    for candidate in candidates(&best.scenario) {
        if *steps >= MAX_SHRINK_STEPS {
            return None;
        }
        *steps += 1;
        if let Err(violation) = run(&candidate, label)
            && violation.kind == best.violation.kind
        {
            return Some(Minimized {
                scenario: candidate,
                violation,
            });
        }
    }
    None
}

fn candidates(scenario: &Scenario) -> Vec<Scenario> {
    let mut candidates = Vec::new();
    for path in scenario.base.paths() {
        let mut candidate = scenario.clone();
        candidate.base = scenario.base.without(&path);
        candidates.push(candidate);
    }
    for index in 0..scenario.local.len() {
        let mut candidate = scenario.clone();
        candidate.local.remove(index);
        candidates.push(candidate);
    }
    for index in 0..scenario.remote.len() {
        let mut candidate = scenario.clone();
        candidate.remote.remove(index);
        candidates.push(candidate);
    }
    candidates
}

/// Render the failure the way a developer needs to read it: the replay recipe
/// first, then the minimized case, then the violation detail.
pub(crate) fn report(replay: &str, minimized: &Minimized) -> String {
    format!(
        "generative convergence failure\n\
         replay with: {replay}\n\
         violation: {:?} — {}\n\
         minimized scenario ({} nodes):\n{}",
        minimized.violation.kind,
        minimized.violation.detail,
        minimized.scenario.size(),
        minimized.scenario,
    )
}

#[cfg(test)]
mod tests {
    use super::super::super::manifest::{FileMode, WorkspacePath};
    use super::super::rng::{Rng, Seed};
    use super::super::scenario::Scenario;
    use super::super::tree::Mutation;
    use super::super::tree::{FileBody, TreeSpec};
    use super::candidates;

    #[test]
    fn every_candidate_is_strictly_smaller() {
        let mut rng = Rng::from_seed(Seed::new(3));
        let base = TreeSpec::generate(&mut rng, 4);
        let scenario = Scenario {
            local: vec![Mutation::Remove {
                path: WorkspacePath::new("alpha/one.txt"),
            }],
            remote: vec![Mutation::Write {
                path: WorkspacePath::new("beta/two.txt"),
                body: FileBody {
                    bytes: b"x".to_vec(),
                    mode: FileMode::new(0o644),
                },
            }],
            base,
        };

        let candidates = candidates(&scenario);
        assert!(!candidates.is_empty());
        for candidate in &candidates {
            assert!(candidate.size() < scenario.size());
        }
    }
}
