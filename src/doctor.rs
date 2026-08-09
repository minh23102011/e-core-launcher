//! Deterministic, read-only diagnostics for the launcher pipeline.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use nix::sched::{sched_getaffinity, CpuSet};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};

use crate::discovery::{DesktopApplicationScanner, DiscoveryOptions};
use crate::integration::{
    AutostartState, CommandRunner, DirectCommandRunner, IntegrationPaths, StartupManager,
};
use crate::registry::{AppRegistry, IoPriorityClass, RegistryStore};
use crate::topology::{CpuTopologyDetector, TopologyClass};

/// Severity of one independent diagnostic condition.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    /// Condition is ready for its supported use.
    Ok,
    /// Action may be unnecessary or may need user/session attention.
    Warning,
    /// A core launch requirement is currently invalid or unavailable.
    Error,
}

/// One stable, structured doctor check.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DoctorCheck {
    /// Stable machine-readable check identifier.
    pub id: String,
    /// Independent severity.
    pub status: DoctorStatus,
    /// Concise human diagnostic.
    pub summary: String,
    /// Deterministically ordered supporting values.
    pub details: BTreeMap<String, String>,
}

/// Complete read-only diagnostic result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DoctorReport {
    /// Worst check status.
    pub status: DoctorStatus,
    /// Checks in stable pipeline order.
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    /// Whether any check found a condition that blocks current core operation.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.status == DoctorStatus::Error
    }
}

/// Session values relevant to graphical applications launched by a user manager.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionEnvironment {
    pub display: Option<String>,
    pub wayland_display: Option<String>,
    pub current_desktop: Option<String>,
    pub dbus_session_bus_address: Option<String>,
}

impl SessionEnvironment {
    /// Capture only the environment keys diagnosed by this command.
    #[must_use]
    pub fn from_environment() -> Self {
        Self {
            display: environment_value("DISPLAY"),
            wayland_display: environment_value("WAYLAND_DISPLAY"),
            current_desktop: environment_value("XDG_CURRENT_DESKTOP"),
            dbus_session_bus_address: environment_value("DBUS_SESSION_BUS_ADDRESS"),
        }
    }
}

/// Fully injectable doctor inputs; no path is mutated.
#[derive(Clone, Debug)]
pub struct DoctorOptions {
    pub registry_path: PathBuf,
    pub discovery: DiscoveryOptions,
    pub sysfs_root: PathBuf,
    pub proc_root: PathBuf,
    pub integration_paths: IntegrationPaths,
    pub launcher_executable: PathBuf,
    pub systemctl_executable: PathBuf,
    pub session: SessionEnvironment,
}

/// Run diagnostics using direct, no-shell `systemctl --user is-enabled` status.
#[must_use]
pub fn diagnose(options: &DoctorOptions) -> DoctorReport {
    diagnose_with_runner(options, DirectCommandRunner)
}

