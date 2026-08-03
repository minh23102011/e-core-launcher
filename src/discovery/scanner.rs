use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use super::desktop_entry::{parse_desktop_entry, DesktopEntry};
use super::error::DiscoveryError;
use super::exec_parser::parse_exec;
use super::resolver::ExecutableResolver;
use super::types::{
    DiscoveredApplication, DiscoveryReport, DiscoveryWarning, DiscoveryWarningCategory,
    DiscoveryWarningSeverity,
};

/// Inputs and filtering policy for desktop application discovery.
#[derive(Clone, Debug)]
pub struct DiscoveryOptions {
    /// XDG user data root. The scanner reads its `applications` child.
    pub data_home: Option<PathBuf>,
    /// Ordered XDG system data roots. Each `applications` child is scanned.
    pub data_dirs: Vec<PathBuf>,
    /// Search path used to resolve bare `Exec` and `TryExec` names.
    pub executable_path: Vec<PathBuf>,
    /// Active locale used for localized names.
    pub locale: Option<String>,
    /// Active desktop names, normally parsed from `XDG_CURRENT_DESKTOP`.
    pub current_desktops: Vec<String>,
    /// Include `NoDisplay=true` entries. `Hidden=true` overrides remain suppressed.
    pub include_no_display: bool,
    /// Ignore `OnlyShowIn` and `NotShowIn` filtering.
    pub ignore_desktop_filter: bool,
    /// Treat every configured `applications` directory as required input.
    pub require_existing_roots: bool,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self::from_environment()
    }
}

impl DiscoveryOptions {
    /// Build production defaults from the process XDG, locale, desktop, and
    /// `PATH` environment.
    #[must_use]
    pub fn from_environment() -> Self {
        Self {
            data_home: environment_data_home(),
            data_dirs: environment_data_dirs(),
            executable_path: env::var_os("PATH")
                .map(|value| env::split_paths(&value).collect())
                .unwrap_or_default(),
            locale: environment_locale(),
            current_desktops: env::var_os("XDG_CURRENT_DESKTOP")
                .map(|value| split_desktops(&value.to_string_lossy()))
                .unwrap_or_default(),
            include_no_display: false,
            ignore_desktop_filter: false,
            require_existing_roots: false,
        }
    }
}

/// Configurable scanner for installed Linux desktop applications.
#[derive(Clone, Debug)]
pub struct DesktopApplicationScanner {
    options: DiscoveryOptions,
}

