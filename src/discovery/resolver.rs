use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Failure to turn a desktop-entry executable name into a usable file.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ExecutableResolutionError {
    /// The executable value was empty.
    #[error("executable is empty")]
    Empty,
    /// The executable cannot be represented by a process argument.
    #[error("executable contains a NUL byte")]
    ContainsNul,
    /// The path exists but does not name a regular executable file.
    #[error("{path} is not a regular executable file")]
    NotExecutable { path: PathBuf },
    /// No executable with this name was found on the configured search path.
    #[error("executable `{executable}` was not found on the configured PATH")]
    NotFound { executable: String },
}

/// Pure-Rust executable lookup with an injectable `PATH`.
#[derive(Clone, Debug, Default)]
pub struct ExecutableResolver {
    search_path: Vec<PathBuf>,
}

impl ExecutableResolver {
    /// Construct a resolver from explicit search directories.
    #[must_use]
    pub fn new(search_path: Vec<PathBuf>) -> Self {
        Self { search_path }
    }

    /// Construct a resolver from the process `PATH`.
    #[must_use]
    pub fn from_environment() -> Self {
        let search_path = env::var_os("PATH")
            .map(|value| env::split_paths(&value).collect())
            .unwrap_or_default();
        Self::new(search_path)
    }

    /// Return the configured search path.
    #[must_use]
    pub fn search_path(&self) -> &[PathBuf] {
        &self.search_path
    }

    /// Resolve `executable` without invoking `which` or a shell.
    ///
    /// Values containing `/` are checked directly. Bare names are searched in
    /// configured `PATH` order. Symlinks are followed for metadata checks but
    /// the returned path is not canonicalized.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutableResolutionError`] when no regular file with at
    /// least one execute bit can be found.
    pub fn resolve(&self, executable: &str) -> Result<PathBuf, ExecutableResolutionError> {
        if executable.is_empty() {
            return Err(ExecutableResolutionError::Empty);
        }
        if executable.contains('\0') {
            return Err(ExecutableResolutionError::ContainsNul);
        }

        if executable.contains('/') {
            let path = PathBuf::from(executable);
            return check_candidate(&path).then_some(path).ok_or_else(|| {
                ExecutableResolutionError::NotExecutable {
                    path: PathBuf::from(executable),
                }
            });
        }

        for directory in &self.search_path {
            let candidate = directory.join(executable);
            if check_candidate(&candidate) {
                return Ok(candidate);
            }
        }
        Err(ExecutableResolutionError::NotFound {
            executable: executable.to_owned(),
        })
    }
}

fn check_candidate(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{ExecutableResolutionError, ExecutableResolver};

    fn temp_directory(test: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "ecore-launcher-resolver-{}-{test}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap_or_else(|error| panic!("create temp directory: {error}"));
        path
    }

    fn executable(path: &std::path::Path) {
        File::create(path).unwrap_or_else(|error| panic!("create executable: {error}"));
        let mut permissions = fs::metadata(path)
            .unwrap_or_else(|error| panic!("read executable metadata: {error}"))
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)
            .unwrap_or_else(|error| panic!("set executable permissions: {error}"));
    }

    #[test]
    fn resolves_path_names_and_symlinks_without_canonicalizing() {
        let root = temp_directory("path");
        let program = root.join("program");
        let link = root.join("program-link");
        executable(&program);
        symlink(&program, &link).unwrap_or_else(|error| panic!("create symlink: {error}"));
        let resolver = ExecutableResolver::new(vec![root.clone()]);

        assert_eq!(resolver.resolve("program"), Ok(program));
        assert_eq!(resolver.resolve("program-link"), Ok(link));
        fs::remove_dir_all(root).unwrap_or_else(|error| panic!("remove temp directory: {error}"));
    }

    #[test]
    fn resolves_absolute_paths_and_rejects_non_executable_files() {
        let root = temp_directory("absolute");
        let program = root.join("absolute-program");
        executable(&program);
        let plain = root.join("plain");
        File::create(&plain).unwrap_or_else(|error| panic!("create plain file: {error}"));
        let resolver = ExecutableResolver::default();

        assert_eq!(
            resolver.resolve(program.to_string_lossy().as_ref()),
            Ok(program)
        );
        assert!(matches!(
            resolver.resolve(plain.to_string_lossy().as_ref()),
            Err(ExecutableResolutionError::NotExecutable { .. })
        ));
        fs::remove_dir_all(root).unwrap_or_else(|error| panic!("remove temp directory: {error}"));
    }

    #[test]
    fn missing_bare_name_is_reported() {
        let resolver = ExecutableResolver::new(Vec::new());
        assert_eq!(
            resolver.resolve("missing"),
            Err(ExecutableResolutionError::NotFound {
                executable: "missing".to_owned()
            })
        );
    }

    #[test]
    fn path_order_skips_non_executable_candidates() {
        let first = temp_directory("path-first");
        let second = temp_directory("path-second");
        let first_program = first.join("program");
        File::create(&first_program)
            .unwrap_or_else(|error| panic!("create non-executable candidate: {error}"));
        let second_program = second.join("program");
        executable(&second_program);
        let resolver = ExecutableResolver::new(vec![first.clone(), second.clone()]);

        assert_eq!(resolver.resolve("program"), Ok(second_program));
        fs::remove_dir_all(first)
            .unwrap_or_else(|error| panic!("remove first temp directory: {error}"));
        fs::remove_dir_all(second)
            .unwrap_or_else(|error| panic!("remove second temp directory: {error}"));
    }
}
