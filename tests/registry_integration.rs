use std::fs::{self, File};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use ecore_launcher::{
    AppRegistry, ApplicationSettingsUpdate, DiscoveredApplication, DiscoveryReport,
    IoPriorityClass, RegisteredApplicationAvailability, RegistryError, RegistryStore,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(test: &str) -> Self {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ecore-launcher-registry-{}-{test}-{sequence}",
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

fn config_path(root: &TempDirectory) -> PathBuf {
    root.path().join("config/ecore-launcher/config.toml")
}

fn discovered(desktop_id: &str, name: &str) -> DiscoveredApplication {
    DiscoveredApplication {
        desktop_id: desktop_id.to_owned(),
        name: name.to_owned(),
        generic_name: None,
        executable: PathBuf::from("/fixture/bin/app"),
        arguments: vec!["--fixture".to_owned()],
        icon: Some("fixture".to_owned()),
        desktop_file: PathBuf::from(format!("/fixture/applications/{desktop_id}")),
        terminal: false,
        categories: vec!["Utility".to_owned()],
        startup_wm_class: None,
        source_priority: 0,
        no_display: false,
    }
}

fn write_executable(path: &Path) {
    File::create(path).unwrap_or_else(|error| panic!("create executable: {error}"));
    let mut permissions = fs::metadata(path)
        .unwrap_or_else(|error| panic!("read executable metadata: {error}"))
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .unwrap_or_else(|error| panic!("set executable permissions: {error}"));
}

#[test]
fn missing_config_loads_empty_without_creating_file() {
    let root = TempDirectory::new("missing");
    let path = config_path(&root);
    let store = RegistryStore::new(&path);

    let load = store
        .load_with_status()
        .unwrap_or_else(|error| panic!("load missing config: {error}"));
    assert!(!load.exists);
    assert!(load.registry.apps.is_empty());
    assert!(!path.exists());
}

#[test]
fn saving_creates_private_parent_and_round_trips_deterministically() {
    let root = TempDirectory::new("save");
    let path = config_path(&root);
    let store = RegistryStore::new(&path);
    let mut registry = AppRegistry::default();
    registry
        .add_discovered(&[
            discovered("z.desktop", "Zulu"),
            discovered("a.desktop", "Alpha"),
        ])
        .unwrap_or_else(|error| panic!("add registry entries: {error}"));
    store
        .save(&registry)
        .unwrap_or_else(|error| panic!("save registry: {error}"));

    let contents = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read config: {error}"));
    assert!(contents.starts_with("schema_version = 1\n"));
    assert!(contents.find("a.desktop") < contents.find("z.desktop"));
    let mode = fs::metadata(&path)
        .unwrap_or_else(|error| panic!("read config metadata: {error}"))
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let loaded = store
        .load()
        .unwrap_or_else(|error| panic!("reload config: {error}"));
    assert_eq!(loaded.apps[0].desktop_id, "a.desktop");
    assert_eq!(loaded.apps[1].desktop_id, "z.desktop");
}

#[test]
fn malformed_unsupported_and_invalid_configs_are_not_replaced() {
    let root = TempDirectory::new("invalid");
    let path = root.path().join("config.toml");
    let store = RegistryStore::new(&path);
    fs::write(&path, "schema_version = [\n")
        .unwrap_or_else(|error| panic!("write malformed config: {error}"));
    let original =
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("read malformed config: {error}"));
    assert!(matches!(
        store.load(),
        Err(RegistryError::TomlSyntax { .. })
    ));
    assert!(matches!(
        store.save(&AppRegistry::default()),
        Err(RegistryError::TomlSyntax { .. })
    ));
    assert_eq!(fs::read_to_string(&path).unwrap_or_default(), original);

    fs::write(&path, "schema_version = 99\n")
        .unwrap_or_else(|error| panic!("write schema config: {error}"));
    assert!(matches!(
        store.load(),
        Err(RegistryError::UnsupportedSchemaVersion { found: 99 })
    ));

    fs::write(
        &path,
        "schema_version = 1\n[launcher]\ndefault_delay_seconds = 0\ndefault_nice = 5\ndefault_io_class = \"best-effort\"\ndefault_io_priority = 9\ndefault_enforce_process_tree = false\n",
    )
    .unwrap_or_else(|error| panic!("write invalid policy config: {error}"));
    assert!(matches!(store.load(), Err(RegistryError::Validation(_))));
}

#[test]
fn duplicate_desktop_ids_are_rejected_semantically() {
    let root = TempDirectory::new("duplicate-ids");
    let path = root.path().join("config.toml");
    fs::write(
        &path,
        "schema_version = 1\n[[apps]]\ndesktop_id = \"same.desktop\"\nname = \"One\"\nenabled = true\ndelay_seconds = 0\nnice = 5\nio_class = \"best-effort\"\nio_priority = 4\nenforce_process_tree = false\n[[apps]]\ndesktop_id = \"same.desktop\"\nname = \"Two\"\nenabled = true\ndelay_seconds = 0\nnice = 5\nio_class = \"best-effort\"\nio_priority = 4\nenforce_process_tree = false\n",
    )
    .unwrap_or_else(|error| panic!("write duplicate config: {error}"));
    let store = RegistryStore::new(&path);
    assert!(matches!(store.load(), Err(RegistryError::Validation(_))));
}

#[test]
fn unknown_toml_values_survive_a_canonical_rewrite() {
    let root = TempDirectory::new("unknown-values");
    let path = root.path().join("config.toml");
    fs::write(
        &path,
        "schema_version = 1\nfuture_top = \"kept\"\n[launcher]\ndefault_delay_seconds = 0\ndefault_nice = 5\ndefault_io_class = \"best-effort\"\ndefault_io_priority = 4\ndefault_enforce_process_tree = false\nfuture_launcher = true\n[[apps]]\ndesktop_id = \"example.desktop\"\nname = \"Example\"\nenabled = true\ndelay_seconds = 0\nnice = 5\nio_class = \"best-effort\"\nio_priority = 4\nenforce_process_tree = false\nfuture_app = 7\n",
    )
    .unwrap_or_else(|error| panic!("write future config: {error}"));
    let store = RegistryStore::new(&path);
    let registry = store
        .load()
        .unwrap_or_else(|error| panic!("load future config: {error}"));
    store
        .save(&registry)
        .unwrap_or_else(|error| panic!("rewrite future config: {error}"));
    let contents =
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("read rewritten config: {error}"));
    for expected in [
        "future_top = \"kept\"",
        "future_launcher = true",
        "future_app = 7",
    ] {
        assert!(contents.contains(expected), "missing {expected}");
    }
}

