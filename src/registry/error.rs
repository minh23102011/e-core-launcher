use std::io;
use std::path::PathBuf;

use thiserror::Error;

use super::model::CURRENT_SCHEMA_VERSION;

/// One semantic validation issue in a registry file or requested mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationError {
    /// Stable TOML-like field path.
    pub field: String,
    /// Human-readable explanation.
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.field, self.message)
    }
}

/// Registry path, load, validation, lock, persistence, and operation errors.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// No usable explicit path, XDG config root, or home directory was available.
    #[error("cannot resolve configuration path: provide --config or set XDG_CONFIG_HOME or HOME")]
    ConfigPathUnavailable,

    /// The caller supplied an empty config path.
    #[error("configuration path is empty")]
    EmptyConfigPath,

    /// A configuration path intended as a file was an existing directory.
    #[error("configuration path {path} is a directory")]
    ConfigPathIsDirectory { path: PathBuf },

    /// A configuration path or lock path was a symlink and was rejected.
    #[error("refusing symlinked configuration path {path}")]
    SymlinkRejected { path: PathBuf },

    /// The existing configuration file could not be read.
    #[error("failed to read configuration file {path}: {source}")]
    ReadConfig {
        /// File path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },

    /// The existing configuration file was not valid TOML.
    #[error("configuration file {path} contains invalid TOML: {source}")]
    TomlSyntax {
        /// File path.
        path: PathBuf,
        /// TOML diagnostic.
        #[source]
        source: toml::de::Error,
    },

    /// The file used a schema this build cannot interpret.
    #[error("configuration schema version {found} is unsupported; this build supports {CURRENT_SCHEMA_VERSION}")]
    UnsupportedSchemaVersion {
        /// Version declared by the file.
        found: u32,
    },

    /// The decoded registry failed one or more semantic checks.
    #[error("registry validation failed: {0}")]
    Validation(ValidationError),

    /// The caller requested an unavailable desktop application ID.
    #[error("desktop application `{desktop_id}` is not currently discoverable")]
    UnknownDiscoveredApplication { desktop_id: String },

    /// The caller requested an ID which is not explicitly registered.
    #[error("desktop application `{desktop_id}` is not registered")]
    UnknownRegisteredApplication { desktop_id: String },

    /// The mutation lock could not be acquired.
    #[error("failed to acquire registry mutation lock {path}: {source}")]
    LockAcquire {
        /// Lock file path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },

    /// The configuration parent could not be created.
    #[error("failed to create configuration directory {path}: {source}")]
    CreateConfigDirectory {
        /// Parent path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },

    /// A temporary file could not be created or written.
    #[error("failed to atomically write configuration file {path}: {source}")]
    AtomicWrite {
        /// Destination path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },

    /// A TOML serialization error occurred before writing.
    #[error("failed to serialize registry TOML: {source}")]
    TomlSerialize {
        /// Serializer error.
        #[source]
        source: toml::ser::Error,
    },

    /// A terminal-only selection or confirmation was unavailable to a script.
    #[error(
        "interactive selection or confirmation requires a terminal; use explicit IDs or --yes"
    )]
    InteractiveInputUnavailable,

    /// The user canceled an interactive selection or confirmation.
    #[error("interactive operation canceled")]
    InteractiveCanceled,

    /// Terminal input or output failed during an interactive command.
    #[error("interactive {operation} failed: {source}")]
    InteractiveIo {
        /// `stdin` or `stdout` operation.
        operation: &'static str,
        /// Underlying terminal I/O error.
        #[source]
        source: io::Error,
    },

    /// User input did not name a valid selectable item.
    #[error("invalid interactive selection `{value}`")]
    InvalidInteractiveSelection { value: String },
}

impl From<ValidationError> for RegistryError {
    fn from(error: ValidationError) -> Self {
        Self::Validation(error)
    }
}
