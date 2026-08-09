use std::collections::BTreeSet;

use super::error::ValidationError;
use super::model::{
    AppRegistry, IoPriorityClass, RegisteredApplication, CURRENT_SCHEMA_VERSION,
    MAX_REGISTERED_APPLICATIONS,
};

const MAX_DELAY_SECONDS: u64 = 3_600;
const MAX_NAME_LENGTH: usize = 4_096;

/// Validate a fully decoded registry independently from TOML deserialization.
///
/// The first stable, field-specific validation error is returned. The store
/// normalizes order before validation, so accepted registries always have
/// deterministic application ordering.
pub fn validate_registry(registry: &AppRegistry) -> Result<(), ValidationError> {
    if registry.schema_version != CURRENT_SCHEMA_VERSION {
        return Err(error(
            "schema_version",
            format!(
                "must be supported version {CURRENT_SCHEMA_VERSION}, found {}",
                registry.schema_version
            ),
        ));
    }
    if registry.apps.len() > MAX_REGISTERED_APPLICATIONS {
        return Err(error(
            "apps",
            format!("contains more than {MAX_REGISTERED_APPLICATIONS} entries"),
        ));
    }
    validate_policy(
        "launcher",
        registry.launcher.default_delay_seconds,
        registry.launcher.default_nice,
        registry.launcher.default_io_class,
        registry.launcher.default_io_priority,
    )?;

    let mut ids = BTreeSet::new();
    for (index, app) in registry.apps.iter().enumerate() {
        validate_application(index, app)?;
        if !ids.insert(&app.desktop_id) {
            return Err(error(
                format!("apps[{index}].desktop_id"),
                format!("duplicates desktop ID `{}`", app.desktop_id),
            ));
        }
    }
    Ok(())
}

fn validate_application(index: usize, app: &RegisteredApplication) -> Result<(), ValidationError> {
    let prefix = format!("apps[{index}]");
    validate_desktop_id(&format!("{prefix}.desktop_id"), &app.desktop_id)?;
    if app.name.trim().is_empty() {
        return Err(error(format!("{prefix}.name"), "must not be empty"));
    }
    if app.name.len() > MAX_NAME_LENGTH {
        return Err(error(
            format!("{prefix}.name"),
            format!("must not exceed {MAX_NAME_LENGTH} bytes"),
        ));
    }
    validate_policy(
        &prefix,
        app.delay_seconds,
        app.nice,
        app.io_class,
        app.io_priority,
    )
}

fn validate_desktop_id(field: &str, desktop_id: &str) -> Result<(), ValidationError> {
    if desktop_id.is_empty() || desktop_id.trim() != desktop_id {
        return Err(error(
            field,
            "must be non-empty and contain no surrounding whitespace",
        ));
    }
    if desktop_id == "." || desktop_id == ".." || desktop_id.contains(['/', '\\', '\0']) {
        return Err(error(
            field,
            "must be a stable desktop ID, not a filesystem path or traversal string",
        ));
    }
    Ok(())
}

fn validate_policy(
    prefix: &str,
    delay_seconds: u64,
    nice: i8,
    io_class: IoPriorityClass,
    io_priority: Option<u8>,
) -> Result<(), ValidationError> {
    if delay_seconds > MAX_DELAY_SECONDS {
        return Err(error(
            format!("{prefix}.delay_seconds"),
            format!("must be in 0..={MAX_DELAY_SECONDS}"),
        ));
    }
    if !(-20..=19).contains(&nice) {
        return Err(error(
            format!("{prefix}.nice"),
            "must be in Linux range -20..=19; negative values may require privileges at launch",
        ));
    }
    match (io_class, io_priority) {
        (IoPriorityClass::BestEffort | IoPriorityClass::Realtime, Some(priority))
            if priority <= 7 =>
        {
            Ok(())
        }
        (IoPriorityClass::BestEffort | IoPriorityClass::Realtime, Some(_)) => Err(error(
            format!("{prefix}.io_priority"),
            "must be in 0..=7 for best-effort or realtime I/O class",
        )),
        (IoPriorityClass::BestEffort | IoPriorityClass::Realtime, None) => Err(error(
            format!("{prefix}.io_priority"),
            "is required for best-effort or realtime I/O class",
        )),
        (IoPriorityClass::None | IoPriorityClass::Idle, None) => Ok(()),
        (IoPriorityClass::None | IoPriorityClass::Idle, Some(_)) => Err(error(
            format!("{prefix}.io_priority"),
            "must be omitted for none or idle I/O class",
        )),
    }
}

fn error(field: impl Into<String>, message: impl Into<String>) -> ValidationError {
    ValidationError {
        field: field.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::validate_registry;
    use crate::registry::{AppRegistry, IoPriorityClass, RegisteredApplication};

    #[test]
    fn validates_default_registry_and_rejects_invalid_policy() {
        assert!(validate_registry(&AppRegistry::default()).is_ok());
        let mut registry = AppRegistry::default();
        registry.apps.push(RegisteredApplication {
            desktop_id: "example.desktop".to_owned(),
            name: "Example".to_owned(),
            io_class: IoPriorityClass::Idle,
            io_priority: Some(0),
            ..RegisteredApplication::default()
        });
        assert!(validate_registry(&registry)
            .expect_err("idle priority must be rejected")
            .field
            .ends_with("io_priority"));
    }
}
