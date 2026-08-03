use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::discovery::{DiscoveredApplication, DiscoveryReport};

use super::error::{RegistryError, ValidationError};
use super::model::{
    normalize_priority, AppRegistry, ApplicationSettingsUpdate, RegisteredApplication,
};
use super::validation::validate_registry;

/// Outcome of adding discovered applications to the registry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AddApplicationsResult {
    /// IDs newly added to the explicit registry.
    pub added: Vec<String>,
    /// IDs already selected before this operation.
    pub already_registered: Vec<String>,
}

/// Outcome of a state-changing multi-application operation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RegistryMutationResult {
    /// IDs whose stored state changed.
    pub changed: Vec<String>,
    /// IDs which already had the requested state.
    pub unchanged: Vec<String>,
}

/// Current discovery status for one explicit registry entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegisteredApplicationAvailability {
    /// The stable desktop ID is currently discoverable.
    Available {
        /// Current display name from desktop-entry discovery.
        current_name: String,
    },
    /// No current discovery result has this stable desktop ID.
    Unavailable,
    /// Availability was not checked for this report.
    Unknown,
}

/// Stored application data paired with current discovery availability.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegisteredApplicationStatus {
    /// Explicitly stored registry entry.
    pub application: RegisteredApplication,
    /// Current discovery result, without mutating the registry.
    pub availability: RegisteredApplicationAvailability,
}

impl AppRegistry {
    /// Add explicitly selected discovery results using a snapshot of current defaults.
    ///
    /// Existing IDs are retained unchanged. The resulting registry is sorted
    /// and fully validated before this method succeeds.
    pub fn add_discovered(
        &mut self,
        applications: &[DiscoveredApplication],
    ) -> Result<AddApplicationsResult, RegistryError> {
        let previous = self.clone();
        let mut selected = BTreeMap::new();
        for application in applications {
            selected
                .entry(application.desktop_id.clone())
                .or_insert(application);
        }

        let existing: BTreeSet<String> = self
            .apps
            .iter()
            .map(|application| application.desktop_id.clone())
            .collect();
        let mut result = AddApplicationsResult::default();
        for (desktop_id, application) in selected {
            if existing.contains(&desktop_id) {
                result.already_registered.push(desktop_id);
                continue;
            }
            self.apps.push(RegisteredApplication {
                desktop_id: application.desktop_id.clone(),
                name: application.name.clone(),
                enabled: true,
                delay_seconds: self.launcher.default_delay_seconds,
                nice: self.launcher.default_nice,
                io_class: self.launcher.default_io_class,
                io_priority: self.launcher.default_io_priority,
                enforce_process_tree: self.launcher.default_enforce_process_tree,
                desktop_file: Some(application.desktop_file.clone()),
                extra: BTreeMap::new(),
            });
            result.added.push(desktop_id);
        }
        self.normalize();
        if let Err(error) = validate_registry(self) {
            *self = previous;
            return Err(error.into());
        }
        Ok(result)
    }

    /// Remove explicitly named registered IDs as one validated operation.
    pub fn remove(
        &mut self,
        desktop_ids: &[String],
    ) -> Result<RegistryMutationResult, RegistryError> {
        ensure_registered_ids(self, desktop_ids)?;
        let previous = self.clone();
        let selected: BTreeSet<&str> = desktop_ids.iter().map(String::as_str).collect();
        self.apps
            .retain(|application| !selected.contains(application.desktop_id.as_str()));
        self.normalize();
        if let Err(error) = validate_registry(self) {
            *self = previous;
            return Err(error.into());
        }
        Ok(RegistryMutationResult {
            changed: selected.into_iter().map(str::to_owned).collect(),
            unchanged: Vec::new(),
        })
    }

