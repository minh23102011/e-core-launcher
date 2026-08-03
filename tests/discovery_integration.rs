use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use ecore_launcher::{
    DesktopApplicationScanner, DiscoveryError, DiscoveryOptions, DiscoveryWarningCategory,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(test: &str) -> Self {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ecore-launcher-discovery-{}-{test}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap_or_else(|error| panic!("create temp directory: {error}"));
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _result = fs::remove_dir_all(&self.0);
    }
}

fn fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/desktop")
        .join(path)
}

fn make_executable(path: &Path) {
    File::create(path).unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
    let mut permissions = fs::metadata(path)
        .unwrap_or_else(|error| panic!("read {} metadata: {error}", path.display()))
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .unwrap_or_else(|error| panic!("make {} executable: {error}", path.display()));
}

fn fixture_bin() -> TempDirectory {
    let directory = TempDirectory::new("bin");
    for name in [
        "fixture-user",
        "fixture-simple",
        "fixture-static",
        "fixture quoted",
        "fixture-terminal",
        "fixture-codes",
        "fixture-meta",
        "fixture-try",
        "fixture-system",
    ] {
        make_executable(&directory.path().join(name));
    }
    directory
}

fn options(bin: &Path) -> DiscoveryOptions {
    DiscoveryOptions {
        data_home: Some(fixture("data-home")),
        data_dirs: vec![fixture("data-dir-1"), fixture("data-dir-2")],
        executable_path: vec![bin.to_owned()],
        locale: Some("fr_CA.UTF-8".to_owned()),
        current_desktops: vec!["GNOME".to_owned()],
        include_no_display: false,
        ignore_desktop_filter: false,
        require_existing_roots: true,
    }
}

#[test]
fn fixture_scan_parses_filters_resolves_and_orders_applications() {
    let bin = fixture_bin();
    let report = DesktopApplicationScanner::from_options(options(bin.path()))
        .discover()
        .unwrap_or_else(|error| panic!("discover fixtures: {error}"));
    let ids: Vec<_> = report
        .applications
        .iter()
        .map(|application| application.desktop_id.as_str())
        .collect();

    assert_eq!(report.applications.len(), 16);
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(ids.contains(&"simple.desktop"));
    assert!(ids.contains(&"vendor-nested.desktop"));
    assert!(ids.contains(&"only-gnome.desktop"));
    assert!(!ids.contains(&"not-gnome.desktop"));
    assert!(!ids.contains(&"no-display.desktop"));
    assert!(!ids.contains(&"hidden.desktop"));
    assert!(!ids.contains(&"suppress.desktop"));
    assert!(ids.contains(&"duplicate-a.desktop"));
    assert!(!ids.contains(&"duplicate-b.desktop"));
    assert!(ids.contains(&"profile-a.desktop"));
    assert!(ids.contains(&"profile-b.desktop"));

    let localized = report
        .applications
        .iter()
        .find(|application| application.desktop_id == "localized.desktop")
        .unwrap_or_else(|| panic!("localized fixture should be present"));
    assert_eq!(localized.name, "Nom québécois");
    assert_eq!(localized.generic_name.as_deref(), Some("Nom générique"));

    let terminal = report
        .applications
        .iter()
        .find(|application| application.desktop_id == "terminal.desktop")
        .unwrap_or_else(|| panic!("terminal fixture should be present"));
    assert!(terminal.terminal);
    assert_eq!(terminal.icon.as_deref(), Some("utilities-terminal"));
    assert_eq!(terminal.categories, ["System", "TerminalEmulator"]);

    let quoted = report
        .applications
        .iter()
        .find(|application| application.desktop_id == "quoted.desktop")
        .unwrap_or_else(|| panic!("quoted fixture should be present"));
    assert_eq!(quoted.executable, bin.path().join("fixture quoted"));
    assert_eq!(quoted.arguments, ["two words", "escaped space"]);

    let try_exec = report
        .applications
        .iter()
        .find(|application| application.desktop_id == "valid-tryexec.desktop")
        .unwrap_or_else(|| panic!("valid TryExec fixture should be present"));
    assert_eq!(try_exec.executable, bin.path().join("fixture-simple"));
    assert_ne!(try_exec.executable, bin.path().join("fixture-try"));
}