/// Run diagnostics with an injected command runner for isolated tests.
#[must_use]
pub fn diagnose_with_runner<R: CommandRunner>(options: &DoctorOptions, runner: R) -> DoctorReport {
    let mut checks = Vec::new();
    checks.push(check(
        "platform",
        DoctorStatus::Ok,
        "Linux runtime is supported.",
        [("platform", std::env::consts::OS.to_owned())],
    ));
    checks.push(check(
        "registry_path",
        DoctorStatus::Ok,
        "Registry path resolved without creating it.",
        [("path", options.registry_path.display().to_string())],
    ));

    let registry_result = RegistryStore::new(&options.registry_path).load_with_status();
    let registry_valid = registry_result.is_ok();
    let registry = match registry_result {
        Ok(load) => {
            checks.push(check(
                "registry",
                DoctorStatus::Ok,
                if load.exists {
                    "Registry syntax and policy are valid."
                } else {
                    "Registry is absent; the empty default registry is valid."
                },
                [
                    ("exists", load.exists.to_string()),
                    ("applications", load.registry.apps.len().to_string()),
                ],
            ));
            load.registry
        }
        Err(error) => {
            checks.push(check(
                "registry",
                DoctorStatus::Error,
                "Registry cannot be used.",
                [("error", error.to_string())],
            ));
            AppRegistry::default()
        }
    };
    let enabled: Vec<_> = registry
        .apps
        .iter()
        .filter(|application| application.enabled)
        .collect();
    checks.push(check(
        "enabled_applications",
        DoctorStatus::Ok,
        if enabled.is_empty() {
            "No applications are enabled; launch is a valid no-op."
        } else {
            "Enabled applications remain explicitly registry-controlled."
        },
        [(
            "desktop_ids",
            enabled
                .iter()
                .map(|application| application.desktop_id.as_str())
                .collect::<Vec<_>>()
                .join(","),
        )],
    ));

    let discovery_result =
        DesktopApplicationScanner::from_options(options.discovery.clone()).discover();
    match discovery_result {
        Ok(report) if registry_valid => {
            let available: BTreeMap<&str, _> = report
                .applications
                .iter()
                .map(|application| (application.desktop_id.as_str(), application))
                .collect();
            let unavailable: Vec<_> = enabled
                .iter()
                .filter(|application| !available.contains_key(application.desktop_id.as_str()))
                .map(|application| application.desktop_id.as_str())
                .collect();
            let terminal: Vec<_> = enabled
                .iter()
                .filter_map(|application| {
                    available
                        .get(application.desktop_id.as_str())
                        .filter(|current| current.terminal)
                        .map(|_current| application.desktop_id.as_str())
                })
                .collect();
            let status = if unavailable.is_empty() && terminal.is_empty() {
                DoctorStatus::Ok
            } else {
                DoctorStatus::Error
            };
            checks.push(check(
                "desktop_resolution",
                status,
                if status == DoctorStatus::Ok {
                    "Every enabled desktop ID resolves to a current non-terminal target."
                } else {
                    "One or more enabled desktop IDs cannot be launched safely."
                },
                [
                    ("unavailable", unavailable.join(",")),
                    ("terminal", terminal.join(",")),
                    ("discovery_warnings", report.warnings.len().to_string()),
                ],
            ));
        }
        Ok(_report) => checks.push(check(
            "desktop_resolution",
            DoctorStatus::Error,
            "Desktop resolution cannot be assessed until the registry is valid.",
            BTreeMap::<String, String>::new(),
        )),
        Err(error) => checks.push(check(
            "desktop_resolution",
            if enabled.is_empty() {
                DoctorStatus::Warning
            } else {
                DoctorStatus::Error
            },
            "Desktop discovery could not complete.",
            [("error", error.to_string())],
        )),
    }

    let topology = CpuTopologyDetector::new(&options.sysfs_root).detect();
    let efficiency_cpus = match topology {
        Ok(topology) => {
            let launchable = topology.classification == TopologyClass::Hybrid
                && !topology.efficiency_cpus.is_empty();
            checks.push(check(
                "topology",
                if launchable {
                    DoctorStatus::Ok
                } else if enabled.is_empty() {
                    DoctorStatus::Warning
                } else {
                    DoctorStatus::Error
                },
                if launchable {
                    "Reliable hybrid topology and E-core CPUs were detected."
                } else {
                    "Reliable non-empty E-core topology is unavailable; launch will fail closed."
                },
                [
                    ("classification", topology.classification.to_string()),
                    ("efficiency_cpus", join_cpus(&topology.efficiency_cpus)),
                ],
            ));
            topology.efficiency_cpus
        }
        Err(error) => {
            checks.push(check(
                "topology",
                if enabled.is_empty() {
                    DoctorStatus::Warning
                } else {
                    DoctorStatus::Error
                },
                "CPU topology detection failed; launch will fail closed.",
                [("error", error.to_string())],
            ));
            Vec::new()
        }
    };
    checks.push(affinity_check(&efficiency_cpus, !enabled.is_empty()));

    let privileged: Vec<_> = enabled
        .iter()
        .filter(|application| {
            application.nice < 0 || application.io_class == IoPriorityClass::Realtime
        })
        .map(|application| application.desktop_id.as_str())
        .collect();
    checks.push(check(
        "runtime_policies",
        if privileged.is_empty() {
            DoctorStatus::Ok
        } else {
            DoctorStatus::Warning
        },
        if privileged.is_empty() {
            "Stored nice and I/O policies are valid and do not normally require privilege."
        } else {
            "Some valid policies may fail without additional Linux privileges."
        },
        [("privilege_sensitive", privileged.join(","))],
    ));

    let startup = StartupManager::new(
        options.integration_paths.clone(),
        options.launcher_executable.clone(),
        options.registry_path.clone(),
        options.systemctl_executable.clone(),
        runner,
    )
    .and_then(|manager| manager.status(&registry));
    match startup {
        Ok(status) => {
            let startup_status = if (status.unit_present && !status.unit_owned)
                || (status.ownership_present && !status.ownership_owned)
                || (status.unit_present && !status.unit_current)
            {
                DoctorStatus::Error
            } else if !status.unit_present
                || !status.ownership_owned
                || status.enabled != Some(true)
            {
                DoctorStatus::Warning
            } else {
                DoctorStatus::Ok
            };
            checks.push(check(
                "startup_integration",
                startup_status,
                match startup_status {
                    DoctorStatus::Ok => "User graphical-session startup is installed and enabled.",
                    DoctorStatus::Warning => {
                        "User startup is optional, missing, disabled, or cannot be queried."
                    }
                    DoctorStatus::Error => "The startup unit exists but is unowned or stale.",
                },
                [
                    ("unit", status.unit_path.display().to_string()),
                    ("present", status.unit_present.to_string()),
                    ("owned", status.unit_owned.to_string()),
                    ("current", status.unit_current.to_string()),
                    ("ownership_present", status.ownership_present.to_string()),
                    ("ownership_owned", status.ownership_owned.to_string()),
                    (
                        "enabled",
                        status
                            .enabled
                            .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                    ),
                    (
                        "systemctl_diagnostic",
                        status.systemctl_diagnostic.unwrap_or_default(),
                    ),
                ],
            ));
            let manager_ready = status
                .manager_environment
                .as_ref()
                .is_some_and(|environment| environment.is_ready());
            checks.push(check(
                "user_manager_environment",
                if manager_ready {
                    DoctorStatus::Ok
                } else {
                    DoctorStatus::Warning
                },
                if manager_ready {
                    "The systemd user manager has graphical desktop and session-bus context."
                } else {
                    "The systemd user manager is missing graphical/desktop/session-bus context or could not be queried."
                },
                [
                    ("ready", manager_ready.to_string()),
                    (
                        "diagnostic",
                        status.manager_environment_diagnostic.unwrap_or_default(),
                    ),
                ],
            ));
            let duplicate: Vec<_> = status
                .autostart
                .iter()
                .filter(|assessment| assessment.state == AutostartState::DuplicateRisk)
                .map(|assessment| assessment.desktop_id.as_str())
                .collect();
            let conflicts: Vec<_> = status
                .autostart
                .iter()
                .filter(|assessment| {
                    assessment.state == AutostartState::UserOverride
                        && !assessment.system_entries.is_empty()
                })
                .map(|assessment| assessment.desktop_id.as_str())
                .collect();
            let owned: Vec<_> = status
                .autostart
                .iter()
                .filter(|assessment| assessment.state == AutostartState::SuppressedByLauncher)
                .map(|assessment| assessment.desktop_id.as_str())
                .collect();
            checks.push(check(
                "desktop_autostart",
                if duplicate.is_empty() && conflicts.is_empty() {
                    DoctorStatus::Ok
                } else {
                    DoctorStatus::Warning
                },
                if duplicate.is_empty() && conflicts.is_empty() {
                    "No duplicate desktop-autostart risk was detected."
                } else {
                    "Desktop autostart may duplicate a managed launch or has a user-file conflict."
                },
                [
                    ("duplicate_risk", duplicate.join(",")),
                    ("user_conflicts", conflicts.join(",")),
                    ("owned_overrides", owned.join(",")),
                ],
            ));
        }
        Err(error) => {
            checks.push(check(
                "startup_integration",
                DoctorStatus::Error,
                "Startup integration paths or state are invalid.",
                [("error", error.to_string())],
            ));
            checks.push(check(
                "user_manager_environment",
                DoctorStatus::Warning,
                "The systemd user-manager environment could not be assessed.",
                [("error", error.to_string())],
            ));
            checks.push(check(
                "desktop_autostart",
                DoctorStatus::Error,
                "Autostart state could not be inspected safely.",
                [("error", error.to_string())],
            ));
        }
    }
    checks.push(session_check(&options.session));
    checks.push(procfs_check(&options.proc_root));
    checks.push(check(
        "runtime_dependencies",
        DoctorStatus::Ok,
        "Affinity, nice, I/O-priority, direct exec, and bounded acknowledgement APIs are compiled in.",
        [
            ("shell", "not used".to_owned()),
            ("privilege", "not globally required".to_owned()),
        ],
    ));
    let status = checks
        .iter()
        .map(|check| check.status)
        .max()
        .unwrap_or(DoctorStatus::Ok);
    DoctorReport { status, checks }
}

