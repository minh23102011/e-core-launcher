//! User-controlled, versioned TOML application registry.
//!
//! Registry entries are explicit consent records. Loading and listing never
//! make a discovered application managed, and this module never launches or
//! modifies a process.

mod error;
mod model;
mod operations;
mod path;
mod store;
mod validation;

pub use error::{RegistryError, ValidationError};
pub use model::{
    AppRegistry, ApplicationSettingsUpdate, IoPriorityClass, LauncherDefaults,
    RegisteredApplication, CURRENT_SCHEMA_VERSION, MAX_REGISTERED_APPLICATIONS,
};
pub use operations::{
    AddApplicationsResult, RegisteredApplicationAvailability, RegisteredApplicationStatus,
    RegistryMutationResult,
};
pub use path::resolve_config_path;
pub use store::{RegistryLoad, RegistryStore};
pub use validation::validate_registry;