    /// Set the enabled flag for explicitly named applications atomically.
    pub fn set_enabled(
        &mut self,
        desktop_ids: &[String],
        enabled: bool,
    ) -> Result<RegistryMutationResult, RegistryError> {
        ensure_registered_ids(self, desktop_ids)?;
        let previous = self.clone();
        let selected: BTreeSet<&str> = desktop_ids.iter().map(String::as_str).collect();
        let mut result = RegistryMutationResult::default();
        for application in &mut self.apps {
            if selected.contains(application.desktop_id.as_str()) {
                if application.enabled == enabled {
                    result.unchanged.push(application.desktop_id.clone());
                } else {
                    application.enabled = enabled;
                    result.changed.push(application.desktop_id.clone());
                }
            }
        }
        self.normalize();
        if let Err(error) = validate_registry(self) {
            *self = previous;
            return Err(error.into());
        }
        Ok(result)
    }

    /// Update supplied settings for one application, or restore its snapshot
    /// settings from current launcher defaults when `reset_to_defaults` is set.
    pub fn configure(
        &mut self,
        desktop_id: &str,
        update: &ApplicationSettingsUpdate,
    ) -> Result<bool, RegistryError> {
        if update.reset_to_defaults
            && (update.delay_seconds.is_some()
                || update.nice.is_some()
                || update.io_class.is_some()
                || update.io_priority.is_some()
                || update.enforce_process_tree.is_some())
        {
            return Err(ValidationError {
                field: "configure".to_owned(),
                message: "--reset cannot be combined with individual settings".to_owned(),
            }
            .into());
        }
        let previous = self.clone();
        let defaults = self.launcher.clone();
        let changed = {
            let application = self
                .apps
                .iter_mut()
                .find(|application| application.desktop_id == desktop_id)
                .ok_or_else(|| RegistryError::UnknownRegisteredApplication {
                    desktop_id: desktop_id.to_owned(),
                })?;
            let before = application.clone();
            if update.reset_to_defaults {
                application.delay_seconds = defaults.default_delay_seconds;
                application.nice = defaults.default_nice;
                application.io_class = defaults.default_io_class;
                application.io_priority = defaults.default_io_priority;
                application.enforce_process_tree = defaults.default_enforce_process_tree;
            } else {
                if let Some(value) = update.delay_seconds {
                    application.delay_seconds = value;
                }
                if let Some(value) = update.nice {
                    application.nice = value;
                }
                if let Some(value) = update.io_class {
                    application.io_class = value;
                }
                if let Some(value) = update.io_priority {
                    application.io_priority = value;
                }
                if let Some(value) = update.enforce_process_tree {
                    application.enforce_process_tree = value;
                }
                normalize_priority(&mut application.io_priority, application.io_class);
            }
            *application != before
        };
        self.normalize();
        if let Err(error) = validate_registry(self) {
            *self = previous;
            return Err(error.into());
        }
        Ok(changed)
    }

    /// Pair all explicit registry entries with a current discovery report.
    ///
    /// Entries absent from discovery are returned as unavailable and are never
    /// removed or changed.
    #[must_use]
    pub fn resolve_against(
        &self,
        discovery: Option<&DiscoveryReport>,
    ) -> Vec<RegisteredApplicationStatus> {
        let discovered: BTreeMap<&str, &DiscoveredApplication> = discovery
            .map(|report| {
                report
                    .applications
                    .iter()
                    .map(|application| (application.desktop_id.as_str(), application))
                    .collect()
            })
            .unwrap_or_default();
        self.apps
            .iter()
            .cloned()
            .map(|application| {
                let availability = match discovery {
                    Some(_) => discovered.get(application.desktop_id.as_str()).map_or(
                        RegisteredApplicationAvailability::Unavailable,
                        |current| RegisteredApplicationAvailability::Available {
                            current_name: current.name.clone(),
                        },
                    ),
                    None => RegisteredApplicationAvailability::Unknown,
                };
                RegisteredApplicationStatus {
                    application,
                    availability,
                }
            })
            .collect()
    }
}

fn ensure_registered_ids(
    registry: &AppRegistry,
    desktop_ids: &[String],
) -> Result<(), RegistryError> {
    let registered: BTreeSet<&str> = registry
        .apps
        .iter()
        .map(|application| application.desktop_id.as_str())
        .collect();
    for desktop_id in desktop_ids {
        if !registered.contains(desktop_id.as_str()) {
            return Err(RegistryError::UnknownRegisteredApplication {
                desktop_id: desktop_id.clone(),
            });
        }
    }
    Ok(())
}
