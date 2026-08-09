//! User-owned systemd startup and explicit desktop-autostart suppression.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::registry::AppRegistry;

/// Name reserved for the launcher-owned user service.
pub const USER_UNIT_NAME: &str = "ecore-launcher.service";
const UNIT_MARKER: &str = "# Generated and owned by ecore-launcher.\n";
const STARTUP_STATE_MARKER: &str = "ecore-launcher startup ownership v1\n";
const AUTOSTART_MARKER: &str = "X-Ecore-Launcher-Owned=true";
static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// XDG locations used by startup and autostart integration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrationPaths {
    /// User configuration root, never a system directory.
    pub config_home: PathBuf,
    /// Ordered system configuration roots inspected read-only for autostart.
    pub config_dirs: Vec<PathBuf>,
    /// User state root containing only launcher ownership metadata.
    pub state_home: PathBuf,
}

impl IntegrationPaths {
    /// Resolve XDG roots without creating anything.
    pub fn from_environment(
        explicit_config_home: Option<&Path>,
        explicit_config_dirs: &[PathBuf],
        explicit_state_home: Option<&Path>,
    ) -> Result<Self, IntegrationError> {
        let config_home = match explicit_config_home {
            Some(path) => path.to_owned(),
            None => std::env::var_os("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                        .map(|home| home.join(".config"))
                })
                .ok_or(IntegrationError::ConfigHomeUnavailable)?,
        };
        validate_absolute_path(&config_home, "XDG configuration home")?;
        validate_user_root(&config_home, "XDG configuration home")?;
        let mut config_dirs = if explicit_config_dirs.is_empty() {
            std::env::var_os("XDG_CONFIG_DIRS")
                .map(|value| std::env::split_paths(&value).collect())
                .filter(|paths: &Vec<PathBuf>| !paths.is_empty())
                .unwrap_or_else(|| vec![PathBuf::from("/etc/xdg")])
        } else {
            explicit_config_dirs.to_vec()
        };
        if explicit_config_dirs.is_empty()
            && !config_dirs
                .iter()
                .any(|path| path == Path::new("/usr/share"))
        {
            // KDE and some distributions also ship autostart entries here.
            // It remains a read-only source; suppression is always a user file.
            config_dirs.push(PathBuf::from("/usr/share"));
        }
        for path in &config_dirs {
            validate_absolute_path(path, "XDG system configuration root")?;
        }
        let state_home = match explicit_state_home {
            Some(path) => path.to_owned(),
            None => std::env::var_os("XDG_STATE_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                        .map(|home| home.join(".local/state"))
                })
                .ok_or(IntegrationError::StateHomeUnavailable)?,
        };
        validate_absolute_path(&state_home, "XDG state home")?;
        validate_user_root(&state_home, "XDG state home")?;
        Ok(Self {
            config_home,
            config_dirs,
            state_home,
        })
    }

    /// Launcher-owned user unit path.
    #[must_use]
    pub fn unit_path(&self) -> PathBuf {
        self.config_home.join("systemd/user").join(USER_UNIT_NAME)
    }

    /// XDG user autostart directory.
    #[must_use]
    pub fn autostart_dir(&self) -> PathBuf {
        self.config_home.join("autostart")
    }

    /// Marker used to prove ownership if the unit itself was manually removed.
    #[must_use]
    pub fn ownership_path(&self) -> PathBuf {
        self.state_home.join("ecore-launcher/startup-owned")
    }
}

/// Result of one direct command invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandResult {
    /// Whether the command returned a successful exit status.
    pub success: bool,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
}

/// Injectable direct-process runner; argument boundaries are never interpreted.
pub trait CommandRunner {
    /// Execute one program directly.
    fn run(&self, program: &Path, arguments: &[OsString]) -> io::Result<CommandResult>;
}

/// Production runner backed by `std::process::Command` with no shell.
#[derive(Clone, Copy, Debug, Default)]
pub struct DirectCommandRunner;