#[test]
fn exec_field_codes_and_shell_metacharacters_are_static_arguments() {
    let bin = fixture_bin();
    let report = DesktopApplicationScanner::from_options(options(bin.path()))
        .discover()
        .unwrap_or_else(|error| panic!("discover fixtures: {error}"));
    let field_codes = report
        .applications
        .iter()
        .find(|application| application.desktop_id == "field-codes.desktop")
        .unwrap_or_else(|| panic!("field-code fixture should be present"));
    assert_eq!(
        field_codes.arguments,
        [
            "Field Code Application".to_owned(),
            fixture("data-dir-1/applications/field-codes.desktop")
                .to_string_lossy()
                .into_owned(),
            "100%".to_owned()
        ]
    );

    let metacharacters = report
        .applications
        .iter()
        .find(|application| application.desktop_id == "shell-meta.desktop")
        .unwrap_or_else(|| panic!("shell metacharacter fixture should be present"));
    assert_eq!(
        metacharacters.arguments,
        [";", "|", "&&", "$HOME", "$(touch)"]
    );
}

#[test]
fn xdg_precedence_override_and_hidden_suppression_are_enforced() {
    let bin = fixture_bin();
    let report = DesktopApplicationScanner::from_options(options(bin.path()))
        .discover()
        .unwrap_or_else(|error| panic!("discover fixtures: {error}"));

    let overridden = report
        .applications
        .iter()
        .find(|application| application.desktop_id == "override.desktop")
        .unwrap_or_else(|| panic!("override fixture should be present"));
    assert_eq!(overridden.name, "User Override");
    assert_eq!(overridden.source_priority, 0);
    let earlier = report
        .applications
        .iter()
        .find(|application| application.desktop_id == "earlier.desktop")
        .unwrap_or_else(|| panic!("earlier fixture should be present"));
    assert_eq!(earlier.name, "Earlier System Directory");
    assert_eq!(earlier.source_priority, 1);
    assert!(report.warnings.iter().any(|warning| {
        warning.category == DiscoveryWarningCategory::Overridden
            && warning.path == fixture("data-dir-1/applications/suppress.desktop")
    }));
    assert!(report.warnings.iter().any(|warning| {
        warning.category == DiscoveryWarningCategory::Visibility
            && warning.path == fixture("data-home/applications/suppress.desktop")
            && warning.reason.contains("suppresses this desktop ID")
    }));
}

#[test]
fn all_and_ignore_desktop_filter_have_narrow_documented_effects() {
    let bin = fixture_bin();
    let mut all_options = options(bin.path());
    all_options.include_no_display = true;
    let all_report = DesktopApplicationScanner::from_options(all_options)
        .discover()
        .unwrap_or_else(|error| panic!("discover --all fixtures: {error}"));
    assert!(all_report
        .applications
        .iter()
        .any(|application| application.desktop_id == "no-display.desktop"));
    assert!(!all_report
        .applications
        .iter()
        .any(|application| application.desktop_id == "suppress.desktop"));

    let mut ignored_options = options(bin.path());
    ignored_options.ignore_desktop_filter = true;
    let ignored_report = DesktopApplicationScanner::from_options(ignored_options)
        .discover()
        .unwrap_or_else(|error| panic!("discover ignored desktop filters: {error}"));
    assert!(ignored_report
        .applications
        .iter()
        .any(|application| application.desktop_id == "not-gnome.desktop"));
}

#[test]
fn invalid_and_unavailable_entries_produce_typed_sorted_warnings() {
    let bin = fixture_bin();
    let report = DesktopApplicationScanner::from_options(options(bin.path()))
        .discover()
        .unwrap_or_else(|error| panic!("discover fixtures: {error}"));

    assert!(report.warnings.windows(2).all(|pair| {
        (
            &pair[0].path,
            pair[0].category,
            pair[0].severity,
            &pair[0].reason,
        ) <= (
            &pair[1].path,
            pair[1].category,
            pair[1].severity,
            &pair[1].reason,
        )
    }));
    for category in [
        DiscoveryWarningCategory::DesktopEntry,
        DiscoveryWarningCategory::Exec,
        DiscoveryWarningCategory::Executable,
        DiscoveryWarningCategory::TryExec,
        DiscoveryWarningCategory::Visibility,
        DiscoveryWarningCategory::Overridden,
        DiscoveryWarningCategory::Duplicate,
    ] {
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.category == category),
            "missing warning category {category:?}"
        );
    }
    assert!(report.warnings.iter().all(|warning| warning.skipped));
}

