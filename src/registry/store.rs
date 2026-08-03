use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fs2::FileExt;

use super::error::RegistryError;
use super::model::{AppRegistry, CURRENT_SCHEMA_VERSION};
use super::validation::validate_registry;

static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Loaded registry data and whether an on-disk configuration file existed.
#[derive(Clone, Debug, PartialEq)]
pub struct RegistryLoad {
    /// Valid, normalized registry data.
    pub registry: AppRegistry,
    /// Whether `RegistryStore::load_with_status` read a file.
    pub exists: bool,
}

/// Versioned TOML registry store rooted at one configuration file.
#[derive(Clone, Debug)]
pub struct RegistryStore {
    path: PathBuf,
}

impl RegistryStore {
    /// Construct a store for an already-resolved configuration file path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Return the exact configuration file path without creating it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load a valid registry, treating a missing file as an empty default.
    pub fn load(&self) -> Result<AppRegistry, RegistryError> {
        Ok(self.load_with_status()?.registry)
    }

    /// Load a valid registry and distinguish a missing file from a stored one.
    pub fn load_with_status(&self) -> Result<RegistryLoad, RegistryError> {
        inspect_config_path(&self.path)?;
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(RegistryLoad {
                    registry: AppRegistry::default(),
                    exists: false,
                });
            }
            Err(source) => {
                return Err(RegistryError::ReadConfig {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let mut registry: AppRegistry =
            toml::from_str(&contents).map_err(|source| RegistryError::TomlSyntax {
                path: self.path.clone(),
                source,
            })?;
        if registry.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(RegistryError::UnsupportedSchemaVersion {
                found: registry.schema_version,
            });
        }
        registry.normalize();
        validate_registry(&registry)?;
        Ok(RegistryLoad {
            registry,
            exists: true,
        })
    }

    /// Atomically save a fully validated registry under an advisory mutation lock.
    pub fn save(&self, registry: &AppRegistry) -> Result<(), RegistryError> {
        self.ensure_parent_directory()?;
        let _lock = MutationLock::acquire(&self.lock_path())?;
        // Never let the convenience save API replace malformed or unsupported
        // user data. `mutate` already reloads while holding this same lock.
        self.load()?;
        self.save_locked(registry)
    }

    /// Reload, mutate, validate, and atomically save while holding one lock.
    ///
    /// The closure receives the latest persisted state, so sequential clients
    /// do not overwrite one another's completed mutations. A closure error or
    /// validation error leaves the prior configuration untouched.
    pub fn mutate<T>(
        &self,
        mutation: impl FnOnce(&mut AppRegistry) -> Result<T, RegistryError>,
    ) -> Result<T, RegistryError> {
        self.ensure_parent_directory()?;
        let _lock = MutationLock::acquire(&self.lock_path())?;
        let mut registry = self.load()?;
        let result = mutation(&mut registry)?;
        registry.normalize();
        validate_registry(&registry)?;
        self.save_locked(&registry)?;
        Ok(result)
    }

    fn save_locked(&self, registry: &AppRegistry) -> Result<(), RegistryError> {
        inspect_config_path(&self.path)?;
        let mut canonical = registry.clone();
        canonical.normalize();
        validate_registry(&canonical)?;
        let serialized = toml::to_string_pretty(&canonical)
            .map_err(|source| RegistryError::TomlSerialize { source })?;
        atomic_write(&self.path, serialized.as_bytes())
    }

    fn ensure_parent_directory(&self) -> Result<(), RegistryError> {
        let parent = self.path.parent().ok_or(RegistryError::EmptyConfigPath)?;
        if parent.as_os_str().is_empty() {
            return Ok(());
        }
        match fs::metadata(parent) {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_metadata) => Err(RegistryError::CreateConfigDirectory {
                path: parent.to_owned(),
                source: io::Error::new(io::ErrorKind::AlreadyExists, "path is not a directory"),
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = DirBuilder::new();
                builder.recursive(true).mode(0o700);
                builder
                    .create(parent)
                    .map_err(|source| RegistryError::CreateConfigDirectory {
                        path: parent.to_owned(),
                        source,
                    })
            }
            Err(source) => Err(RegistryError::CreateConfigDirectory {
                path: parent.to_owned(),
                source,
            }),
        }
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.path.clone().into_os_string();
        path.push(".lock");
        PathBuf::from(path)
    }
}

struct MutationLock {
    _file: File,
}

impl MutationLock {
    fn acquire(path: &Path) -> Result<Self, RegistryError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(RegistryError::SymlinkRejected {
                    path: path.to_owned(),
                });
            }
            Ok(_metadata) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(RegistryError::LockAcquire {
                    path: path.to_owned(),
                    source,
                });
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|source| RegistryError::LockAcquire {
                path: path.to_owned(),
                source,
            })?;
        file.lock_exclusive()
            .map_err(|source| RegistryError::LockAcquire {
                path: path.to_owned(),
                source,
            })?;
        Ok(Self { _file: file })
    }
}

fn inspect_config_path(path: &Path) -> Result<(), RegistryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(RegistryError::SymlinkRejected {
            path: path.to_owned(),
        }),
        Ok(metadata) if metadata.is_dir() => Err(RegistryError::ConfigPathIsDirectory {
            path: path.to_owned(),
        }),
        Ok(_metadata) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(RegistryError::ReadConfig {
            path: path.to_owned(),
            source,
        }),
    }
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), RegistryError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = create_temporary_file(parent, path)?;
    let temporary_path = temporary.path.clone();
    let destination = path.to_owned();
    let result = (|| -> io::Result<()> {
        let mut file = temporary.file;
        file.write_all(contents)?;
        file.flush()?;
        file.sync_all()?;
        fs::set_permissions(&temporary_path, fs::Permissions::from_mode(0o600))?;
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "destination became a symlink",
                ));
            }
            Ok(_metadata) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::rename(&temporary_path, &destination)?;
        let directory = File::open(parent)?;
        directory.sync_all()?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(()),
        Err(source) => {
            let source = match fs::remove_file(&temporary_path) {
                Ok(()) => source,
                Err(error) if error.kind() == io::ErrorKind::NotFound => source,
                Err(cleanup) => io::Error::new(
                    source.kind(),
                    format!("{source}; temporary cleanup also failed: {cleanup}"),
                ),
            };
            Err(RegistryError::AtomicWrite {
                path: destination,
                source,
            })
        }
    }
}

struct TemporaryFile {
    path: PathBuf,
    file: File,
}

fn create_temporary_file(
    parent: &Path,
    destination: &Path,
) -> Result<TemporaryFile, RegistryError> {
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(RegistryError::EmptyConfigPath)?;
    for _attempt in 0..32 {
        let counter = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{file_name}.tmp-{}-{counter}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok(TemporaryFile { path, file }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(RegistryError::AtomicWrite {
                    path: destination.to_owned(),
                    source,
                });
            }
        }
    }
    Err(RegistryError::AtomicWrite {
        path: destination.to_owned(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a unique temporary configuration file",
        ),
    })
}