impl CommandRunner for DirectCommandRunner {
    fn run(&self, program: &Path, arguments: &[OsString]) -> io::Result<CommandResult> {
        let output = Command::new(program).args(arguments).output()?;
        Ok(CommandResult {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// Classification of an enabled app's current desktop-autostart state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutostartState {
    /// No system autostart entry was detected.
    NotPresent,
    /// A system entry exists and no user override suppresses it.
    DuplicateRisk,
    /// The exact launcher-owned `Hidden=true` override is present.
    SuppressedByLauncher,
    /// A user-owned file occupies the override path and is never overwritten.
    UserOverride,
}

/// Read-only autostart assessment for one registered application.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AutostartAssessment {
    /// Stable registry identity.
    pub desktop_id: String,
    /// Current state.
    pub state: AutostartState,
    /// System entry paths which establish duplicate risk.
    pub system_entries: Vec<PathBuf>,
    /// User override path.
    pub user_override: PathBuf,
}

/// Read-only startup integration state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StartupStatus {
    /// User-level unit location.
    pub unit_path: PathBuf,
    /// Whether a unit file exists at that location.
    pub unit_present: bool,
    /// Whether present contents match the launcher's generated format.
    pub unit_owned: bool,
    /// Whether present contents exactly match current executable/config inputs.
    pub unit_current: bool,
    /// Whether the startup ownership marker exists.
    pub ownership_present: bool,
    /// Whether a present ownership marker has the exact expected contents.
    pub ownership_owned: bool,
    /// `systemctl --user is-enabled` result; absent when systemctl was unavailable.
    pub enabled: Option<bool>,
    /// Read-only systemctl diagnostic, when its invocation failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub systemctl_diagnostic: Option<String>,
    /// Relevant environment readiness reported by the systemd user manager.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manager_environment: Option<ManagerEnvironmentStatus>,
    /// Diagnostic when user-manager environment could not be queried.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manager_environment_diagnostic: Option<String>,
    /// Enabled registered applications' autostart assessments.
    pub autostart: Vec<AutostartAssessment>,
}

/// Presence-only view of graphical variables in the systemd user manager.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ManagerEnvironmentStatus {
    pub display: bool,
    pub wayland_display: bool,
    pub current_desktop: bool,
    pub session_bus: bool,
}

impl ManagerEnvironmentStatus {
    /// A display protocol, desktop identity, and session bus are all present.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        (self.display || self.wayland_display) && self.current_desktop && self.session_bus
    }
}

/// Mutating startup operation summary.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct StartupChange {
    /// Unit installed or removed by this operation.
    pub unit_changed: bool,
    /// Desktop IDs whose launcher-owned overrides were created or removed.
    pub autostart_overrides_changed: Vec<String>,
}

