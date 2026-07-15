#![cfg(feature = "fault-injection")]

use std::{cell::RefCell, error::Error, fmt};

thread_local! {
    static ACTIVE: RefCell<Option<FaultState>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultPoint {
    BeforeWorkViewAcceptCheckpoint,
    AfterWorkViewAcceptCheckpoint,
    AfterObjectUpload,
    AfterManifestCommit,
    AfterRefCas,
    AfterLocalHeadWrite,
    AfterStatCacheWriteBack,
    AfterMaterializationRename,
}

impl FaultPoint {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BeforeWorkViewAcceptCheckpoint => "before-work-view-accept-checkpoint",
            Self::AfterWorkViewAcceptCheckpoint => "after-work-view-accept-checkpoint",
            Self::AfterObjectUpload => "after-object-upload",
            Self::AfterManifestCommit => "after-manifest-commit",
            Self::AfterRefCas => "after-ref-cas",
            Self::AfterLocalHeadWrite => "after-local-head-write",
            Self::AfterStatCacheWriteBack => "after-stat-cache-write-back",
            Self::AfterMaterializationRename => "after-materialization-rename",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultPlan {
    point: FaultPoint,
    occurrence: usize,
}

impl FaultPlan {
    pub fn new(point: FaultPoint, occurrence: usize) -> Self {
        Self {
            point,
            occurrence: occurrence.max(1),
        }
    }

    pub fn point(self) -> FaultPoint {
        self.point
    }

    pub fn occurrence(self) -> usize {
        self.occurrence
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultError {
    point: FaultPoint,
    occurrence: usize,
}

impl FaultError {
    pub fn point(&self) -> FaultPoint {
        self.point
    }

    pub fn occurrence(&self) -> usize {
        self.occurrence
    }
}

impl fmt::Display for FaultError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "injected sync fault at {} occurrence {}",
            self.point.as_str(),
            self.occurrence
        )
    }
}

impl Error for FaultError {}

#[derive(Debug)]
struct FaultState {
    plan: FaultPlan,
    observed: usize,
}

pub struct FaultGuard;

impl Drop for FaultGuard {
    fn drop(&mut self) {
        ACTIVE.with(|active| *active.borrow_mut() = None);
    }
}

pub fn arm(plan: FaultPlan) -> FaultGuard {
    ACTIVE.with(|active| *active.borrow_mut() = Some(FaultState { plan, observed: 0 }));
    FaultGuard
}

pub fn trip(point: FaultPoint) -> Result<(), FaultError> {
    ACTIVE.with(|active| {
        let mut active = active.borrow_mut();
        let Some(state) = active.as_mut() else {
            return Ok(());
        };
        if state.plan.point != point {
            return Ok(());
        }
        state.observed += 1;
        if state.observed != state.plan.occurrence {
            return Ok(());
        }
        let error = FaultError {
            point,
            occurrence: state.observed,
        };
        *active = None;
        Err(error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn armed_fault_trips_on_selected_occurrence() {
        let _guard = arm(FaultPlan::new(FaultPoint::AfterObjectUpload, 2));

        assert!(trip(FaultPoint::AfterObjectUpload).is_ok());
        let error = trip(FaultPoint::AfterObjectUpload).expect_err("second occurrence trips");

        assert_eq!(error.point(), FaultPoint::AfterObjectUpload);
        assert_eq!(error.occurrence(), 2);
        assert!(trip(FaultPoint::AfterObjectUpload).is_ok());
    }

    #[test]
    fn guard_disarms_without_trip() {
        {
            let _guard = arm(FaultPlan::new(FaultPoint::AfterRefCas, 1));
        }

        assert!(trip(FaultPoint::AfterRefCas).is_ok());
    }

    #[test]
    fn armed_faults_are_thread_local() {
        let first = thread::spawn(|| {
            let _guard = arm(FaultPlan::new(FaultPoint::AfterObjectUpload, 1));
            let error = trip(FaultPoint::AfterObjectUpload).expect_err("first thread trips");
            assert_eq!(error.point(), FaultPoint::AfterObjectUpload);
            assert_eq!(error.occurrence(), 1);
            assert!(trip(FaultPoint::AfterManifestCommit).is_ok());
        });
        let second = thread::spawn(|| {
            let _guard = arm(FaultPlan::new(FaultPoint::AfterManifestCommit, 1));
            let error = trip(FaultPoint::AfterManifestCommit).expect_err("second thread trips");
            assert_eq!(error.point(), FaultPoint::AfterManifestCommit);
            assert_eq!(error.occurrence(), 1);
            assert!(trip(FaultPoint::AfterObjectUpload).is_ok());
        });

        first.join().expect("first fault thread");
        second.join().expect("second fault thread");
    }

    #[test]
    fn work_view_accept_checkpoint_faults_cover_both_commit_sides() {
        for point in [
            FaultPoint::BeforeWorkViewAcceptCheckpoint,
            FaultPoint::AfterWorkViewAcceptCheckpoint,
        ] {
            let _guard = arm(FaultPlan::new(point, 1));
            let error = trip(point).expect_err("accept checkpoint fault");
            assert_eq!(error.point(), point);
        }
    }
}