#[test]
fn report_and_json_serialization_are_deterministic() {
    let bin = fixture_bin();
    let scanner = DesktopApplicationScanner::from_options(options(bin.path()));
    let first = scanner
        .discover()
        .unwrap_or_else(|error| panic!("first fixture discovery: {error}"));
    let second = scanner
        .discover()
        .unwrap_or_else(|error| panic!("second fixture discovery: {error}"));
    assert_eq!(first, second);

    let first_json = serde_json::to_string_pretty(&first)
        .unwrap_or_else(|error| panic!("serialize first report: {error}"));
    let second_json = serde_json::to_string_pretty(&second)
        .unwrap_or_else(|error| panic!("serialize second report: {error}"));
    assert_eq!(first_json, second_json);
    let value: serde_json::Value = serde_json::from_str(&first_json)
        .unwrap_or_else(|error| panic!("parse report JSON: {error}"));
    assert!(value["applications"].is_array());
    assert!(value["warnings"].is_array());
    assert!(value["applications"][0]["desktop_file"].is_string());
}

#[test]
fn absolute_executable_and_invalid_utf8_use_only_temporary_fixtures() {
    let root = TempDirectory::new("absolute");
    let applications = root.path().join("data/applications");
    let bin = root.path().join("absolute executable");
    fs::create_dir_all(&applications)
        .unwrap_or_else(|error| panic!("create applications directory: {error}"));
    make_executable(&bin);
    let template = fs::read_to_string(fixture("templates/absolute.desktop.in"))
        .unwrap_or_else(|error| panic!("read absolute template: {error}"));
    fs::write(
        applications.join("absolute.desktop"),
        template.replace("@EXECUTABLE@", &bin.to_string_lossy()),
    )
    .unwrap_or_else(|error| panic!("write absolute fixture: {error}"));
    fs::write(applications.join("invalid-utf8.desktop"), [0xff, 0xfe])
        .unwrap_or_else(|error| panic!("write invalid UTF-8 fixture: {error}"));
    let scanner = DesktopApplicationScanner::from_options(DiscoveryOptions {
        data_home: Some(root.path().join("data")),
        data_dirs: Vec::new(),
        executable_path: Vec::new(),
        locale: None,
        current_desktops: Vec::new(),
        include_no_display: false,
        ignore_desktop_filter: false,
        require_existing_roots: true,
    });
    let report = scanner
        .discover()
        .unwrap_or_else(|error| panic!("discover absolute fixture: {error}"));

    assert_eq!(report.applications.len(), 1);
    assert_eq!(report.applications[0].executable, bin);
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.category == DiscoveryWarningCategory::InvalidUtf8));
}

#[test]
fn invalid_configuration_and_required_roots_are_fatal() {
    let mut no_roots = options(Path::new("/unused"));
    no_roots.data_home = None;
    no_roots.data_dirs.clear();
    assert!(matches!(
        DesktopApplicationScanner::from_options(no_roots).discover(),
        Err(DiscoveryError::NoDiscoveryRoots)
    ));

    let missing = TempDirectory::new("missing-root");
    let scanner = DesktopApplicationScanner::from_options(DiscoveryOptions {
        data_home: Some(missing.path().join("missing")),
        data_dirs: Vec::new(),
        executable_path: Vec::new(),
        locale: None,
        current_desktops: Vec::new(),
        include_no_display: false,
        ignore_desktop_filter: false,
        require_existing_roots: true,
    });
    assert!(matches!(
        scanner.discover(),
        Err(DiscoveryError::RequiredRootUnavailable { .. })
    ));
}

#[test]
fn cli_json_uses_replacement_paths_and_fixture_path() {
    let bin = fixture_bin();
    let output = Command::new(env!("CARGO_BIN_EXE_ecore-launcher"))
        .arg("discover")
        .arg("--json")
        .arg("--data-home")
        .arg(fixture("data-home"))
        .arg("--data-dir")
        .arg(fixture("data-dir-1"))
        .arg("--data-dir")
        .arg(fixture("data-dir-2"))
        .env("PATH", bin.path())
        .env("LC_ALL", "fr_CA.UTF-8")
        .env("XDG_CURRENT_DESKTOP", "GNOME")
        .output()
        .unwrap_or_else(|error| panic!("run discover CLI: {error}"));

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("parse CLI JSON: {error}"));
    assert_eq!(value["applications"].as_array().map(Vec::len), Some(16));
    assert!(value["warnings"].is_array());
}
