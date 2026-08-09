#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdAllocation {
    Available(u64),
    Terminal(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct IdSequence {
    pub(super) next: u64,
}

impl IdSequence {
    pub(super) const fn initial() -> Self {
        Self { next: 1 }
    }

    pub(super) fn allocate(&mut self) -> IdAllocation {
        if self.next == u64::MAX {
            return IdAllocation::Terminal(self.next);
        }
        let value = self.next;
        self.next += 1;
        IdAllocation::Available(value)
    }
}
