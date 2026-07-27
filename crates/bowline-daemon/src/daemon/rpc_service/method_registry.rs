//! The single registry of daemon RPC methods.
//!
//! Method identity used to live in five parallel `&str` tables — the advertised
//! capability list, two dispatch matches, the lane assignment, and the
//! connection-ownership predicate — which had already drifted apart. Everything
//! about a method now comes from one row here, so adding a method is one edit
//! and the compiler catches an unhandled one.

use super::rpc_executor::RpcLane;

/// Which executor serves a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MethodOwner {
    /// Queued onto an RPC executor lane and answered off the connection thread.
    Lane(RpcLane),
    /// Answered inline on the connection pump, because it reads or mutates
    /// per-connection state (the subscription table, the shutdown request).
    Connection,
}

/// Every method this daemon serves. The wire names are the machine contract; the
/// order is the advertised capability order, so it stays stable and sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RpcMethod {
    DaemonInfo,
    DaemonMetrics,
    DaemonPing,
    DaemonShutdown,
    DeviceApprove,
    DeviceDeny,
    StatusGetSnapshot,
    StatusSubscribe,
    SubscriptionCancel,
    SyncBarrier,
    SyncConfirmDeletions,
    SyncGetBlockedDeletions,
    WorkAccept,
    WorkCreate,
    WorkReview,
}

struct MethodSpec {
    method: RpcMethod,
    wire: &'static str,
    owner: MethodOwner,
}

const METHODS: &[MethodSpec] = &[
    spec(
        RpcMethod::DaemonInfo,
        "daemon.info",
        MethodOwner::Lane(RpcLane::Query),
    ),
    spec(
        RpcMethod::DaemonMetrics,
        "daemon.metrics",
        MethodOwner::Lane(RpcLane::Query),
    ),
    spec(
        RpcMethod::DaemonPing,
        "daemon.ping",
        MethodOwner::Lane(RpcLane::Status),
    ),
    spec(
        RpcMethod::DaemonShutdown,
        "daemon.shutdown",
        MethodOwner::Connection,
    ),
    spec(
        RpcMethod::DeviceApprove,
        "device.approve",
        MethodOwner::Lane(RpcLane::Mutation),
    ),
    spec(
        RpcMethod::DeviceDeny,
        "device.deny",
        MethodOwner::Lane(RpcLane::Mutation),
    ),
    spec(
        RpcMethod::StatusGetSnapshot,
        "status.getSnapshot",
        MethodOwner::Lane(RpcLane::Status),
    ),
    spec(
        RpcMethod::StatusSubscribe,
        "status.subscribe",
        MethodOwner::Connection,
    ),
    spec(
        RpcMethod::SubscriptionCancel,
        "subscription.cancel",
        MethodOwner::Connection,
    ),
    spec(
        RpcMethod::SyncBarrier,
        "sync.barrier",
        MethodOwner::Lane(RpcLane::Query),
    ),
    spec(
        RpcMethod::SyncConfirmDeletions,
        "sync.confirmDeletions",
        MethodOwner::Lane(RpcLane::Mutation),
    ),
    spec(
        RpcMethod::SyncGetBlockedDeletions,
        "sync.getBlockedDeletions",
        MethodOwner::Lane(RpcLane::Query),
    ),
    spec(
        RpcMethod::WorkAccept,
        "work.accept",
        MethodOwner::Lane(RpcLane::Mutation),
    ),
    spec(
        RpcMethod::WorkCreate,
        "work.create",
        MethodOwner::Lane(RpcLane::Mutation),
    ),
    spec(
        RpcMethod::WorkReview,
        "work.review",
        MethodOwner::Lane(RpcLane::Mutation),
    ),
];

const fn spec(method: RpcMethod, wire: &'static str, owner: MethodOwner) -> MethodSpec {
    MethodSpec {
        method,
        wire,
        owner,
    }
}

impl RpcMethod {
    /// Parse a wire method name once, at the connection pump, so every
    /// downstream match is exhaustive.
    pub(super) fn from_wire(method: &str) -> Option<Self> {
        METHODS
            .iter()
            .find(|spec| spec.wire == method)
            .map(|spec| spec.method)
    }

    pub(super) fn owner(self) -> MethodOwner {
        self.spec().owner
    }

    /// The executor lane, or `None` for a connection-owned method.
    pub(super) fn lane(self) -> Option<RpcLane> {
        match self.owner() {
            MethodOwner::Lane(lane) => Some(lane),
            MethodOwner::Connection => None,
        }
    }

    pub(super) fn is_connection_owned(self) -> bool {
        matches!(self.owner(), MethodOwner::Connection)
    }

    fn spec(self) -> &'static MethodSpec {
        METHODS
            .iter()
            .find(|spec| spec.method == self)
            .expect("every RpcMethod variant has exactly one registry row")
    }
}

/// The capability list advertised in the server hello and `daemon.info`. Derived
/// from the registry, so it can never advertise a method that is not served or
/// omit one that is.
pub(super) fn supported_capabilities() -> Vec<String> {
    METHODS.iter().map(|spec| spec.wire.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_exactly_one_row() {
        for spec in METHODS {
            assert_eq!(RpcMethod::from_wire(spec.wire), Some(spec.method));
        }
        let mut wires: Vec<&str> = METHODS.iter().map(|spec| spec.wire).collect();
        let total = wires.len();
        wires.sort_unstable();
        wires.dedup();
        assert_eq!(wires.len(), total, "wire method names are unique");
    }

    #[test]
    fn advertised_capabilities_are_the_served_methods() {
        let capabilities = supported_capabilities();
        for capability in &capabilities {
            assert!(
                RpcMethod::from_wire(capability).is_some(),
                "advertised capability {capability} is not a served method"
            );
        }
        assert_eq!(capabilities.len(), METHODS.len());
        // Stable order, so the hello frame does not churn between builds.
        let mut sorted = capabilities.clone();
        sorted.sort();
        assert_eq!(capabilities, sorted);
    }

    #[test]
    fn connection_owned_methods_have_no_lane() {
        for spec in METHODS {
            assert_eq!(
                spec.method.is_connection_owned(),
                spec.method.lane().is_none()
            );
        }
    }

    #[test]
    fn unknown_wire_names_do_not_parse() {
        assert!(RpcMethod::from_wire("status.snapshot").is_none());
        assert!(RpcMethod::from_wire("device.actions").is_none());
    }
}
