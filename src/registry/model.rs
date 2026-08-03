use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The only TOML schema version supported by this release.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Maximum number of explicitly managed applications in one registry.
pub const MAX_REGISTERED_APPLICATIONS: usize = 10_000;

/// Default delay in seconds before a future launcher starts an application.
pub const DEFAULT_DELAY_SECONDS: u64 = 0;
/// Default nice value stored for a future launcher phase.
pub const DEFAULT_NICE: i8 = 5;
/// Default I/O priority within the `best-effort` class.
pub const DEFAULT_IO_PRIORITY: u8 = 4;

/// I/O scheduling class stored as a preference for a later launcher phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IoPriorityClass {
    /// Do not request an I/O priority change.
    None,
    /// Future real-time I/O preference.
    Realtime,
    /// Future best-effort I/O preference.
    BestEffort,
    /// Future idle I/O preference.
    Idle,
}

impl std::fmt::Display for IoPriorityClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::None => "none",
            Self::Realtime => "realtime",
            Self::BestEffort => "best-effort",
            Self::Idle => "idle",
        };
        formatter.write_str(value)
    }
}

/// Default stored preferences copied into an application when it is added.
///
/// This registry intentionally uses a snapshot model: later changes to these
/// defaults do not alter applications which have already been registered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LauncherDefaults {
    /// Default startup delay in seconds, bounded by validation.
    pub default_delay_seconds: u64,
    /// Default Linux nice value for a later phase.
    pub default_nice: i8,
    /// Default I/O class for a later phase.
    pub default_io_class: IoPriorityClass,
    /// Default I/O priority when the selected class accepts one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_io_priority: Option<u8>,
    /// Default process-tree preference for a later phase.
    pub default_enforce_process_tree: bool,
    /// Unknown launcher keys retained across canonical rewrites.
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, toml::Value>,
}

impl Default for LauncherDefaults {
    fn default() -> Self {
        Self {
            default_delay_seconds: DEFAULT_DELAY_SECONDS,
            default_nice: DEFAULT_NICE,
            default_io_class: IoPriorityClass::BestEffort,
            default_io_priority: Some(DEFAULT_IO_PRIORITY),
            default_enforce_process_tree: false,
            extra: BTreeMap::new(),
        }
    }
}

/// One application that the user explicitly elected to manage later.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RegisteredApplication {
    /// Stable desktop-file ID. It is an identity, not a filesystem path.
    pub desktop_id: String,
    /// Display name captured when the user selected the application.
    pub name: String,
    /// Whether a future launcher phase may act on this selected application.
    pub enabled: bool,
    /// Snapshot startup delay in seconds for a future phase.
    pub delay_seconds: u64,
    /// Snapshot Linux nice value for a future phase.
    pub nice: i8,
    /// Snapshot I/O class for a future phase.
    pub io_class: IoPriorityClass,
    /// Snapshot I/O priority when the selected class accepts one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_priority: Option<u8>,
    /// Snapshot process-tree preference for a future phase.
    pub enforce_process_tree: bool,
    /// Desktop-entry path captured for diagnostics only; it is never launch authority.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop_file: Option<PathBuf>,
    /// Unknown application keys retained across canonical rewrites.
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, toml::Value>,
}

impl Default for RegisteredApplication {
    fn default() -> Self {
        Self {
            desktop_id: String::new(),
            name: String::new(),
            enabled: true,
            delay_seconds: DEFAULT_DELAY_SECONDS,
            nice: DEFAULT_NICE,
            io_class: IoPriorityClass::BestEffort,
            io_priority: Some(DEFAULT_IO_PRIORITY),
            enforce_process_tree: false,
            desktop_file: None,
            extra: BTreeMap::new(),
        }
    }
}

/// Versioned, user-controlled application registry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppRegistry {
    /// Version required to interpret this TOML file.
    pub schema_version: u32,
    /// Defaults copied when a new application is added.
    #[serde(default)]
    pub launcher: LauncherDefaults,
    /// Explicitly managed applications, kept sorted by desktop ID.
    #[serde(default)]
    pub apps: Vec<RegisteredApplication>,
    /// Unknown top-level keys retained across canonical rewrites.
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, toml::Value>,
}

impl Default for AppRegistry {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            launcher: LauncherDefaults::default(),
            apps: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

impl AppRegistry {
    pub(crate) fn normalize(&mut self) {
        self.apps
            .sort_by(|left, right| left.desktop_id.cmp(&right.desktop_id));
    }
}

/// Changes to one registered application's stored policy.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ApplicationSettingsUpdate {
    /// Replacement delay, when supplied.
    pub delay_seconds: Option<u64>,
    /// Replacement nice value, when supplied.
    pub nice: Option<i8>,
    /// Replacement I/O class, when supplied.
    pub io_class: Option<IoPriorityClass>,
    /// Replacement I/O priority. `Some(None)` clears it.
    pub io_priority: Option<Option<u8>>,
    /// Replacement process-tree preference, when supplied.
    pub enforce_process_tree: Option<bool>,
    /// Restore all policy fields from current launcher defaults.
    pub reset_to_defaults: bool,
}

pub(crate) fn normalize_priority(priority: &mut Option<u8>, class: IoPriorityClass) {
    if matches!(class, IoPriorityClass::None | IoPriorityClass::Idle) {
        *priority = None;
    } else if priority.is_none() {
        *priority = Some(DEFAULT_IO_PRIORITY);
    }
}