impl Default for DesktopApplicationScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopApplicationScanner {
    /// Construct a scanner using process environment defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::from_options(DiscoveryOptions::default())
    }

    /// Construct a scanner with fully explicit options.
    #[must_use]
    pub fn from_options(options: DiscoveryOptions) -> Self {
        Self { options }
    }

    /// Replace the user data root.
    #[must_use]
    pub fn with_data_home(mut self, data_home: impl Into<PathBuf>) -> Self {
        self.options.data_home = Some(data_home.into());
        self
    }

    /// Remove the user data root from the scan.
    #[must_use]
    pub fn without_data_home(mut self) -> Self {
        self.options.data_home = None;
        self
    }

    /// Replace all system data roots, preserving the supplied order.
    #[must_use]
    pub fn with_data_dirs(mut self, data_dirs: Vec<PathBuf>) -> Self {
        self.options.data_dirs = data_dirs;
        self
    }

    /// Replace the executable search path.
    #[must_use]
    pub fn with_executable_path(mut self, executable_path: Vec<PathBuf>) -> Self {
        self.options.executable_path = executable_path;
        self
    }

    /// Return the active scanner options.
    #[must_use]
    pub fn options(&self) -> &DiscoveryOptions {
        &self.options
    }

    /// Scan configured `applications` directories in XDG precedence order.
    ///
    /// Individual files which are malformed, filtered, overridden, or
    /// unavailable become deterministic warnings. No desktop-entry content is
    /// executed.
    ///
    /// # Errors
    ///
    /// Returns [`DiscoveryError`] only for invalid configuration or an
    /// unavailable root which the caller marked as required.
    pub fn discover(&self) -> Result<DiscoveryReport, DiscoveryError> {
        let roots = self.discovery_roots()?;
        let resolver = ExecutableResolver::new(self.options.executable_path.clone());
        let mut warnings = Vec::new();
        let mut seen_ids = BTreeMap::<String, PathBuf>::new();
        let mut applications = Vec::new();

        for (priority, root) in roots {
            let desktop_files = self.desktop_files(&root, &mut warnings)?;
            for desktop_file in desktop_files {
                let Some(desktop_id) = desktop_file_id(&root, &desktop_file) else {
                    warnings.push(skipped_warning(
                        &desktop_file,
                        DiscoveryWarningCategory::DesktopEntry,
                        DiscoveryWarningSeverity::Warning,
                        "desktop-file path cannot be represented as a valid UTF-8 desktop ID",
                    ));
                    continue;
                };

                if let Some(higher_path) = seen_ids.get(&desktop_id) {
                    warnings.push(skipped_warning(
                        &desktop_file,
                        DiscoveryWarningCategory::Overridden,
                        DiscoveryWarningSeverity::Info,
                        format!(
                            "desktop ID {desktop_id} is overridden by higher-priority {}",
                            higher_path.display()
                        ),
                    ));
                    continue;
                }
                seen_ids.insert(desktop_id.clone(), desktop_file.clone());

                let Some(entry) = read_entry(&desktop_file, &mut warnings) else {
                    continue;
                };
                if let Some(application) = self.build_application(
                    desktop_id,
                    desktop_file,
                    priority,
                    entry,
                    &resolver,
                    &mut warnings,
                ) {
                    applications.push(application);
                }
            }
        }

        applications = deduplicate_launch_targets(applications, &mut warnings);
        applications.sort_by(|left, right| left.desktop_id.cmp(&right.desktop_id));
        sort_warnings(&mut warnings);
        Ok(DiscoveryReport {
            applications,
            warnings,
        })
    }

    fn discovery_roots(&self) -> Result<Vec<(usize, PathBuf)>, DiscoveryError> {
        let configured: Vec<PathBuf> = self
            .options
            .data_home
            .iter()
            .chain(self.options.data_dirs.iter())
            .cloned()
            .collect();
        if configured.is_empty() {
            return Err(DiscoveryError::NoDiscoveryRoots);
        }

        let mut unique = BTreeSet::new();
        let mut roots = Vec::new();
        for (priority, data_root) in configured.into_iter().enumerate() {
            if data_root.as_os_str().is_empty() {
                return Err(DiscoveryError::EmptyDiscoveryRoot { priority });
            }
            let applications = data_root.join("applications");
            if unique.insert(applications.clone()) {
                roots.push((priority, applications));
            }
        }
        Ok(roots)
    }

    fn desktop_files(
        &self,
        root: &Path,
        warnings: &mut Vec<DiscoveryWarning>,
    ) -> Result<Vec<PathBuf>, DiscoveryError> {
        match fs::metadata(root) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_metadata) => {
                let source = io::Error::new(io::ErrorKind::InvalidInput, "path is not a directory");
                if self.options.require_existing_roots {
                    return Err(DiscoveryError::RequiredRootUnavailable {
                        path: root.to_owned(),
                        source,
                    });
                }
                warnings.push(skipped_warning(
                    root,
                    DiscoveryWarningCategory::Io,
                    DiscoveryWarningSeverity::Warning,
                    "configured applications path is not a directory",
                ));
                return Ok(Vec::new());
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                if self.options.require_existing_roots {
                    return Err(DiscoveryError::RequiredRootUnavailable {
                        path: root.to_owned(),
                        source,
                    });
                }
                return Ok(Vec::new());
            }
            Err(source) => {
                if self.options.require_existing_roots {
                    return Err(DiscoveryError::RequiredRootUnavailable {
                        path: root.to_owned(),
                        source,
                    });
                }
                warnings.push(skipped_warning(
                    root,
                    DiscoveryWarningCategory::Io,
                    DiscoveryWarningSeverity::Warning,
                    format!("applications directory could not be inspected: {source}"),
                ));
                return Ok(Vec::new());
            }
        }

        if let Err(source) = fs::read_dir(root) {
            if self.options.require_existing_roots {
                return Err(DiscoveryError::RequiredRootUnavailable {
                    path: root.to_owned(),
                    source,
                });
            }
            warnings.push(skipped_warning(
                root,
                DiscoveryWarningCategory::Io,
                DiscoveryWarningSeverity::Warning,
                format!("applications directory could not be read: {source}"),
            ));
            return Ok(Vec::new());
        }

        let mut files = Vec::new();
        collect_desktop_files(root, &mut files, warnings);
        files.sort();
        Ok(files)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_application(
        &self,
        desktop_id: String,
        desktop_file: PathBuf,
        source_priority: usize,
        entry: DesktopEntry,
        resolver: &ExecutableResolver,
        warnings: &mut Vec<DiscoveryWarning>,
    ) -> Option<DiscoveredApplication> {
        if entry.hidden {
            warnings.push(skipped_warning(
                &desktop_file,
                DiscoveryWarningCategory::Visibility,
                DiscoveryWarningSeverity::Info,
                "Hidden=true suppresses this desktop ID at every lower-priority source",
            ));
            return None;
        }
        if entry.entry_type.as_deref() != Some("Application") {
            warnings.push(skipped_warning(
                &desktop_file,
                DiscoveryWarningCategory::DesktopEntry,
                DiscoveryWarningSeverity::Info,
                "Type must be Application",
            ));
            return None;
        }
        if !entry.only_show_in.is_empty() && !entry.not_show_in.is_empty() {
            warnings.push(skipped_warning(
                &desktop_file,
                DiscoveryWarningCategory::DesktopEntry,
                DiscoveryWarningSeverity::Warning,
                "OnlyShowIn and NotShowIn must not both be present",
            ));
            return None;
        }
        if entry.no_display && !self.options.include_no_display {
            warnings.push(skipped_warning(
                &desktop_file,
                DiscoveryWarningCategory::Visibility,
                DiscoveryWarningSeverity::Info,
                "NoDisplay=true is excluded by the default discovery policy",
            ));
            return None;
        }
        if !self.options.ignore_desktop_filter && !desktop_is_visible(&entry, &self.options) {
            warnings.push(skipped_warning(
                &desktop_file,
                DiscoveryWarningCategory::Visibility,
                DiscoveryWarningSeverity::Info,
                "entry is excluded by OnlyShowIn/NotShowIn for the active desktop",
            ));
            return None;
        }

        let Some(name) = entry
            .names
            .resolve(self.options.locale.as_deref())
            .filter(|name| !name.trim().is_empty())
        else {
            warnings.push(skipped_warning(
                &desktop_file,
                DiscoveryWarningCategory::DesktopEntry,
                DiscoveryWarningSeverity::Warning,
                "Name is missing or empty for the active locale and fallback",
            ));
            return None;
        };
        let Some(exec) = entry.exec.as_deref() else {
            warnings.push(skipped_warning(
                &desktop_file,
                DiscoveryWarningCategory::DesktopEntry,
                DiscoveryWarningSeverity::Warning,
                "Exec is missing or empty",
            ));
            return None;
        };

        if let Some(try_exec) = entry.try_exec.as_deref() {
            if let Err(error) = resolver.resolve(try_exec) {
                warnings.push(skipped_warning(
                    &desktop_file,
                    DiscoveryWarningCategory::TryExec,
                    DiscoveryWarningSeverity::Warning,
                    format!("TryExec `{try_exec}` is unavailable: {error}"),
                ));
                return None;
            }
        }

        let parsed_exec = match parse_exec(exec, &name, &desktop_file) {
            Ok(parsed) => parsed,
            Err(error) => {
                warnings.push(skipped_warning(
                    &desktop_file,
                    DiscoveryWarningCategory::Exec,
                    DiscoveryWarningSeverity::Warning,
                    format!("invalid Exec value: {error}"),
                ));
                return None;
            }
        };
        let executable = match resolver.resolve(&parsed_exec.executable) {
            Ok(executable) => executable,
            Err(error) => {
                warnings.push(skipped_warning(
                    &desktop_file,
                    DiscoveryWarningCategory::Executable,
                    DiscoveryWarningSeverity::Warning,
                    format!(
                        "launch executable `{}` is unavailable: {error}",
                        parsed_exec.executable
                    ),
                ));
                return None;
            }
        };

        Some(DiscoveredApplication {
            desktop_id,
            name,
            generic_name: entry
                .generic_names
                .resolve(self.options.locale.as_deref())
                .filter(|value| !value.trim().is_empty()),
            executable,
            arguments: parsed_exec.arguments,
            icon: entry.icon,
            desktop_file,
            terminal: entry.terminal,
            categories: entry.categories,
            startup_wm_class: entry.startup_wm_class,
            source_priority,
            no_display: entry.no_display,
        })
    }
}