fn affinity_check(efficiency_cpus: &[u32], enabled: bool) -> DoctorCheck {
    match sched_getaffinity(Pid::this()) {
        Ok(allowed) => {
            let representable = efficiency_cpus.iter().all(|cpu| {
                usize::try_from(*cpu)
                    .ok()
                    .filter(|cpu| *cpu < CpuSet::count())
                    .is_some()
            });
            let allowed_cpus: BTreeSet<u32> = (0..CpuSet::count())
                .filter(|cpu| allowed.is_set(*cpu).unwrap_or(false))
                .filter_map(|cpu| u32::try_from(cpu).ok())
                .collect();
            let all_allowed = efficiency_cpus.iter().all(|cpu| allowed_cpus.contains(cpu));
            let usable = representable && all_allowed && !efficiency_cpus.is_empty();
            check(
                "affinity_api",
                if usable || !enabled {
                    if efficiency_cpus.is_empty() || !usable {
                        DoctorStatus::Warning
                    } else {
                        DoctorStatus::Ok
                    }
                } else {
                    DoctorStatus::Error
                },
                if usable {
                    "The affinity API can represent every detected E-core allowed to this process."
                } else {
                    "Detected E-cores are empty, unrepresentable, or outside this process's allowed CPU set."
                },
                [
                    ("requested_cpus", join_cpus(efficiency_cpus)),
                    (
                        "allowed_cpus",
                        join_cpus(&allowed_cpus.into_iter().collect::<Vec<_>>()),
                    ),
                ],
            )
        }
        Err(error) => check(
            "affinity_api",
            if enabled {
                DoctorStatus::Error
            } else {
                DoctorStatus::Warning
            },
            "The current process affinity set could not be read.",
            [("error", error.to_string())],
        ),
    }
}

