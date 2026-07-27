//! The deterministic generator seed and PRNG the generative engine tests share.
//!
//! Reproducibility is the entire value of a generative suite: a failure is only
//! useful if the exact case can be replayed. Every seed here therefore derives
//! from a compile-time constant (or an explicit environment override), never
//! from system entropy or the wall clock, so a CI failure replays byte-for-byte
//! on a developer machine from the seed printed in the assertion message.

/// One replayable generator seed.
///
/// Seeds are derived per case from one base seed, and every generative failure
/// prints the base seed and case count that replay it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Seed(u64);

impl Seed {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    /// The seed for case `index` of a run started from this base seed.
    ///
    /// Mixed rather than added so neighbouring case indices do not produce
    /// correlated generator streams (SplitMix64 advances by a fixed odd
    /// increment, so `base + 1` would otherwise be `base`'s next state).
    pub(crate) fn case(self, index: u32) -> Self {
        Self(mix(self.0 ^ u64::from(index).wrapping_mul(GOLDEN_GAMMA)))
    }
}

/// SplitMix64's increment: the odd 64-bit fraction of the golden ratio.
const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

fn mix(value: u64) -> u64 {
    let mut z = value;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// SplitMix64. Chosen over any external PRNG crate because it is eight lines,
/// has no dependency, and its stream is stable across toolchains — a recorded
/// seed must reproduce the same case forever, which a crate upgrade could
/// otherwise silently break.
pub(crate) struct Rng {
    state: u64,
}

impl Rng {
    pub(crate) fn from_seed(seed: Seed) -> Self {
        Self { state: seed.get() }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GOLDEN_GAMMA);
        mix(self.state)
    }

    /// A uniform index into `0..bound`, or `None` when `bound` is zero.
    ///
    /// Returning `Option` rather than panicking keeps every caller total: an
    /// empty candidate set is a normal generator outcome (a tree with no files
    /// left to mutate), not a test bug.
    pub(crate) fn below(&mut self, bound: usize) -> Option<usize> {
        let bound = u64::try_from(bound).ok()?;
        if bound == 0 {
            return None;
        }
        usize::try_from(self.next_u64() % bound).ok()
    }

    /// A uniform value in `low..=high`, clamped so a reversed range still
    /// yields `low` instead of panicking.
    pub(crate) fn in_range(&mut self, low: u32, high: u32) -> u32 {
        let span = u64::from(high.saturating_sub(low)) + 1;
        let offset = u32::try_from(self.next_u64() % span).unwrap_or(0);
        low.saturating_add(offset)
    }

    /// True with probability `numerator / denominator`.
    pub(crate) fn chance(&mut self, numerator: u32, denominator: u32) -> bool {
        if denominator == 0 {
            return false;
        }
        self.next_u64() % u64::from(denominator) < u64::from(numerator)
    }

    pub(crate) fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        self.below(items.len()).and_then(|index| items.get(index))
    }
}

#[cfg(test)]
mod tests {
    use super::{Rng, Seed};

    #[test]
    fn a_seed_replays_the_same_stream() {
        let stream = |seed: Seed| {
            let mut rng = Rng::from_seed(seed);
            (0..16).filter_map(|_| rng.below(1_000)).collect::<Vec<_>>()
        };

        assert_eq!(stream(Seed::new(7)), stream(Seed::new(7)));
        assert_ne!(stream(Seed::new(7)), stream(Seed::new(8)));
    }

    #[test]
    fn neighbouring_case_seeds_do_not_share_a_stream() {
        let base = Seed::new(0x1234);
        assert_ne!(base.case(0), base.case(1));
        assert_ne!(base.case(1), base.case(2));
    }

    #[test]
    fn generators_are_total_on_empty_and_reversed_inputs() {
        let mut rng = Rng::from_seed(Seed::new(1));
        assert_eq!(rng.below(0), None);
        assert_eq!(rng.pick::<u8>(&[]), None);
        assert_eq!(rng.in_range(9, 3), 9);
        assert!(!rng.chance(1, 0));
    }
}