#[test]
fn existing_symlinked_config_is_rejected() {
    let root = TempDirectory::new("symlink");
    let target = root.path().join("target.toml");
    let link = root.path().join("config.toml");
    fs::write(&target, "schema_version = 1\n")
        .unwrap_or_else(|error| panic!("write target: {error}"));
    symlink(&target, &link).unwrap_or_else(|error| panic!("create config symlink: {error}"));
    let store = RegistryStore::new(&link);
    assert!(matches!(
        store.load(),
        Err(RegistryError::SymlinkRejected { .. })
    ));
    assert!(matches!(
        store.save(&AppRegistry::default()),
        Err(RegistryError::SymlinkRejected { .. })
    ));
}

#[test]
fn registry_operations_are_explicit_idempotent_and_keep_unavailable_entries() {
    let mut registry = AppRegistry::default();
    let one = discovered("one.desktop", "One");
    let two = discovered("two.desktop", "Two");
    let first = registry
        .add_discovered(&[one.clone(), two.clone(), one.clone()])
        .unwrap_or_else(|error| panic!("add selected apps: {error}"));
    assert_eq!(first.added, ["one.desktop", "two.desktop"]);
    let repeated = registry
        .add_discovered(&[one])
        .unwrap_or_else(|error| panic!("repeat add: {error}"));
    assert_eq!(repeated.already_registered, ["one.desktop"]);
    assert_eq!(registry.apps.len(), 2);

    let disable = registry
        .set_enabled(&["one.desktop".to_owned()], false)
        .unwrap_or_else(|error| panic!("disable: {error}"));
    assert_eq!(disable.changed, ["one.desktop"]);
    let disabled_again = registry
        .set_enabled(&["one.desktop".to_owned()], false)
        .unwrap_or_else(|error| panic!("repeat disable: {error}"));
    assert_eq!(disabled_again.unchanged, ["one.desktop"]);

    registry
        .configure(
            "one.desktop",
            &ApplicationSettingsUpdate {
                delay_seconds: Some(5),
                nice: Some(0),
                io_class: Some(IoPriorityClass::Idle),
                ..ApplicationSettingsUpdate::default()
            },
        )
        .unwrap_or_else(|error| panic!("configure: {error}"));
    let configured = registry
        .apps
        .iter()
        .find(|application| application.desktop_id == "one.desktop")
        .unwrap_or_else(|| panic!("configured app should exist"));
    assert_eq!(configured.delay_seconds, 5);
    assert_eq!(configured.io_priority, None);
    registry
        .configure(
            "one.desktop",
            &ApplicationSettingsUpdate {
                reset_to_defaults: true,
                ..ApplicationSettingsUpdate::default()
            },
        )
        .unwrap_or_else(|error| panic!("reset: {error}"));
    assert_eq!(registry.apps[0].delay_seconds, 0);

    let unavailable = registry.resolve_against(Some(&DiscoveryReport::default()));
    assert!(unavailable.iter().all(|status| matches!(
        status.availability,
        RegisteredApplicationAvailability::Unavailable
    )));
    registry
        .remove(&["two.desktop".to_owned()])
        .unwrap_or_else(|error| panic!("remove: {error}"));
    assert_eq!(registry.apps.len(), 1);
}

