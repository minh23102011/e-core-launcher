use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The only TOML schema version supported by this release.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Maximum number of explicitly managed applications in one registry.
pub const MAX_REGISTERED_APPLICATIONS: usize = 10_000;

/// Default delay in seconds before the launcher starts an application.
pub const DEFAULT_DELAY_SECONDS: u64 = 0;
/// Default nice value applied by the runtime helper.
pub const DEFAULT_NICE: i8 = 5;
/// Default I/O priority within the `best-effort` class.
pub const DEFAULT_IO_PRIORITY: u8 = 4;

/// I/O scheduling class applied by the runtime helper.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IoPriorityClass {
    /// Do not request an I/O priority change.
    None,
    /// Real-time I/O policy.
    Realtime,
    /// Best-effort I/O policy.
    BestEffort,
    /// Idle I/O policy.
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
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LauncherDefaults {
    /// Default startup delay in seconds, bounded by validation.
    pub default_delay_seconds: u64,
    /// Default Linux nice value.
    pub default_nice: i8,
    /// Default I/O class.
    pub default_io_class: IoPriorityClass,
    /// Default I/O priority when the selected class accepts one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_io_priority: Option<u8>,
    /// Default process-tree enforcement preference for supervised launches.
    pub default_enforce_process_tree: bool,
    /// Unknown launcher keys retained across canonical rewrites.
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, toml::Value>,
}

#[derive(Deserialize)]
#[serde(default)]
struct LauncherDefaultsWire {
    default_delay_seconds: u64,
    default_nice: i8,
    default_io_class: IoPriorityClass,
    default_io_priority: Option<u8>,
    default_enforce_process_tree: bool,
    #[serde(flatten)]
    extra: BTreeMap<String, toml::Value>,
}

impl Default for LauncherDefaultsWire {
    fn default() -> Self {
        let defaults = LauncherDefaults::default();
        Self {
            default_delay_seconds: defaults.default_delay_seconds,
            default_nice: defaults.default_nice,
            default_io_class: defaults.default_io_class,
            default_io_priority: None,
            default_enforce_process_tree: defaults.default_enforce_process_tree,
            extra: BTreeMap::new(),
        }
    }
}

impl<'de> Deserialize<'de> for LauncherDefaults {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let wire = LauncherDefaultsWire::deserialize(deserializer)?;
        let default_io_priority = wire.default_io_priority.or_else(|| {
            matches!(
                wire.default_io_class,
                IoPriorityClass::BestEffort | IoPriorityClass::Realtime
            )
            .then_some(DEFAULT_IO_PRIORITY)
        });
        Ok(Self {
            default_delay_seconds: wire.default_delay_seconds,
            default_nice: wire.default_nice,
            default_io_class: wire.default_io_class,
            default_io_priority,
            default_enforce_process_tree: wire.default_enforce_process_tree,
            extra: wire.extra,
        })
    }
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
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RegisteredApplication {
    /// Stable desktop-file ID. It is an identity, not a filesystem path.
    pub desktop_id: String,
    /// Display name captured when the user selected the application.
    pub name: String,
    /// Whether the launcher may act on this selected application.
    pub enabled: bool,
    /// Snapshot startup delay in seconds.
    pub delay_seconds: u64,
    /// Snapshot Linux nice value.
    pub nice: i8,
    /// Snapshot I/O class.
    pub io_class: IoPriorityClass,
    /// Snapshot I/O priority when the selected class accepts one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_priority: Option<u8>,
    /// Snapshot process-tree enforcement preference for supervised launches.
    pub enforce_process_tree: bool,
    /// Desktop-entry path captured for diagnostics only; it is never launch authority.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop_file: Option<PathBuf>,
    /// Unknown application keys retained across canonical rewrites.
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, toml::Value>,
}

#[derive(Deserialize)]
#[serde(default)]
struct RegisteredApplicationWire {
    desktop_id: String,
    name: String,
    enabled: bool,
    delay_seconds: u64,
    nice: i8,
    io_class: IoPriorityClass,
    io_priority: Option<u8>,
    enforce_process_tree: bool,
    desktop_file: Option<PathBuf>,
    #[serde(flatten)]
    extra: BTreeMap<String, toml::Value>,
}

impl Default for RegisteredApplicationWire {
    fn default() -> Self {
        let defaults = RegisteredApplication::default();
        Self {
            desktop_id: defaults.desktop_id,
            name: defaults.name,
            enabled: defaults.enabled,
            delay_seconds: defaults.delay_seconds,
            nice: defaults.nice,
            io_class: defaults.io_class,
            io_priority: None,
            enforce_process_tree: defaults.enforce_process_tree,
            desktop_file: defaults.desktop_file,
            extra: BTreeMap::new(),
        }
    }
}

impl<'de> Deserialize<'de> for RegisteredApplication {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let wire = RegisteredApplicationWire::deserialize(deserializer)?;
        let io_priority = wire.io_priority.or_else(|| {
            matches!(
                wire.io_class,
                IoPriorityClass::BestEffort | IoPriorityClass::Realtime
            )
            .then_some(DEFAULT_IO_PRIORITY)
        });
        Ok(Self {
            desktop_id: wire.desktop_id,
            name: wire.name,
            enabled: wire.enabled,
            delay_seconds: wire.delay_seconds,
            nice: wire.nice,
            io_class: wire.io_class,
            io_priority,
            enforce_process_tree: wire.enforce_process_tree,
            desktop_file: wire.desktop_file,
            extra: wire.extra,
        })
    }
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