fn session_check(session: &SessionEnvironment) -> DoctorCheck {
    let graphical = session.display.is_some() || session.wayland_display.is_some();
    let ready = graphical
        && session.dbus_session_bus_address.is_some()
        && session.current_desktop.is_some();
    check(
        "graphical_session",
        if ready {
            DoctorStatus::Ok
        } else {
            DoctorStatus::Warning
        },
        if ready {
            "Current graphical/session environment appears ready."
        } else {
            "Current environment is missing graphical, desktop, or session-bus context; ensure the user manager imports it."
        },
        [
            ("display", session.display.clone().unwrap_or_default()),
            (
                "wayland_display",
                session.wayland_display.clone().unwrap_or_default(),
            ),
            (
                "current_desktop",
                session.current_desktop.clone().unwrap_or_default(),
            ),
            (
                "dbus_session_bus",
                session
                    .dbus_session_bus_address
                    .as_ref()
                    .map_or("missing", |_value| "present")
                    .to_owned(),
            ),
        ],
    )
}

fn procfs_check(root: &std::path::Path) -> DoctorCheck {
    let self_root = root.join("self");
    let stat = self_root.join("stat");
    let tasks = self_root.join("task");
    let children = tasks.join(std::process::id().to_string()).join("children");
    let stat_readable = std::fs::read_to_string(&stat).is_ok();
    let tasks_readable = std::fs::read_dir(&tasks).is_ok();
    let children_readable = std::fs::read_to_string(&children).is_ok();
    let ready = root.is_dir() && stat_readable && tasks_readable && children_readable;
    check(
        "procfs",
        if ready {
            DoctorStatus::Ok
        } else {
            DoctorStatus::Error
        },
        if ready {
            "Procfs exposes process identity, task, and child metadata for supervision."
        } else {
            "Procfs process/task metadata required by supervision is unavailable."
        },
        [
            ("root", root.display().to_string()),
            ("ready", ready.to_string()),
            ("stat_readable", stat_readable.to_string()),
            ("tasks_readable", tasks_readable.to_string()),
            ("children_readable", children_readable.to_string()),
        ],
    )
}

fn check<I, K>(id: &str, status: DoctorStatus, summary: &str, details: I) -> DoctorCheck
where
    I: IntoIterator<Item = (K, String)>,
    K: Into<String>,
{
    DoctorCheck {
        id: id.to_owned(),
        status,
        summary: summary.to_owned(),
        details: details
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    }
}