/// Safe startup/autostart integration errors.
#[derive(Debug, Error)]
pub enum IntegrationError {
    /// No user configuration root was available.
    #[error("cannot resolve a user configuration home; set XDG_CONFIG_HOME or HOME")]
    ConfigHomeUnavailable,
    /// No user state root was available.
    #[error("cannot resolve a user state home; set XDG_STATE_HOME or HOME")]
    StateHomeUnavailable,
    /// Generated integration requires absolute, control-free paths.
    #[error("invalid {field} path {path}: {reason}")]
    InvalidPath {
        field: &'static str,
        path: PathBuf,
        reason: &'static str,
    },
    /// A desktop ID was unsafe to use as one direct filename.
    #[error("desktop ID `{desktop_id}` cannot be used for autostart integration")]
    InvalidDesktopId { desktop_id: String },
    /// An existing user file was not generated by this launcher.
    #[error("refusing to overwrite or remove unowned file {path}")]
    UnownedFile { path: PathBuf },
    /// Final symlinks are never followed or replaced by integration mutations.
    #[error("refusing symlinked integration path {path}")]
    SymlinkRejected { path: PathBuf },
    /// Filesystem operation failed.
    #[error("integration filesystem operation failed for {path}: {source}")]
    FileSystem {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A direct systemctl operation failed.
    #[error("systemctl --user {operation} failed: {reason}")]
    Systemctl {
        operation: &'static str,
        reason: String,
    },
    /// A required path was not UTF-8 representable in a systemd unit.
    #[error("{field} path is not valid UTF-8 and cannot be encoded safely in a systemd unit")]
    NonUtf8Path { field: &'static str },
    /// Integration roots must resolve beneath state owned by the invoking user.
    #[error("{field} path {path} is not a safe user-owned location: {reason}")]
    UnsafeUserRoot {
        field: &'static str,
        path: PathBuf,
        reason: String,
    },
}

/// Reusable manager for user-level startup integration.
pub struct StartupManager<R> {
    paths: IntegrationPaths,
    launcher_executable: PathBuf,
    registry_path: PathBuf,
    systemctl_executable: PathBuf,
    runner: R,
}

impl<R: CommandRunner> StartupManager<R> {
    /// Construct a manager without reading or changing integration state.
    pub fn new(
        paths: IntegrationPaths,
        launcher_executable: PathBuf,
        registry_path: PathBuf,
        systemctl_executable: PathBuf,
        runner: R,
    ) -> Result<Self, IntegrationError> {
        validate_user_root(&paths.config_home, "XDG configuration home")?;
        validate_user_root(&paths.state_home, "XDG state home")?;
        validate_absolute_path(&launcher_executable, "launcher executable")?;
        validate_absolute_path(&registry_path, "registry configuration")?;
        validate_path_bytes(&systemctl_executable, "systemctl executable")?;
        Ok(Self {
            paths,
            launcher_executable,
            registry_path,
            systemctl_executable,
            runner,
        })
    }

    /// Return deterministic unit contents for the current paths.
    pub fn unit_contents(&self) -> Result<String, IntegrationError> {
        let executable = systemd_quote_path(&self.launcher_executable, "launcher executable")?;
        let registry = systemd_quote_path(&self.registry_path, "registry configuration")?;
        Ok(format!(
            "{UNIT_MARKER}[Unit]\nDescription=Launch explicitly managed desktop applications on detected E-cores\nAfter=graphical-session.target\nPartOf=graphical-session.target\n\n[Service]\nType=simple\nExecStart=:{executable} --config {registry} supervise\nKillMode=process\nRestart=no\n\n[Install]\nWantedBy=graphical-session.target\n"
        ))
    }

    /// Install/update the owned unit, enable it for graphical-session startup,
    /// and optionally create explicit duplicate-autostart suppressions.
    pub fn enable(
        &self,
        registry: &AppRegistry,
        suppress_autostart: bool,
    ) -> Result<StartupChange, IntegrationError> {
        let assessments = assess_autostart(&self.paths, registry)?;
        if suppress_autostart {
            preflight_suppression(&assessments)?;
        }
        let contents = self.unit_contents()?;
        let unit_path = self.paths.unit_path();
        let ownership_path = self.paths.ownership_path();
        reject_unowned_existing_unit(&unit_path)?;
        reject_unowned_state(&ownership_path)?;
        let changed = read_optional_regular(&unit_path)?
            .map(|current| current.as_bytes() != contents.as_bytes())
            .unwrap_or(true);
        if changed {
            atomic_write(&unit_path, contents.as_bytes(), is_generated_unit)?;
        }
        if read_optional_regular(&ownership_path)?.as_deref() != Some(STARTUP_STATE_MARKER) {
            atomic_write(&ownership_path, STARTUP_STATE_MARKER.as_bytes(), |value| {
                value == STARTUP_STATE_MARKER
            })?;
        }
        self.systemctl("daemon-reload", &["daemon-reload"])?;
        self.systemctl("enable", &["enable", USER_UNIT_NAME])?;
        let autostart_overrides_changed = if suppress_autostart {
            create_suppressions(&assessments)?
        } else {
            Vec::new()
        };
        Ok(StartupChange {
            unit_changed: changed,
            autostart_overrides_changed,
        })
    }

    /// Disable and remove only launcher-owned integration. Running targets and
    /// the application registry are untouched.
    pub fn disable(&self) -> Result<StartupChange, IntegrationError> {
        let unit_path = self.paths.unit_path();
        let ownership_path = self.paths.ownership_path();
        let current = read_optional_regular(&unit_path)?;
        let ownership = read_optional_regular(&ownership_path)?;
        let unit_present = current.is_some();
        if let Some(contents) = current.as_deref() {
            if !is_generated_unit(contents) {
                return Err(IntegrationError::UnownedFile { path: unit_path });
            }
        }
        if ownership
            .as_deref()
            .is_some_and(|value| value != STARTUP_STATE_MARKER)
        {
            return Err(IntegrationError::UnownedFile {
                path: ownership_path,
            });
        }
        if unit_present || ownership.is_some() {
            self.systemctl("disable", &["disable", USER_UNIT_NAME])?;
            if unit_present {
                remove_regular_owned(&unit_path, is_generated_unit)?;
            }
            if ownership.is_some() {
                remove_regular_owned(&ownership_path, |value| value == STARTUP_STATE_MARKER)?;
            }
            self.systemctl("daemon-reload", &["daemon-reload"])?;
        }
        let autostart_overrides_changed = remove_owned_suppressions(&self.paths)?;
        Ok(StartupChange {
            unit_changed: unit_present,
            autostart_overrides_changed,
        })
    }

    /// Inspect unit and autostart state without modifying the filesystem.
    pub fn status(&self, registry: &AppRegistry) -> Result<StartupStatus, IntegrationError> {
        let desired = self.unit_contents()?;
        let unit_path = self.paths.unit_path();
        let ownership_path = self.paths.ownership_path();
        let current = read_optional_regular(&unit_path)?;
        let ownership = read_optional_regular(&ownership_path)?;
        let unit_present = current.is_some();
        let unit_owned = current.as_deref().is_some_and(is_generated_unit);
        let unit_current = current.as_deref() == Some(desired.as_str());
        let result = self.runner.run(
            &self.systemctl_executable,
            &[
                OsString::from("--user"),
                OsString::from("is-enabled"),
                OsString::from(USER_UNIT_NAME),
            ],
        );
        let (enabled, systemctl_diagnostic) = match result {
            Ok(result) if result.success => (Some(true), None),
            Ok(result) => {
                let stdout = String::from_utf8_lossy(&result.stdout);
                let state = stdout.trim();
                if matches!(state, "disabled" | "static" | "indirect" | "not-found") {
                    (Some(false), None)
                } else {
                    (Some(false), Some(command_diagnostic(&result)))
                }
            }
            Err(error) => (None, Some(error.to_string())),
        };
        let environment_result = self.runner.run(
            &self.systemctl_executable,
            &[OsString::from("--user"), OsString::from("show-environment")],
        );
        let (manager_environment, manager_environment_diagnostic) = match environment_result {
            Ok(result) if result.success => (Some(parse_manager_environment(&result.stdout)), None),
            Ok(result) => (None, Some(command_diagnostic(&result))),
            Err(error) => (None, Some(error.to_string())),
        };
        Ok(StartupStatus {
            unit_path,
            unit_present,
            unit_owned,
            unit_current,
            ownership_present: ownership.is_some(),
            ownership_owned: ownership.as_deref() == Some(STARTUP_STATE_MARKER),
            enabled,
            systemctl_diagnostic,
            manager_environment,
            manager_environment_diagnostic,
            autostart: assess_autostart(&self.paths, registry)?,
        })
    }

    fn systemctl(
        &self,
        operation: &'static str,
        arguments: &[&str],
    ) -> Result<(), IntegrationError> {
        let mut direct_arguments = vec![OsString::from("--user")];
        direct_arguments.extend(arguments.iter().map(OsString::from));
        let result = self
            .runner
            .run(&self.systemctl_executable, &direct_arguments)
            .map_err(|source| IntegrationError::Systemctl {
                operation,
                reason: source.to_string(),
            })?;
        if result.success {
            Ok(())
        } else {
            Err(IntegrationError::Systemctl {
                operation,
                reason: command_diagnostic(&result),
            })
        }
    }
}

/// Assess only enabled, explicitly registered IDs against XDG autostart state.
pub fn assess_autostart(
    paths: &IntegrationPaths,
    registry: &AppRegistry,
) -> Result<Vec<AutostartAssessment>, IntegrationError> {
    let mut assessments = Vec::new();
    for application in registry
        .apps
        .iter()
        .filter(|application| application.enabled)
    {
        validate_desktop_id(&application.desktop_id)?;
        let mut system_entries = Vec::new();
        for root in &paths.config_dirs {
            let path = root.join("autostart").join(&application.desktop_id);
            match fs::symlink_metadata(&path) {
                Ok(_metadata) => system_entries.push(path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => return Err(IntegrationError::FileSystem { path, source }),
            }
        }
        let user_override = paths.autostart_dir().join(&application.desktop_id);
        let state = match read_optional_regular(&user_override)? {
            Some(contents) if contents == owned_autostart_contents(&application.desktop_id) => {
                AutostartState::SuppressedByLauncher
            }
            Some(_contents) => AutostartState::UserOverride,
            None if system_entries.is_empty() => AutostartState::NotPresent,
            None => AutostartState::DuplicateRisk,
        };
        assessments.push(AutostartAssessment {
            desktop_id: application.desktop_id.clone(),
            state,
            system_entries,
            user_override,
        });
    }
    assessments.sort_by(|left, right| left.desktop_id.cmp(&right.desktop_id));
    Ok(assessments)
}

fn preflight_suppression(assessments: &[AutostartAssessment]) -> Result<(), IntegrationError> {
    if let Some(conflict) = assessments.iter().find(|assessment| {
        assessment.state == AutostartState::UserOverride && !assessment.system_entries.is_empty()
    }) {
        return Err(IntegrationError::UnownedFile {
            path: conflict.user_override.clone(),
        });
    }
    Ok(())
}

fn create_suppressions(
    assessments: &[AutostartAssessment],
) -> Result<Vec<String>, IntegrationError> {
    let mut changed = Vec::new();
    for assessment in assessments {
        if assessment.state != AutostartState::DuplicateRisk {
            continue;
        }
        let expected = owned_autostart_contents(&assessment.desktop_id);
        atomic_write(&assessment.user_override, expected.as_bytes(), |value| {
            value == expected
        })?;
        changed.push(assessment.desktop_id.clone());
    }
    Ok(changed)
}

fn remove_owned_suppressions(paths: &IntegrationPaths) -> Result<Vec<String>, IntegrationError> {
    let mut removed = Vec::new();
    validate_user_root(&paths.autostart_dir(), "autostart integration directory")?;
    let entries = match fs::read_dir(paths.autostart_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(removed),
        Err(source) => {
            return Err(IntegrationError::FileSystem {
                path: paths.autostart_dir(),
                source,
            })
        }
    };
    let mut paths_to_check = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| IntegrationError::FileSystem {
            path: paths.autostart_dir(),
            source,
        })?;
        paths_to_check.push(entry.path());
    }
    paths_to_check.sort();
    for path in paths_to_check {
        let Some(desktop_id) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if validate_desktop_id(desktop_id).is_err() {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => continue,
            Ok(_metadata) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => return Err(IntegrationError::FileSystem { path, source }),
        }
        let expected = owned_autostart_contents(desktop_id);
        if read_optional_regular(&path)?.as_deref() != Some(expected.as_str()) {
            continue;
        }
        remove_regular_owned(&path, |contents| contents == expected)?;
        removed.push(desktop_id.to_owned());
    }
    Ok(removed)
}

fn owned_autostart_contents(desktop_id: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nHidden=true\n{AUTOSTART_MARKER}\nX-Ecore-Launcher-DesktopId={desktop_id}\n"
    )
}

