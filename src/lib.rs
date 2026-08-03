//! Reusable Linux topology detection, desktop discovery, and explicit registry APIs.
//!
//! The crate is intentionally read-only. It inspects sysfs and desktop-entry
//! files, and does not launch processes or change CPU affinity.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

#[cfg(not(target_os = "linux"))]
compile_error!("ecore-launcher supports Linux only");

pub mod discovery;
pub mod registry;
pub mod topology;

pub use discovery::{
    DesktopApplicationScanner, DesktopEntryParseError, DiscoveredApplication, DiscoveryError,
    DiscoveryOptions, DiscoveryReport, DiscoveryWarning, DiscoveryWarningCategory,
    DiscoveryWarningSeverity, ExecParseError, ExecutableResolutionError, ExecutableResolver,
    ParsedExec,
};
pub use registry::{
    resolve_config_path, validate_registry, AddApplicationsResult, AppRegistry,
    ApplicationSettingsUpdate, IoPriorityClass, LauncherDefaults, RegisteredApplication,
    RegisteredApplicationAvailability, RegisteredApplicationStatus, RegistryError, RegistryLoad,
    RegistryMutationResult, RegistryStore, ValidationError, CURRENT_SCHEMA_VERSION,
    MAX_REGISTERED_APPLICATIONS,
};
pub use topology::{
    CoreClass, CpuTopology, CpuTopologyDetector, DetectionEvidence, DetectorError, EvidenceKind,
    EvidenceSource, EvidenceStrength, PhysicalCore, TopologyClass,
};
