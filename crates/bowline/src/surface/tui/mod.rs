pub mod app;
pub mod input;
pub mod model;
pub mod render;
pub mod terminal;

pub use app::{run_app, run_onboarding_app};
pub use model::{OnboardingModel, TuiModel};