fn join_cpus(cpus: &[u32]) -> String {
    cpus.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn environment_value(key: &str) -> Option<String> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{diagnose_with_runner, DoctorOptions, DoctorStatus, SessionEnvironment};
    use crate::discovery::DiscoveryOptions;
    use crate::integration::{CommandResult, CommandRunner, IntegrationPaths};
    use nix::sched::{sched_getaffinity, CpuSet};
    use nix::unistd::Pid;

    struct EnabledRunner;

    impl CommandRunner for EnabledRunner {
        fn run(
            &self,
            _program: &Path,
            arguments: &[std::ffi::OsString],
        ) -> std::io::Result<CommandResult> {
            let stdout = if arguments
                .iter()
                .any(|argument| argument == "show-environment")
            {
                b"DISPLAY=:0\nXDG_CURRENT_DESKTOP=Test\nDBUS_SESSION_BUS_ADDRESS=present\n".to_vec()
            } else {
                b"enabled\n".to_vec()
            };
            Ok(CommandResult {
                success: true,
                stdout,
                stderr: Vec::new(),
            })
        }
    }

    fn options(root: &Path) -> DoctorOptions {
        let config_home = root.join("config");
        let data_home = root.join("data");
        fs::create_dir_all(data_home.join("applications")).unwrap();
        DoctorOptions {
            registry_path: config_home.join("ecore-launcher/config.toml"),
            discovery: DiscoveryOptions {
                data_home: Some(data_home),
                data_dirs: Vec::new(),
                executable_path: vec![PathBuf::from("/bin")],
                locale: Some("C".to_owned()),
                current_desktops: vec!["Test".to_owned()],
                include_no_display: true,
                ignore_desktop_filter: false,
                require_existing_roots: true,
            },
            sysfs_root: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/sysfs/intel-uniform"),
            proc_root: PathBuf::from("/proc"),
            integration_paths: IntegrationPaths {
                config_home,
                config_dirs: vec![root.join("system-config")],
                state_home: root.join("state"),
            },
            launcher_executable: PathBuf::from("/bin/true"),
            systemctl_executable: PathBuf::from("/bin/true"),
            session: SessionEnvironment {
                display: Some(":0".to_owned()),
                wayland_display: None,
                current_desktop: Some("Test".to_owned()),
                dbus_session_bus_address: Some("unix:path=/fixture".to_owned()),
            },
        }
    }

    fn synthetic_hybrid(root: &Path) -> PathBuf {
        let allowed = sched_getaffinity(Pid::this())
            .unwrap_or_else(|error| panic!("read test affinity: {error}"));
        let efficiency = (0..CpuSet::count())
            .find(|cpu| allowed.is_set(*cpu).unwrap_or(false))
            .and_then(|cpu| u32::try_from(cpu).ok())
            .unwrap_or_else(|| panic!("test process needs one allowed CPU"));
        let performance = if efficiency == 0 { 1 } else { 0 };
        let sysfs = root.join("sysfs");
        let mut cpus = [efficiency, performance];
        cpus.sort_unstable();
        fs::create_dir_all(&sysfs).unwrap();
        fs::write(sysfs.join("online"), format!("{},{}\n", cpus[0], cpus[1])).unwrap();
        fs::write(sysfs.join("present"), format!("{},{}\n", cpus[0], cpus[1])).unwrap();
        for (cpu, core_type) in [(efficiency, 32), (performance, 64)] {
            let topology = sysfs.join(format!("cpu{cpu}/topology"));
            fs::create_dir_all(&topology).unwrap();
            fs::write(topology.join("core_type"), format!("{core_type}\n")).unwrap();
            fs::write(topology.join("core_id"), format!("{cpu}\n")).unwrap();
            fs::write(topology.join("physical_package_id"), "0\n").unwrap();
            fs::write(topology.join("thread_siblings_list"), format!("{cpu}\n")).unwrap();
        }
        sysfs
    }

    fn healthy_options(root: &Path) -> DoctorOptions {
        let mut options = options(root);
        options.sysfs_root = synthetic_hybrid(root);
        let applications = options
            .discovery
            .data_home
            .as_ref()
            .unwrap()
            .join("applications");
        fs::write(
            applications.join("healthy.desktop"),
            "[Desktop Entry]\nType=Application\nName=Healthy\nExec=/bin/true\n",
        )
        .unwrap();
        fs::create_dir_all(options.registry_path.parent().unwrap()).unwrap();
        fs::write(
            &options.registry_path,
            "schema_version = 1\n[[apps]]\ndesktop_id = \"healthy.desktop\"\nname = \"Stored\"\nenabled = true\ndelay_seconds = 0\nnice = 5\nio_class = \"none\"\nenforce_process_tree = false\n",
        )
        .unwrap();
        let manager = crate::integration::StartupManager::new(
            options.integration_paths.clone(),
            options.launcher_executable.clone(),
            options.registry_path.clone(),
            options.systemctl_executable.clone(),
            EnabledRunner,
        )
        .unwrap();
        let registry = crate::registry::RegistryStore::new(&options.registry_path)
            .load()
            .unwrap();
        manager.enable(&registry, false).unwrap();
        options
    }

    #[test]
    fn empty_fixture_is_read_only_deterministic_and_warns_for_uniform_topology() {
        let root =
            std::env::temp_dir().join(format!("ecore-launcher-doctor-{}", std::process::id()));
        let _ignored = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let options = options(&root);
        let before: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        let first = diagnose_with_runner(&options, EnabledRunner);
        let second = diagnose_with_runner(&options, EnabledRunner);
        assert_eq!(first, second);
        assert!(first
            .checks
            .iter()
            .any(|check| check.id == "topology" && check.status == DoctorStatus::Warning));
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
        let after: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn invalid_registry_and_missing_procfs_are_errors_without_panics() {
        let root = std::env::temp_dir().join(format!(
            "ecore-launcher-doctor-invalid-{}",
            std::process::id()
        ));
        let _ignored = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("config/ecore-launcher")).unwrap();
        let mut options = options(&root);
        fs::write(&options.registry_path, "not = [valid").unwrap();
        options.proc_root = root.join("missing-proc");
        let report = diagnose_with_runner(&options, EnabledRunner);
        assert!(report.has_errors());
        assert!(report
            .checks
            .iter()
            .any(|check| check.id == "registry" && check.status == DoctorStatus::Error));
        assert!(report
            .checks
            .iter()
            .any(|check| check.id == "procfs" && check.status == DoctorStatus::Error));
    }

    #[test]
    fn healthy_fixture_is_ok_and_unavailable_app_is_reported_without_mutation() {
        let root = std::env::temp_dir().join(format!(
            "ecore-launcher-doctor-healthy-{}",
            std::process::id()
        ));
        let _ignored = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let options = healthy_options(&root);
        let unit_path = options.integration_paths.unit_path();
        let unit_before = fs::read(&unit_path).unwrap();
        let healthy = diagnose_with_runner(&options, EnabledRunner);
        assert_eq!(healthy.status, DoctorStatus::Ok);
        assert_eq!(fs::read(&unit_path).unwrap(), unit_before);

        fs::remove_file(
            options
                .discovery
                .data_home
                .as_ref()
                .unwrap()
                .join("applications/healthy.desktop"),
        )
        .unwrap();
        let unavailable = diagnose_with_runner(&options, EnabledRunner);
        let check = unavailable
            .checks
            .iter()
            .find(|check| check.id == "desktop_resolution")
            .unwrap();
        assert_eq!(check.status, DoctorStatus::Error);
        assert_eq!(check.details["unavailable"], "healthy.desktop");
        assert_eq!(fs::read(&unit_path).unwrap(), unit_before);
    }

    #[test]
    fn conflicting_autostart_is_a_warning_and_remains_untouched() {
        let root = std::env::temp_dir().join(format!(
            "ecore-launcher-doctor-autostart-{}",
            std::process::id()
        ));
        let _ignored = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let options = healthy_options(&root);
        let system_entry = root.join("system-config/autostart/healthy.desktop");
        let user_entry = root.join("config/autostart/healthy.desktop");
        fs::create_dir_all(system_entry.parent().unwrap()).unwrap();
        fs::create_dir_all(user_entry.parent().unwrap()).unwrap();
        fs::write(&system_entry, "system\n").unwrap();
        fs::write(&user_entry, "user-owned\n").unwrap();
        let report = diagnose_with_runner(&options, EnabledRunner);
        let check = report
            .checks
            .iter()
            .find(|check| check.id == "desktop_autostart")
            .unwrap();
        assert_eq!(check.status, DoctorStatus::Warning);
        assert_eq!(check.details["user_conflicts"], "healthy.desktop");
        assert_eq!(fs::read_to_string(user_entry).unwrap(), "user-owned\n");
    }
}
