pub const DEFAULT_CONVEX_URL: &str = "https://adamant-dog-830.eu-west-1.convex.cloud";
pub const DEFAULT_WORKOS_CLIENT_ID: &str = "client_01KW7J684RKNG2SAE09GPD9MRV";

// The hosted control plane bootstraps an untouched workspace with this snapshot
// ref ID. Single owner for the sentinel so the daemon, local sync runner, and
// control-plane client compare against one value instead of drifting copies.
pub const EMPTY_SNAPSHOT_ID: &str = "empty";