fn read_entry(path: &Path, warnings: &mut Vec<DiscoveryWarning>) -> Option<DesktopEntry> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            warnings.push(skipped_warning(
                path,
                DiscoveryWarningCategory::Io,
                DiscoveryWarningSeverity::Warning,
                format!("desktop file could not be read: {error}"),
            ));
            return None;
        }
    };
    let input = match std::str::from_utf8(&bytes) {
        Ok(input) => input,
        Err(error) => {
            warnings.push(skipped_warning(
                path,
                DiscoveryWarningCategory::InvalidUtf8,
                DiscoveryWarningSeverity::Warning,
                format!("desktop file is not valid UTF-8: {error}"),
            ));
            return None;
        }
    };
    match parse_desktop_entry(input) {
        Ok(entry) => Some(entry),
        Err(error) => {
            warnings.push(skipped_warning(
                path,
                DiscoveryWarningCategory::DesktopEntry,
                DiscoveryWarningSeverity::Warning,
                format!("desktop entry is malformed: {error}"),
            ));
            None
        }
    }
}

fn collect_desktop_files(
    directory: &Path,
    files: &mut Vec<PathBuf>,
    warnings: &mut Vec<DiscoveryWarning>,
) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(skipped_warning(
                directory,
                DiscoveryWarningCategory::Io,
                DiscoveryWarningSeverity::Warning,
                format!("directory could not be read: {error}"),
            ));
            return;
        }
    };

    let mut entries: Vec<_> = entries.collect();
    entries.sort_by_key(|entry| entry.as_ref().ok().map(fs::DirEntry::path));
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(skipped_warning(
                    directory,
                    DiscoveryWarningCategory::Io,
                    DiscoveryWarningSeverity::Warning,
                    format!("directory entry could not be read: {error}"),
                ));
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                warnings.push(skipped_warning(
                    &path,
                    DiscoveryWarningCategory::Io,
                    DiscoveryWarningSeverity::Warning,
                    format!("file type could not be inspected: {error}"),
                ));
                continue;
            }
        };
        if file_type.is_dir() {
            collect_desktop_files(&path, files, warnings);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "desktop")
            && (file_type.is_file()
                || (file_type.is_symlink() && fs::metadata(&path).is_ok_and(|item| item.is_file())))
        {
            files.push(path);
        }
    }
}