#[test]
fn failed_operations_and_store_mutations_leave_prior_state_intact() {
    let root = TempDirectory::new("rollback");
    let path = config_path(&root);
    let store = RegistryStore::new(&path);
    store
        .mutate(|registry| registry.add_discovered(&[discovered("valid.desktop", "Valid")]))
        .unwrap_or_else(|error| panic!("initial mutation: {error}"));
    let before =
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("read original config: {error}"));
    let invalid = discovered("../invalid.desktop", "Invalid");
    assert!(store
        .mutate(|registry| registry.add_discovered(&[invalid]))
        .is_err());
    assert_eq!(fs::read_to_string(&path).unwrap_or_default(), before);
    assert!(matches!(
        store.mutate(|registry| registry.remove(&["missing.desktop".to_owned()])),
        Err(RegistryError::UnknownRegisteredApplication { .. })
    ));
    assert_eq!(fs::read_to_string(&path).unwrap_or_default(), before);
}

#[test]
fn sequential_mutations_reload_the_latest_saved_state() {
    let root = TempDirectory::new("sequential");
    let path = config_path(&root);
    let first = RegistryStore::new(&path);
    let second = RegistryStore::new(&path);
    first
        .mutate(|registry| registry.add_discovered(&[discovered("one.desktop", "One")]))
        .unwrap_or_else(|error| panic!("first mutation: {error}"));
    second
        .mutate(|registry| registry.add_discovered(&[discovered("two.desktop", "Two")]))
        .unwrap_or_else(|error| panic!("second mutation: {error}"));
    let loaded = first
        .load()
        .unwrap_or_else(|error| panic!("load sequential result: {error}"));
    assert_eq!(loaded.apps.len(), 2);
}

