pub mod infer;
pub mod local_state;
pub mod readiness;
pub mod recipe;
pub mod redact;
pub mod runner;

pub use infer::{
    SetupBlocker, SetupCommandPlan, SetupInferenceError, SetupInferenceSource, SetupPlan,
    infer_setup_plan,
};
pub use local_state::{
    FileIdentity, LocalRegenerateKind, LocalRegenerateOutput, PackageManagerIdentity,
    SetupReceiptIdentityInputs, collect_receipt_identity_inputs,
};
pub use readiness::{
    SetupIdentity, SetupReadinessClassification, SetupReadinessState,
    classify_setup_command_result, collect_setup_identity, inferred_receipt_key,
    inferred_recipe_hash, recipe_receipt_key, setup_identity_hash, setup_receipt_id,
};
pub use recipe::{
    SetupRecipe, SetupRecipeCommand, SetupRecipeError, load_setup_recipe, parse_setup_recipe,
    validate_setup_cwd,
};
pub use redact::{RedactedSetupText, redact_setup_text, redact_setup_text_with_values};
pub use runner::{
    LearnedSetupCandidate, ProjectSetupOptions, ProjectSetupOutcome, ProjectSetupState,
    SetupRunError, learned_setup_candidate, run_project_setup,
};