fn reject_unowned_existing_unit(path: &Path) -> Result<(), IntegrationError> {
    if let Some(contents) = read_optional_regular(path)? {
        if !is_generated_unit(&contents) {
            return Err(IntegrationError::UnownedFile {
                path: path.to_owned(),
            });
        }
    }
    Ok(())
}

fn reject_unowned_state(path: &Path) -> Result<(), IntegrationError> {
    if read_optional_regular(path)?
        .as_deref()
        .is_some_and(|contents| contents != STARTUP_STATE_MARKER)
    {
        Err(IntegrationError::UnownedFile {
            path: path.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn is_generated_unit(contents: &str) -> bool {
    if !contents.starts_with(UNIT_MARKER) || contents.contains('\0') {
        return false;
    }
    let lines: Vec<&str> = contents.lines().collect();
    let common_header = lines.get(1) == Some(&"[Unit]")
        && lines.get(2)
            == Some(
                &"Description=Launch explicitly managed desktop applications on detected E-cores",
            )
        && lines.get(3) == Some(&"After=graphical-session.target");
    let current = lines.len() == 14
        && common_header
        && lines[4] == "PartOf=graphical-session.target"
        && lines[5].is_empty()
        && lines[6] == "[Service]"
        && lines[7] == "Type=simple"
        && lines[8].starts_with("ExecStart=:")
        && lines[9] == "KillMode=process"
        && lines[10] == "Restart=no"
        && lines[11].is_empty()
        && lines[12] == "[Install]"
        && lines[13] == "WantedBy=graphical-session.target";
    let legacy = lines.len() == 13
        && common_header
        && lines[4].is_empty()
        && lines[5] == "[Service]"
        && lines[6] == "Type=simple"
        && lines[7].starts_with("ExecStart=")
        && lines[8] == "KillMode=process"
        && lines[9] == "Restart=no"
        && lines[10].is_empty()
        && lines[11] == "[Install]"
        && lines[12] == "WantedBy=graphical-session.target";
    current || legacy
}

fn systemd_quote_path(path: &Path, field: &'static str) -> Result<String, IntegrationError> {
    let value = path
        .to_str()
        .ok_or(IntegrationError::NonUtf8Path { field })?;
    if value.chars().any(char::is_control) {
        return Err(IntegrationError::InvalidPath {
            field,
            path: path.to_owned(),
            reason: "control characters are not permitted",
        });
    }
    let escaped = value
        .replace('%', "%%")
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

fn validate_absolute_path(path: &Path, field: &'static str) -> Result<(), IntegrationError> {
    validate_path_bytes(path, field)?;
    if !path.is_absolute() {
        return Err(IntegrationError::InvalidPath {
            field,
            path: path.to_owned(),
            reason: "an absolute path is required",
        });
    }
    Ok(())
}

fn validate_user_root(path: &Path, field: &'static str) -> Result<(), IntegrationError> {
    validate_absolute_path(path, field)?;
    let normalized = normalize_absolute(path);
    if is_system_path(&normalized) {
        return Err(IntegrationError::UnsafeUserRoot {
            field,
            path: path.to_owned(),
            reason: "system-owned locations are never integration destinations".to_owned(),
        });
    }
    let mut existing = path;
    loop {
        match fs::metadata(existing) {
            Ok(metadata) => {
                if fs::canonicalize(existing)
                    .map(|resolved| is_system_path(&resolved))
                    .unwrap_or(false)
                {
                    return Err(IntegrationError::UnsafeUserRoot {
                        field,
                        path: path.to_owned(),
                        reason: format!(
                            "nearest existing ancestor {} resolves into a system-owned location",
                            existing.display()
                        ),
                    });
                }
                let current_uid = rustix::process::geteuid().as_raw();
                if metadata.uid() != current_uid {
                    return Err(IntegrationError::UnsafeUserRoot {
                        field,
                        path: path.to_owned(),
                        reason: format!(
                            "nearest existing ancestor {} is owned by UID {}, not UID {current_uid}",
                            existing.display(),
                            metadata.uid()
                        ),
                    });
                }
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                existing = existing
                    .parent()
                    .ok_or_else(|| IntegrationError::UnsafeUserRoot {
                        field,
                        path: path.to_owned(),
                        reason: "no existing user-owned ancestor was found".to_owned(),
                    })?;
            }
            Err(source) => {
                return Err(IntegrationError::FileSystem {
                    path: existing.to_owned(),
                    source,
                })
            }
        }
    }
}

fn is_system_path(path: &Path) -> bool {
    let system_prefixes = [
        Path::new("/etc"),
        Path::new("/usr"),
        Path::new("/lib"),
        Path::new("/lib64"),
        Path::new("/boot"),
        Path::new("/proc"),
        Path::new("/sys"),
        Path::new("/dev"),
        Path::new("/run/systemd"),
        Path::new("/var/lib"),
        Path::new("/var/run"),
    ];
    path == Path::new("/")
        || system_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

fn normalize_absolute(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(value) => normalized.push(value),
            std::path::Component::Prefix(_prefix) => {}
        }
    }
    normalized
}

fn validate_path_bytes(path: &Path, field: &'static str) -> Result<(), IntegrationError> {
    if path.as_os_str().is_empty() {
        return Err(IntegrationError::InvalidPath {
            field,
            path: path.to_owned(),
            reason: "the path is empty",
        });
    }
    if path
        .as_os_str()
        .as_bytes()
        .iter()
        .any(|byte| byte.is_ascii_control())
    {
        return Err(IntegrationError::InvalidPath {
            field,
            path: path.to_owned(),
            reason: "control characters are not permitted",
        });
    }
    Ok(())
}

fn validate_desktop_id(desktop_id: &str) -> Result<(), IntegrationError> {
    if desktop_id.is_empty()
        || desktop_id == "."
        || desktop_id == ".."
        || desktop_id.contains(['/', '\\', '\0'])
        || desktop_id.chars().any(char::is_control)
    {
        Err(IntegrationError::InvalidDesktopId {
            desktop_id: desktop_id.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn read_optional_regular(path: &Path) -> Result<Option<String>, IntegrationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(IntegrationError::SymlinkRejected {
                path: path.to_owned(),
            })
        }
        Ok(metadata) if !metadata.is_file() => Err(IntegrationError::UnownedFile {
            path: path.to_owned(),
        }),
        Ok(_metadata) => {
            fs::read_to_string(path)
                .map(Some)
                .map_err(|source| IntegrationError::FileSystem {
                    path: path.to_owned(),
                    source,
                })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(IntegrationError::FileSystem {
            path: path.to_owned(),
            source,
        }),
    }
}

fn atomic_write(
    path: &Path,
    contents: &[u8],
    existing_is_owned: impl Fn(&str) -> bool,
) -> Result<(), IntegrationError> {
    let parent = path.parent().ok_or_else(|| IntegrationError::InvalidPath {
        field: "integration destination",
        path: path.to_owned(),
        reason: "the path has no parent directory",
    })?;
    validate_user_root(parent, "integration destination")?;
    ensure_private_directory(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(IntegrationError::SymlinkRejected {
                path: path.to_owned(),
            })
        }
        Ok(_metadata) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(IntegrationError::FileSystem {
                path: path.to_owned(),
                source,
            })
        }
    }
    if let Some(existing) = read_optional_regular(path)? {
        if !existing_is_owned(&existing) {
            return Err(IntegrationError::UnownedFile {
                path: path.to_owned(),
            });
        }
    }
    let file_name =
        path.file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| IntegrationError::InvalidPath {
                field: "integration destination",
                path: path.to_owned(),
                reason: "the filename is not valid UTF-8",
            })?;
    let mut temporary = None;
    for _attempt in 0..32 {
        let sequence = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary_path = parent.join(format!(
            ".{file_name}.tmp-{}-{sequence}",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary_path)
        {
            Ok(file) => {
                temporary = Some((temporary_path, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(IntegrationError::FileSystem {
                    path: path.to_owned(),
                    source,
                })
            }
        }
    }
    let (temporary_path, mut file) = temporary.ok_or_else(|| IntegrationError::FileSystem {
        path: path.to_owned(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no temporary filename available",
        ),
    })?;
    let result = (|| -> io::Result<()> {
        file.write_all(contents)?;
        file.flush()?;
        file.sync_all()?;
        fs::set_permissions(&temporary_path, fs::Permissions::from_mode(0o600))?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "destination became a symlink",
                ));
            }
            Ok(metadata) if metadata.is_file() => {
                let existing = fs::read_to_string(path)?;
                if !existing_is_owned(&existing) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "destination became an unowned file",
                    ));
                }
            }
            Ok(_metadata) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "destination became a non-file",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::rename(&temporary_path, path)?;
        File::open(parent)?.sync_all()
    })();
    if let Err(source) = result {
        let _cleanup = fs::remove_file(&temporary_path);
        return Err(IntegrationError::FileSystem {
            path: path.to_owned(),
            source,
        });
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), IntegrationError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_metadata) => Err(IntegrationError::FileSystem {
            path: path.to_owned(),
            source: io::Error::new(io::ErrorKind::AlreadyExists, "path is not a directory"),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(path)
                .map_err(|source| IntegrationError::FileSystem {
                    path: path.to_owned(),
                    source,
                })
        }
        Err(source) => Err(IntegrationError::FileSystem {
            path: path.to_owned(),
            source,
        }),
    }
}

