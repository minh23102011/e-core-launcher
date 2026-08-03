use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// An installed desktop application which is safe to launch without a shell.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredApplication {
    /// Stable desktop-file ID, derived from the path below an `applications` directory.
    pub desktop_id: String,
    /// Locale-resolved display name.
    pub name: String,
    /// Locale-resolved generic name, when supplied.
    pub generic_name: Option<String>,
    /// Resolved executable path.
    pub executable: PathBuf,
    /// Static arguments after safe desktop field-code processing.
    pub arguments: Vec<String>,
    /// Icon name or path from the desktop entry.
    pub icon: Option<String>,
    /// Desktop file which produced this application.
    pub desktop_file: PathBuf,
    /// Whether the desktop entry requests a terminal.
    pub terminal: bool,
    /// Declared application categories, in source order with empty values removed.
    pub categories: Vec<String>,
    /// Window-manager startup class, when supplied.
    pub startup_wm_class: Option<String>,
    /// Zero-based source precedence; smaller values have higher priority.
    pub source_priority: usize,
    /// Whether the entry is normally omitted from desktop menus.
    pub no_display: bool,
}

/// Broad category for a non-fatal discovery diagnostic.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryWarningCategory {
    /// A directory or file could not be read.
    Io,
    /// A desktop file was not valid UTF-8.
    InvalidUtf8,
    /// Desktop-entry syntax or a supported value was malformed.
    DesktopEntry,
    /// The `Exec` command could not be parsed safely.
    Exec,
    /// The launch executable could not be resolved.
    Executable,
    /// `TryExec` was invalid or unavailable.
    TryExec,
    /// The entry was excluded by visibility or desktop-environment policy.
    Visibility,
    /// A higher-priority desktop-file ID replaced this entry.
    Overridden,
    /// An equivalent launch target was already present.
    Duplicate,
}

/// Severity of a discovery diagnostic.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryWarningSeverity {
    /// Expected filtering or override information.
    Info,
    /// Invalid or unavailable input which may merit investigation.
    Warning,
}

/// A deterministic, structured non-fatal discovery diagnostic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryWarning {
    /// Desktop file or discovery directory associated with the warning.
    pub path: PathBuf,
    /// Machine-readable warning category.
    pub category: DiscoveryWarningCategory,
    /// Diagnostic severity.
    pub severity: DiscoveryWarningSeverity,
    /// Human-readable reason.
    pub reason: String,
    /// Whether this condition prevented an entry from being returned.
    pub skipped: bool,
}

impl DiscoveryWarning {
    pub(crate) fn skipped(
        path: PathBuf,
        category: DiscoveryWarningCategory,
        severity: DiscoveryWarningSeverity,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            path,
            category,
            severity,
            reason: reason.into(),
            skipped: true,
        }
    }
}

/// Applications and non-fatal warnings produced by one discovery scan.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryReport {
    /// Applications sorted by stable desktop-file ID.
    pub applications: Vec<DiscoveredApplication>,
    /// Diagnostics sorted by path, category, severity, and reason.
    pub warnings: Vec<DiscoveryWarning>,
}
