//! One replayable convergence case and the three properties it checks.
//!
//! A scenario is a value: a base tree plus two independent mutation lists. That
//! is what makes it shrinkable — [`super::shrink`] re-runs modified copies of
//! the same value until it finds the smallest one that still fails.
//!
//! The properties are Unison's correctness statement restated as assertions:
//! the reconciler terminates, both replicas agree at the fixpoint, and no bytes
//! a device authored are destroyed on the way there (relocation into a
//! conflict-aside is allowed; silent loss is not).

use std::collections::BTreeSet;
use std::fmt;

use super::super::engine_test_support::FakeRemote;
use super::super::manifest::WorkspacePath;
use super::fleet::{Device, FleetError};
use super::tree::{Mutation, TreeSpec};

/// Round-trips a scenario may take to reach a fixpoint. Each round is one full
/// settle on each device, so a healthy conflict (aside + re-push + peer pull)
/// costs two or three rounds; anything beyond this bound is a livelock.
pub(crate) const MAX_ROUNDS: u32 = 8;

/// A generated three-way case: `base` is the shared ancestor both devices start
/// from, `local` perturbs the first device and `remote` the second.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Scenario {
    pub(crate) base: TreeSpec,
    pub(crate) local: Vec<Mutation>,
    pub(crate) remote: Vec<Mutation>,
}

impl Scenario {
    pub(crate) fn size(&self) -> usize {
        self.base.len() + self.local.len() + self.remote.len()
    }
}

impl fmt::Display for Scenario {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "base:")?;
        for (path, body) in self.base.files() {
            writeln!(
                formatter,
                "  {} mode={:o} bytes={:?}",
                path.as_str(),
                body.mode.get(),
                String::from_utf8_lossy(&body.bytes)
            )?;
        }
        writeln!(formatter, "local mutations: {:?}", self.local)?;
        write!(formatter, "remote mutations: {:?}", self.remote)
    }
}

/// The classes of failure a scenario can produce. The shrinker only accepts a
/// smaller candidate when its kind matches the original, so minimization never
/// silently swaps one bug for another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViolationKind {
    /// The devices never reached a fixpoint within [`MAX_ROUNDS`].
    Termination,
    /// The fixpoint is not shared: the two workspaces differ.
    Divergence,
    /// Content a device authored is absent from that device's final workspace,
    /// at its own path and at every conflict-aside path.
    LostBytes,
    /// The engine or the harness itself failed the run.
    Fault,
}

#[derive(Debug, Clone)]
pub(crate) struct Violation {
    pub(crate) kind: ViolationKind,
    pub(crate) detail: String,
}

impl Violation {
    fn new(kind: ViolationKind, detail: String) -> Self {
        Self { kind, detail }
    }
}

impl From<FleetError> for Violation {
    fn from(error: FleetError) -> Self {
        Self::new(ViolationKind::Fault, error.to_string())
    }
}

/// Run one scenario to a fixpoint and check all three properties.
pub(crate) fn run(scenario: &Scenario, label: &str) -> Result<(), Violation> {
    let remote_store = FakeRemote::new();
    let mut first = Device::new(&format!("{label}-a"), "device-a")?;
    let mut second = Device::new(&format!("{label}-b"), "device-b")?;

    scenario
        .base
        .materialize(first.root())
        .map_err(|error| Violation::new(ViolationKind::Fault, error.to_string()))?;
    first.start(&remote_store)?;
    first.settle(&remote_store)?;
    second.start(&remote_store)?;
    second.settle(&remote_store)?;

    // The shared starting point both devices perturb. Read from disk rather
    // than assumed from `scenario.base`, so a setup that failed to propagate is
    // reported here instead of poisoning the properties below.
    let baseline = first.files()?;
    if baseline != second.files()? {
        return Err(Violation::new(
            ViolationKind::Divergence,
            describe_divergence(&baseline, &second.files()?, 0),
        ));
    }

    apply_all(&scenario.local, &mut first)?;
    apply_all(&scenario.remote, &mut second)?;

    let first_authored = authored_bodies(&baseline, &first.files()?, &scenario.local);
    let second_authored = authored_bodies(&baseline, &second.files()?, &scenario.remote);

    let rounds = converge(&mut first, &mut second, &remote_store)?;
    let Some(rounds) = rounds else {
        return Err(Violation::new(
            ViolationKind::Termination,
            format!("no fixpoint after {MAX_ROUNDS} rounds"),
        ));
    };

    let final_first = first.files()?;
    let final_second = second.files()?;
    if final_first != final_second {
        return Err(Violation::new(
            ViolationKind::Divergence,
            describe_divergence(&final_first, &final_second, rounds),
        ));
    }
    check_durability(first.device_id(), &first_authored, &final_first)?;
    check_durability(second.device_id(), &second_authored, &final_second)
}