fn desktop_file_id(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return None;
        };
        parts.push(component.to_str()?.to_owned());
    }
    (!parts.is_empty()).then(|| parts.join("-"))
}

fn desktop_is_visible(entry: &DesktopEntry, options: &DiscoveryOptions) -> bool {
    if !entry.only_show_in.is_empty()
        && !entry
            .only_show_in
            .iter()
            .any(|entry_desktop| desktop_matches(entry_desktop, &options.current_desktops))
    {
        return false;
    }
    !entry
        .not_show_in
        .iter()
        .any(|entry_desktop| desktop_matches(entry_desktop, &options.current_desktops))
}

fn desktop_matches(entry_desktop: &str, current_desktops: &[String]) -> bool {
    current_desktops
        .iter()
        .any(|current| entry_desktop.eq_ignore_ascii_case(current))
}

fn deduplicate_launch_targets(
    applications: Vec<DiscoveredApplication>,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Vec<DiscoveredApplication> {
    let mut targets = BTreeMap::<(PathBuf, Vec<String>, bool), (String, PathBuf)>::new();
    let mut deduplicated = Vec::new();
    for application in applications {
        let key = (
            application.executable.clone(),
            application.arguments.clone(),
            application.terminal,
        );
        if let Some((desktop_id, path)) = targets.get(&key) {
            warnings.push(skipped_warning(
                &application.desktop_file,
                DiscoveryWarningCategory::Duplicate,
                DiscoveryWarningSeverity::Info,
                format!(
                    "launch target is equivalent to desktop ID {desktop_id} from {}",
                    path.display()
                ),
            ));
        } else {
            targets.insert(
                key,
                (
                    application.desktop_id.clone(),
                    application.desktop_file.clone(),
                ),
            );
            deduplicated.push(application);
        }
    }
    deduplicated
}

fn skipped_warning(
    path: &Path,
    category: DiscoveryWarningCategory,
    severity: DiscoveryWarningSeverity,
    reason: impl Into<String>,
) -> DiscoveryWarning {
    DiscoveryWarning::skipped(path.to_owned(), category, severity, reason)
}

fn sort_warnings(warnings: &mut [DiscoveryWarning]) {
    warnings.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.category.cmp(&right.category))
            .then_with(|| left.severity.cmp(&right.severity))
            .then_with(|| left.reason.cmp(&right.reason))
            .then_with(|| left.skipped.cmp(&right.skipped))
    });
}

