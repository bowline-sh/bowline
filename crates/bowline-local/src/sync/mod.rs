//! Sync engine.
//!
//! After the Plan 111 cutover the only sync engine is the manifest engine; the
//! paged-namespace/convergence engine and all its glue were deleted. Path
//! predicates the engine needs (`is_private_workspace_state_path`,
//! `is_secret_bearing_path`) moved to [`crate::policy`], the single owner of
//! what syncs.

pub mod manifest_engine;