fn apply_all(mutations: &[Mutation], device: &mut Device) -> Result<(), Violation> {
    for mutation in mutations {
        mutation
            .apply_to_disk(device.root())
            .map_err(|error| Violation::new(ViolationKind::Fault, error.to_string()))?;
    }
    Ok(())
}

/// Settle both devices repeatedly until a round changes neither workspace and
/// the two agree. Returns the round count, or `None` if the bound was hit.
fn converge(
    first: &mut Device,
    second: &mut Device,
    remote_store: &FakeRemote,
) -> Result<Option<u32>, Violation> {
    for round in 0..MAX_ROUNDS {
        let before_first = first.files()?;
        let before_second = second.files()?;
        first.settle(remote_store)?;
        second.settle(remote_store)?;
        let after_first = first.files()?;
        let after_second = second.files()?;
        if after_first == after_second
            && after_first == before_first
            && after_second == before_second
        {
            return Ok(Some(round + 1));
        }
    }
    Ok(None)
}

/// The byte strings a device authored: content its own writes left on disk that
/// actually differs from the shared baseline.
///
/// Only genuinely new content is protected, and only against loss. A path this
/// device merely chmod'd, did not touch, or rewrote with identical bytes may
/// legitimately lose those bytes by adopting a peer's deletion — that is the
/// `(Unchanged, Absent)` row doing its job. Content this device actually wrote,
/// by contrast, is never destroyed: every remote delta it can meet either keeps
/// it in place or relocates the peer's version into a conflict-aside.
fn authored_bodies(
    baseline: &TreeSpec,
    after_mutations: &TreeSpec,
    mutations: &[Mutation],
) -> BTreeSet<Vec<u8>> {
    let written: BTreeSet<&WorkspacePath> = mutations
        .iter()
        .filter_map(|mutation| match mutation {
            Mutation::Write { path, .. } => Some(path),
            // A planted FIFO authors no bytes, so it contributes nothing to the
            // durability property; it is here to prove the OTHER two properties
            // still hold while the engine is meeting an object it cannot sync.
            Mutation::Remove { .. } | Mutation::Chmod { .. } | Mutation::PlantUnsyncable { .. } => {
                None
            }
        })
        .collect();
    after_mutations
        .files()
        .iter()
        .filter(|(path, body)| {
            // A write that reproduces the baseline bytes authored nothing: the
            // device is still in agreement with the ancestor, so a peer's change
            // to that path is the only change and legitimately wins.
            written.contains(path)
                && baseline.files().get(*path).map(|base| &base.bytes) != Some(&body.bytes)
        })
        .map(|(_, body)| body.bytes.clone())
        .collect()
}

fn check_durability(
    device_id: &str,
    authored: &BTreeSet<Vec<u8>>,
    final_tree: &TreeSpec,
) -> Result<(), Violation> {
    let surviving = final_tree.bodies();
    for body in authored {
        if !surviving.contains(body) {
            return Err(Violation::new(
                ViolationKind::LostBytes,
                format!(
                    "{device_id} authored {:?} but it survives at no path (not even an aside)",
                    String::from_utf8_lossy(body)
                ),
            ));
        }
    }
    Ok(())
}

fn describe_divergence(first: &TreeSpec, second: &TreeSpec, rounds: u32) -> String {
    let mut lines = vec![format!("fixpoint at round {rounds} is not shared")];
    let mut paths: BTreeSet<&WorkspacePath> = first.files().keys().collect();
    paths.extend(second.files().keys());
    for path in paths {
        let left = first.files().get(path);
        let right = second.files().get(path);
        if left != right {
            lines.push(format!(
                "  {}: a={} b={}",
                path.as_str(),
                render(left),
                render(right)
            ));
        }
    }
    lines.join("\n")
}

fn render(body: Option<&super::tree::FileBody>) -> String {
    match body {
        None => "<absent>".to_string(),
        Some(body) => format!(
            "mode={:o} bytes={:?}",
            body.mode.get(),
            String::from_utf8_lossy(&body.bytes)
        ),
    }
}
