//! Reusable Linux topology, desktop discovery, explicit registry, fail-closed
//! launch, verified-tree supervision, user startup, and diagnostic APIs.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

#[cfg(not(target_os = "linux"))]
compile_error!("ecore-launcher supports Linux only");

pub mod discovery;
pub mod doctor;
pub mod integration;
pub mod launcher;
pub mod registry;
pub mod supervisor;
pub mod topology;

pub use discovery::{
    DesktopApplicationScanner, DesktopEntryParseError, DiscoveredApplication, DiscoveryError,
    DiscoveryOptions, DiscoveryReport, DiscoveryWarning, DiscoveryWarningCategory,
    DiscoveryWarningSeverity, ExecParseError, ExecutableResolutionError, ExecutableResolver,
    ParsedExec,
};
pub use doctor::{
    diagnose, diagnose_with_runner, DoctorCheck, DoctorOptions, DoctorReport, DoctorStatus,
    SessionEnvironment,
};
pub use integration::{
    assess_autostart, AutostartAssessment, AutostartState, CommandResult, CommandRunner,
    DirectCommandRunner, IntegrationError, IntegrationPaths, ManagerEnvironmentStatus,
    StartupChange, StartupManager, StartupStatus, USER_UNIT_NAME,
};
pub use launcher::{
    build_launch_plan, exec_with_affinity, execute_plan, execute_plan_with_options,
    launch_schedule, run_exec_helper, ExecutionOptions, InitiatedApplication, LaunchFailure,
    LaunchFailureStage, LaunchPlan, LaunchReport, LauncherError, PlannedApplication,
    ScheduledLaunch,
};
pub use registry::{
    resolve_config_path, validate_registry, AddApplicationsResult, AppRegistry,
    ApplicationSettingsUpdate, IoPriorityClass, LauncherDefaults, RegisteredApplication,
    RegisteredApplicationAvailability, RegisteredApplicationStatus, RegistryError, RegistryLoad,
    RegistryMutationResult, RegistryStore, ValidationError, CURRENT_SCHEMA_VERSION,
    MAX_REGISTERED_APPLICATIONS,
};
pub use supervisor::{
    supervise_process_trees, SupervisionReport, SupervisorError, SupervisorOptions,
    SupervisorWarning, DEFAULT_SUPERVISOR_POLL_INTERVAL,
};
pub use topology::{
    CoreClass, CpuTopology, CpuTopologyDetector, DetectionEvidence, DetectorError, EvidenceKind,
    EvidenceSource, EvidenceStrength, PhysicalCore, TopologyClass,
};