fn environment_data_home() -> Option<PathBuf> {
    let configured = env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    if configured
        .as_ref()
        .is_some_and(|path| path.is_absolute() && !path.as_os_str().is_empty())
    {
        return configured;
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute() && !home.as_os_str().is_empty())
        .map(|home| home.join(".local/share"))
}

fn environment_data_dirs() -> Vec<PathBuf> {
    let configured = env::var_os("XDG_DATA_DIRS")
        .map(|value| {
            env::split_paths(&value)
                .filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if configured.is_empty() {
        vec![
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ]
    } else {
        configured
    }
}

fn environment_locale() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|key| {
            env::var_os(key)
                .map(|value| value.to_string_lossy().into_owned())
                .filter(|value| !value.is_empty())
        })
}

fn split_desktops(value: &str) -> Vec<String> {
    value
        .split(':')
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{desktop_file_id, desktop_is_visible, DiscoveryOptions};
    use crate::discovery::desktop_entry::parse_desktop_entry;
    use std::path::Path;

    #[test]
    fn nested_paths_form_standard_desktop_ids() {
        assert_eq!(
            desktop_file_id(
                Path::new("/data/applications"),
                Path::new("/data/applications/vendor/example.desktop")
            )
            .as_deref(),
            Some("vendor-example.desktop")
        );
    }

    #[test]
    fn desktop_filters_match_any_active_desktop() {
        let only = parse_desktop_entry(
            "[Desktop Entry]\nName=Example\nExec=example\nOnlyShowIn=GNOME;KDE;\n",
        )
        .unwrap_or_else(|error| panic!("parse OnlyShowIn entry: {error}"));
        let not =
            parse_desktop_entry("[Desktop Entry]\nName=Example\nExec=example\nNotShowIn=GNOME;\n")
                .unwrap_or_else(|error| panic!("parse NotShowIn entry: {error}"));
        let mut options = DiscoveryOptions::from_environment();
        options.current_desktops = vec!["sway".to_owned(), "gnome".to_owned()];

        assert!(desktop_is_visible(&only, &options));
        assert!(!desktop_is_visible(&not, &options));
        options.current_desktops.clear();
        assert!(!desktop_is_visible(&only, &options));
        assert!(desktop_is_visible(&not, &options));
    }
}
