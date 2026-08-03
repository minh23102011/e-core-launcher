use std::env;
use std::path::{Path, PathBuf};

use super::error::RegistryError;

/// Resolve an explicit or XDG configuration file path without creating it.
///
/// An explicit non-empty path wins. Otherwise the result is
/// `$XDG_CONFIG_HOME/ecore-launcher/config.toml`, falling back to
/// `$HOME/.config/ecore-launcher/config.toml`. Relative explicit paths are
/// accepted for diagnostics; environment-derived roots must be absolute.
pub fn resolve_config_path(explicit: Option<&Path>) -> Result<PathBuf, RegistryError> {
    resolve_config_path_from(
        explicit,
        env::var_os("XDG_CONFIG_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
    )
}

fn resolve_config_path_from(
    explicit: Option<&Path>,
    xdg_config_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, RegistryError> {
    if let Some(path) = explicit {
        if path.as_os_str().is_empty() {
            return Err(RegistryError::EmptyConfigPath);
        }
        return Ok(path.to_owned());
    }

    if let Some(root) = xdg_config_home
        .map(PathBuf::from)
        .filter(|root| !root.as_os_str().is_empty() && root.is_absolute())
    {
        return Ok(root.join("ecore-launcher/config.toml"));
    }
    if let Some(home) = home
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty() && home.is_absolute())
    {
        return Ok(home.join(".config/ecore-launcher/config.toml"));
    }
    Err(RegistryError::ConfigPathUnavailable)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    use super::resolve_config_path_from;
    use crate::registry::RegistryError;

    #[test]
    fn resolves_explicit_xdg_and_home_paths_without_environment_mutation() {
        assert_eq!(
            resolve_config_path_from(
                Some(Path::new("relative-config.toml")),
                Some(OsStr::new("/ignored")),
                Some(OsStr::new("/ignored"))
            )
            .unwrap_or_else(|error| panic!("resolve explicit path: {error}")),
            PathBuf::from("relative-config.toml")
        );
        assert_eq!(
            resolve_config_path_from(None, Some(OsStr::new("/xdg")), Some(OsStr::new("/home")))
                .unwrap_or_else(|error| panic!("resolve XDG path: {error}")),
            PathBuf::from("/xdg/ecore-launcher/config.toml")
        );
        assert_eq!(
            resolve_config_path_from(None, Some(OsStr::new("")), Some(OsStr::new("/home")))
                .unwrap_or_else(|error| panic!("resolve home path: {error}")),
            PathBuf::from("/home/.config/ecore-launcher/config.toml")
        );
        assert!(matches!(
            resolve_config_path_from(None, None, None),
            Err(RegistryError::ConfigPathUnavailable)
        ));
        assert!(matches!(
            resolve_config_path_from(Some(Path::new("")), None, None),
            Err(RegistryError::EmptyConfigPath)
        ));
    }
}