#[test]
fn cli_registry_workflow_uses_only_fixture_discovery_and_temp_config() {
    let root = TempDirectory::new("cli");
    let config = root.path().join("registry/config.toml");
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).unwrap_or_else(|error| panic!("create CLI bin: {error}"));
    write_executable(&bin.join("fixture-simple"));
    write_executable(&bin.join("fixture-user"));
    let executable = env!("CARGO_BIN_EXE_ecore-launcher");

    let unknown_config = root.path().join("unknown/config.toml");
    let unknown = Command::new(executable)
        .arg("add")
        .arg("--config")
        .arg(&unknown_config)
        .arg("--data-home")
        .arg(fixture("data-home"))
        .arg("--data-dir")
        .arg(fixture("data-dir-1"))
        .arg("missing.desktop")
        .env("PATH", &bin)
        .env("XDG_CURRENT_DESKTOP", "GNOME")
        .output()
        .unwrap_or_else(|error| panic!("run unknown add: {error}"));
    assert!(!unknown.status.success());
    assert!(!unknown_config.exists());

    let add = Command::new(executable)
        .arg("add")
        .arg("--config")
        .arg(&config)
        .arg("--data-home")
        .arg(fixture("data-home"))
        .arg("--data-dir")
        .arg(fixture("data-dir-1"))
        .arg("simple.desktop")
        .env("PATH", &bin)
        .env("XDG_CURRENT_DESKTOP", "GNOME")
        .output()
        .unwrap_or_else(|error| panic!("run add: {error}"));
    assert!(
        add.status.success(),
        "add stderr: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    assert!(config.exists());

    let list = Command::new(executable)
        .arg("list")
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap_or_else(|error| panic!("run list: {error}"));
    assert!(list.status.success());
    assert!(String::from_utf8_lossy(&list.stdout).contains("simple.desktop"));

    let list_json = Command::new(executable)
        .arg("list")
        .arg("--config")
        .arg(&config)
        .arg("--json")
        .output()
        .unwrap_or_else(|error| panic!("run list JSON: {error}"));
    assert!(list_json.status.success());
    let json: serde_json::Value = serde_json::from_slice(&list_json.stdout)
        .unwrap_or_else(|error| panic!("parse list JSON: {error}"));
    assert_eq!(
        json["applications"][0]["application"]["desktop_id"],
        "simple.desktop"
    );

    for command in ["disable", "enable"] {
        let output = Command::new(executable)
            .arg(command)
            .arg("--config")
            .arg(&config)
            .arg("simple.desktop")
            .output()
            .unwrap_or_else(|error| panic!("run {command}: {error}"));
        assert!(output.status.success());
    }
    let configure = Command::new(executable)
        .arg("configure")
        .arg("--config")
        .arg(&config)
        .arg("simple.desktop")
        .arg("--delay")
        .arg("5")
        .arg("--nice")
        .arg("5")
        .output()
        .unwrap_or_else(|error| panic!("run configure: {error}"));
    assert!(configure.status.success());

    let show = Command::new(executable)
        .arg("show")
        .arg("--config")
        .arg(&config)
        .arg("simple.desktop")
        .arg("--json")
        .output()
        .unwrap_or_else(|error| panic!("run show: {error}"));
    assert!(show.status.success());
    assert!(String::from_utf8_lossy(&show.stdout).contains("\"delay_seconds\": 5"));

    let path_output = Command::new(executable)
        .arg("config")
        .arg("path")
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap_or_else(|error| panic!("run config path: {error}"));
    assert!(path_output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&path_output.stdout).trim(),
        config.display().to_string()
    );
    let validate = Command::new(executable)
        .arg("config")
        .arg("validate")
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap_or_else(|error| panic!("run config validate: {error}"));
    assert!(validate.status.success());

    let remove = Command::new(executable)
        .arg("remove")
        .arg("--config")
        .arg(&config)
        .arg("--yes")
        .arg("simple.desktop")
        .output()
        .unwrap_or_else(|error| panic!("run remove: {error}"));
    assert!(remove.status.success());
}

#[test]
fn cli_read_only_missing_registry_does_not_create_file() {
    let root = TempDirectory::new("cli-missing");
    let config = root.path().join("missing/config.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_ecore-launcher"))
        .arg("list")
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap_or_else(|error| panic!("run missing list: {error}"));
    assert!(output.status.success());
    assert!(!config.exists());

    let validate = Command::new(env!("CARGO_BIN_EXE_ecore-launcher"))
        .arg("config")
        .arg("validate")
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap_or_else(|error| panic!("run missing config validation: {error}"));
    assert!(validate.status.success());
    assert!(String::from_utf8_lossy(&validate.stdout).contains("does not exist"));
    assert!(!config.exists());
}