fn remove_regular_owned(
    path: &Path,
    is_owned: impl FnOnce(&str) -> bool,
) -> Result<(), IntegrationError> {
    let parent = path.parent().ok_or_else(|| IntegrationError::InvalidPath {
        field: "integration destination",
        path: path.to_owned(),
        reason: "the path has no parent directory",
    })?;
    validate_user_root(parent, "integration destination")?;
    let contents = read_optional_regular(path)?.ok_or_else(|| IntegrationError::FileSystem {
        path: path.to_owned(),
        source: io::Error::new(io::ErrorKind::NotFound, "owned file disappeared"),
    })?;
    if !is_owned(&contents) {
        return Err(IntegrationError::UnownedFile {
            path: path.to_owned(),
        });
    }
    fs::remove_file(path).map_err(|source| IntegrationError::FileSystem {
        path: path.to_owned(),
        source,
    })
}

fn command_diagnostic(result: &CommandResult) -> String {
    let stderr = String::from_utf8_lossy(&result.stderr);
    let stdout = String::from_utf8_lossy(&result.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        "command returned a non-zero status".to_owned()
    } else {
        detail.to_owned()
    }
}

fn parse_manager_environment(stdout: &[u8]) -> ManagerEnvironmentStatus {
    let mut environment = ManagerEnvironmentStatus::default();
    for line in String::from_utf8_lossy(stdout).lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        match key {
            "DISPLAY" => environment.display = true,
            "WAYLAND_DISPLAY" => environment.wayland_display = true,
            "XDG_CURRENT_DESKTOP" => environment.current_desktop = true,
            "DBUS_SESSION_BUS_ADDRESS" => environment.session_bus = true,
            _other => {}
        }
    }
    environment
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    use super::{
        assess_autostart, AutostartState, CommandResult, CommandRunner, IntegrationError,
        IntegrationPaths, StartupManager, USER_UNIT_NAME,
    };
    use crate::registry::{AppRegistry, RegisteredApplication};

    #[derive(Default)]
    struct FakeRunner {
        calls: RefCell<Vec<(PathBuf, Vec<std::ffi::OsString>)>>,
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            program: &Path,
            arguments: &[std::ffi::OsString],
        ) -> std::io::Result<CommandResult> {
            self.calls
                .borrow_mut()
                .push((program.to_owned(), arguments.to_vec()));
            Ok(CommandResult {
                success: true,
                stdout: b"enabled\n".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    fn temporary(test: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ecore-launcher-integration-{}-{test}",
            std::process::id()
        ));
        let _ignored = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap_or_else(|error| panic!("create temp path: {error}"));
        path
    }

    fn registry() -> AppRegistry {
        let mut registry = AppRegistry::default();
        registry.apps.push(RegisteredApplication {
            desktop_id: "managed.desktop".to_owned(),
            name: "Managed".to_owned(),
            enabled: true,
            ..RegisteredApplication::default()
        });
        registry
    }

    fn manager(root: &Path) -> StartupManager<FakeRunner> {
        StartupManager::new(
            IntegrationPaths {
                config_home: root.join("user"),
                config_dirs: vec![root.join("system")],
                state_home: root.join("state"),
            },
            PathBuf::from("/opt/$HOME/E Core/bin/ecore-launcher"),
            PathBuf::from("/home/user/config with space.toml"),
            PathBuf::from("/usr/bin/systemctl"),
            FakeRunner::default(),
        )
        .unwrap_or_else(|error| panic!("create manager: {error}"))
    }

    #[test]
    fn generated_unit_is_deterministic_safe_and_preserves_launched_apps() {
        let root = temporary("unit");
        let manager = manager(&root);
        let first = manager
            .unit_contents()
            .unwrap_or_else(|error| panic!("render unit: {error}"));
        assert_eq!(first, manager.unit_contents().unwrap());
        assert!(first.contains("ExecStart=:\"/opt/$HOME/E Core/bin/ecore-launcher\" --config \"/home/user/config with space.toml\" supervise"));
        assert!(first.contains("WantedBy=graphical-session.target"));
        assert!(first.contains("KillMode=process"));
        assert!(!first.contains("/etc/systemd/system"));
    }

    #[test]
    fn enable_and_disable_use_direct_user_systemctl_arguments() {
        let root = temporary("systemctl");
        let manager = manager(&root);
        manager
            .enable(&registry(), false)
            .unwrap_or_else(|error| panic!("enable: {error}"));
        let calls = manager.runner.calls.borrow();
        assert_eq!(calls[0].1, ["--user", "daemon-reload"]);
        assert_eq!(calls[1].1, ["--user", "enable", USER_UNIT_NAME]);
        drop(calls);
        manager
            .disable()
            .unwrap_or_else(|error| panic!("disable: {error}"));
        let calls = manager.runner.calls.borrow();
        assert_eq!(calls[2].1, ["--user", "disable", USER_UNIT_NAME]);
        assert_eq!(calls[3].1, ["--user", "daemon-reload"]);
    }

    #[test]
    fn ownership_state_allows_safe_disable_after_unit_disappears() {
        let root = temporary("missing-unit-disable");
        let manager = manager(&root);
        manager.enable(&registry(), false).unwrap();
        fs::remove_file(manager.paths.unit_path()).unwrap();
        let change = manager.disable().unwrap();
        assert!(!change.unit_changed);
        assert!(!manager.paths.ownership_path().exists());
        let calls = manager.runner.calls.borrow();
        assert_eq!(calls[2].1, ["--user", "disable", USER_UNIT_NAME]);
        assert_eq!(calls[3].1, ["--user", "daemon-reload"]);
    }

    #[test]
    fn explicit_suppression_is_owned_reversible_and_never_overwrites_user_files() {
        let root = temporary("autostart");
        let manager = manager(&root);
        let system = root.join("system/autostart/managed.desktop");
        fs::create_dir_all(system.parent().unwrap()).unwrap();
        fs::write(&system, "[Desktop Entry]\nType=Application\n").unwrap();
        let assessments = assess_autostart(&manager.paths, &registry()).unwrap();
        assert_eq!(assessments[0].state, AutostartState::DuplicateRisk);
        let change = manager.enable(&registry(), true).unwrap();
        assert_eq!(change.autostart_overrides_changed, ["managed.desktop"]);
        let override_path = root.join("user/autostart/managed.desktop");
        let contents = fs::read_to_string(&override_path).unwrap();
        assert!(contents.contains("Hidden=true"));
        assert!(contents.contains("X-Ecore-Launcher-Owned=true"));
        manager.disable().unwrap();
        assert!(!override_path.exists());

        fs::create_dir_all(override_path.parent().unwrap()).unwrap();
        fs::write(&override_path, "user content\n").unwrap();
        assert!(matches!(
            manager.enable(&registry(), true),
            Err(IntegrationError::UnownedFile { .. })
        ));
        assert_eq!(
            fs::read_to_string(&override_path).unwrap(),
            "user content\n"
        );
        manager.disable().unwrap();
        assert_eq!(
            fs::read_to_string(&override_path).unwrap(),
            "user content\n"
        );
    }

    #[test]
    fn status_is_read_only_and_uses_only_is_enabled() {
        let root = temporary("status");
        let manager = manager(&root);
        let status = manager.status(&registry()).unwrap();
        assert!(!status.unit_present);
        assert!(!status.ownership_present);
        assert!(!root.join("user").exists());
        assert!(!root.join("state").exists());
        let calls = manager.runner.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, ["--user", "is-enabled", USER_UNIT_NAME]);
        assert_eq!(calls[1].1, ["--user", "show-environment"]);
    }

    #[test]
    fn unrelated_user_autostart_file_without_system_entry_is_not_a_conflict() {
        let root = temporary("unrelated-user-autostart");
        let manager = manager(&root);
        let override_path = root.join("user/autostart/managed.desktop");
        fs::create_dir_all(override_path.parent().unwrap()).unwrap();
        fs::write(&override_path, "user content\n").unwrap();
        let change = manager.enable(&registry(), true).unwrap();
        assert!(change.autostart_overrides_changed.is_empty());
        assert_eq!(
            fs::read_to_string(&override_path).unwrap(),
            "user content\n"
        );
        manager.disable().unwrap();
        assert_eq!(
            fs::read_to_string(&override_path).unwrap(),
            "user content\n"
        );
    }

    #[test]
    fn disable_ignores_unrelated_autostart_symlinks() {
        let root = temporary("unrelated-autostart-symlink");
        let manager = manager(&root);
        let target = root.join("user-created.desktop");
        fs::write(&target, "[Desktop Entry]\nType=Application\n").unwrap();
        let link = root.join("user/autostart/unrelated.desktop");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&target, &link).unwrap();

        let change = manager.disable().unwrap();

        assert!(change.autostart_overrides_changed.is_empty());
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "[Desktop Entry]\nType=Application\n"
        );
    }

    #[test]
    fn intermediate_symlink_into_system_state_is_rejected_before_mutation() {
        let root = temporary("system-parent-symlink");
        let manager = manager(&root);
        fs::create_dir_all(root.join("user")).unwrap();
        symlink("/etc/systemd", root.join("user/systemd")).unwrap();

        assert!(matches!(
            manager.enable(&registry(), false),
            Err(IntegrationError::UnsafeUserRoot { .. })
        ));
        assert!(manager.runner.calls.borrow().is_empty());
        assert!(!manager.paths.ownership_path().exists());
    }

    #[test]
    fn traversal_desktop_id_and_control_character_paths_are_rejected() {
        let mut invalid = registry();
        invalid.apps[0].desktop_id = "../escape.desktop".to_owned();
        let paths = IntegrationPaths {
            config_home: PathBuf::from("/tmp/user"),
            config_dirs: vec![PathBuf::from("/tmp/system")],
            state_home: PathBuf::from("/tmp/state"),
        };
        assert!(matches!(
            assess_autostart(&paths, &invalid),
            Err(IntegrationError::InvalidDesktopId { .. })
        ));
        assert!(StartupManager::new(
            paths,
            PathBuf::from("/bad\npath"),
            PathBuf::from("/config"),
            PathBuf::from("systemctl"),
            FakeRunner::default(),
        )
        .is_err());
        assert!(matches!(
            IntegrationPaths::from_environment(
                Some(Path::new("/etc")),
                &[],
                Some(Path::new("/tmp"))
            ),
            Err(IntegrationError::UnsafeUserRoot { .. })
        ));
    }
}
