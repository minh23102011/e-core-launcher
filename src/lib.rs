//! Reusable Linux topology detection, desktop discovery, explicit registry,
//! and fail-closed E-core launch APIs.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

#[cfg(not(target_os = "linux"))]
compile_error!("ecore-launcher supports Linux only");

pub mod discovery;
pub mod launcher;
pub mod registry;
pub mod topology;

pub use discovery::{
    DesktopApplicationScanner, DesktopEntryParseError, DiscoveredApplication, DiscoveryError,
    DiscoveryOptions, DiscoveryReport, DiscoveryWarning, DiscoveryWarningCategory,
    DiscoveryWarningSeverity, ExecParseError, ExecutableResolutionError, ExecutableResolver,
    ParsedExec,
};
pub use launcher::{
    build_launch_plan, exec_with_affinity, execute_plan, InitiatedApplication, LaunchPlan,
    LaunchReport, LauncherError, PlannedApplication,
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
