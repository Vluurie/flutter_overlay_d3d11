mod resize_decision_tests;
mod view_registry_tests;
#[cfg(feature = "engine-tests")]
mod crash_report;
#[cfg(feature = "engine-tests")]
pub(crate) mod harness;
#[cfg(feature = "engine-tests")]
mod resize_integration_tests;
#[cfg(feature = "engine-tests")]
mod maximize_tests;
#[cfg(feature = "engine-tests")]
mod resize_hook_tests;
#[cfg(feature = "engine-tests")]
mod resize_race_tests;
#[cfg(feature = "engine-tests")]
mod shared_texture_race_tests;
#[cfg(feature = "engine-tests")]
mod black_window_tests;
#[cfg(feature = "engine-tests")]
mod satellite_visibility_tests;
#[cfg(feature = "engine-tests")]
mod stress_tests;
