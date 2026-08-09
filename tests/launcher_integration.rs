use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use ecore_launcher::{
    build_launch_plan, AppRegistry, CpuTopology, CpuTopologyDetector, DiscoveredApplication,
    DiscoveryReport, LauncherError, RegisteredApplication, TopologyClass,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(test: &str) -> Self {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ecore-launcher-launcher-{}-{test}-{sequence}",
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

fn sysfs(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/sysfs")
        .join(name)
}

fn topology(name: &str) -> CpuTopology {
    CpuTopologyDetector::new(sysfs(name))
        .detect()
        .unwrap_or_else(|error| panic!("detect {name}: {error}"))
}

fn registry(entries: &[(&str, bool)]) -> AppRegistry {
    let mut registry = AppRegistry::default();
    registry.apps = entries
        .iter()
        .map(|(desktop_id, enabled)| {
            let mut application = RegisteredApplication::default();
            application.desktop_id = (*desktop_id).to_owned();
            application.name = format!("stored {desktop_id}");
            application.enabled = *enabled;
            application
        })
        .collect();
    registry
}

fn application(desktop_id: &str) -> DiscoveredApplication {
    DiscoveredApplication {
        desktop_id: desktop_id.to_owned(),
        name: format!("current {desktop_id}"),
        generic_name: None,
        executable: PathBuf::from("/bin/fixture-target"),
        arguments: vec!["--static".to_owned()],
        icon: None,
        desktop_file: PathBuf::from(format!("/fixture/{desktop_id}")),
        terminal: false,
        categories: Vec::new(),
        startup_wm_class: None,
        source_priority: 0,
        no_display: false,
    }
}

fn report(applications: Vec<DiscoveredApplication>) -> DiscoveryReport {
    DiscoveryReport {
        applications,
        warnings: Vec::new(),
    }
}

#[test]
fn default_selection_includes_only_enabled_apps_in_desktop_id_order() {
    let plan = build_launch_plan(
        &registry(&[
            ("z.desktop", true),
            ("disabled.desktop", false),
            ("a.desktop", true),
        ]),
        &report(vec![application("z.desktop"), application("a.desktop")]),
        Some(&topology("intel-hybrid")),
        &[],
    )
    .unwrap_or_else(|error| panic!("build default plan: {error}"));
    assert_eq!(
        plan.applications
            .iter()
            .map(|application| application.desktop_id.as_str())
            .collect::<Vec<_>>(),
        ["a.desktop", "z.desktop"]
    );
}

#[test]
fn explicit_selection_is_registered_enabled_deduplicated_and_ordered() {
    let plan = build_launch_plan(
        &registry(&[("z.desktop", true), ("a.desktop", true)]),
        &report(vec![application("z.desktop"), application("a.desktop")]),
        Some(&topology("intel-hybrid")),
        &[
            "z.desktop".to_owned(),
            "a.desktop".to_owned(),
            "z.desktop".to_owned(),
        ],
    )
    .unwrap_or_else(|error| panic!("build explicit plan: {error}"));
    assert_eq!(
        plan.applications
            .iter()
            .map(|application| application.desktop_id.as_str())
            .collect::<Vec<_>>(),
        ["a.desktop", "z.desktop"]
    );
}

#[test]
fn planner_rejects_unknown_disabled_unavailable_and_terminal_apps() {
    let hybrid = topology("intel-hybrid");
    assert!(matches!(
        build_launch_plan(
            &registry(&[]),
            &report(Vec::new()),
            Some(&hybrid),
            &["missing.desktop".to_owned()]
        ),
        Err(LauncherError::UnknownRegisteredApplication { .. })
    ));
    assert!(matches!(
        build_launch_plan(
            &registry(&[("off.desktop", false)]),
            &report(vec![application("off.desktop")]),
            Some(&hybrid),
            &["off.desktop".to_owned()]
        ),
        Err(LauncherError::DisabledApplication { .. })
    ));
    assert!(matches!(
        build_launch_plan(
            &registry(&[("gone.desktop", true)]),
            &report(Vec::new()),
            Some(&hybrid),
            &[]
        ),
        Err(LauncherError::UnavailableApplication { .. })
    ));
    let mut terminal = application("terminal.desktop");
    terminal.terminal = true;
    assert!(matches!(
        build_launch_plan(
            &registry(&[("terminal.desktop", true)]),
            &report(vec![terminal]),
            Some(&hybrid),
            &[]
        ),
        Err(LauncherError::TerminalApplication { .. })
    ));
}

#[test]
fn empty_registry_is_a_successful_no_op_without_topology() {
    let plan = build_launch_plan(&registry(&[]), &report(Vec::new()), None, &[])
        .unwrap_or_else(|error| panic!("empty plan: {error}"));
    assert!(plan.applications.is_empty());
    assert!(plan.efficiency_cpus.is_empty());
}

#[test]
fn planner_fails_closed_for_uniform_and_unknown_topology() {
    for fixture in ["intel-uniform", "ambiguous"] {
        let error = build_launch_plan(
            &registry(&[("app.desktop", true)]),
            &report(vec![application("app.desktop")]),
            Some(&topology(fixture)),
            &[],
        )
        .expect_err("non-hybrid topology must be rejected");
        assert!(matches!(error, LauncherError::TopologyNotHybrid { .. }));
    }
}

#[test]
fn hybrid_plan_uses_exact_efficiency_cpus_and_current_exec_data() {
    let mut current = application("meta.desktop");
    current.executable = PathBuf::from("/current/target");
    current.arguments = vec![
        ";".to_owned(),
        "|".to_owned(),
        "&&".to_owned(),
        "$HOME".to_owned(),
        "$(touch)".to_owned(),
    ];
    let plan = build_launch_plan(
        &registry(&[("meta.desktop", true)]),
        &report(vec![current]),
        Some(&topology("intel-hybrid")),
        &[],
    )
    .unwrap_or_else(|error| panic!("hybrid plan: {error}"));
    assert_eq!(plan.efficiency_cpus, [1, 3, 5, 7]);
    assert_eq!(
        plan.applications[0].executable,
        PathBuf::from("/current/target")
    );
    assert_eq!(
        plan.applications[0].arguments,
        [";", "|", "&&", "$HOME", "$(touch)"]
    );
    let mut current_again = application("meta.desktop");
    current_again.executable = PathBuf::from("/current/target");
    current_again.arguments = vec![
        ";".to_owned(),
        "|".to_owned(),
        "&&".to_owned(),
        "$HOME".to_owned(),
        "$(touch)".to_owned(),
    ];
    let repeated = build_launch_plan(
        &registry(&[("meta.desktop", true)]),
        &report(vec![current_again]),
        Some(&topology("intel-hybrid")),
        &[],
    )
    .unwrap_or_else(|error| panic!("repeat plan: {error}"));
    assert_eq!(
        serde_json::to_string(&plan).unwrap(),
        serde_json::to_string(&repeated).unwrap()
    );
}

#[test]
fn fixture_backed_cli_dry_run_is_deterministic_and_does_not_spawn_target() {
    let root = TempDirectory::new("dry-run");
    let data_home = root.path().join("data");
    let applications = data_home.join("applications");
    fs::create_dir_all(&applications)
        .unwrap_or_else(|error| panic!("create applications: {error}"));
    let target = root.path().join("would-not-run");
    let marker = root.path().join("spawned");
    fs::write(&target, format!("#!/bin/sh\ntouch {}\n", marker.display()))
        .unwrap_or_else(|error| panic!("write target: {error}"));
    let mut permissions = fs::metadata(&target)
        .unwrap_or_else(|error| panic!("target metadata: {error}"))
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
    }
    fs::set_permissions(&target, permissions)
        .unwrap_or_else(|error| panic!("set target mode: {error}"));
    fs::write(
        applications.join("dry.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=Dry\nNoDisplay=true\nExec={} ; $HOME\n",
            target.display()
        ),
    )
    .unwrap_or_else(|error| panic!("write desktop entry: {error}"));
    let config = root.path().join("config.toml");
    fs::write(
        &config,
        "schema_version = 1\n[[apps]]\ndesktop_id = \"dry.desktop\"\nname = \"stored\"\nenabled = true\ndelay_seconds = 0\nnice = 5\nio_class = \"best-effort\"\nio_priority = 4\nenforce_process_tree = false\n",
    )
    .unwrap_or_else(|error| panic!("write config: {error}"));
    let binary = env!("CARGO_BIN_EXE_ecore-launcher");
    let run = || {
        Command::new(binary)
            .args([
                "--config",
                config
                    .to_str()
                    .unwrap_or_else(|| panic!("config path utf8")),
                "run",
                "--dry-run",
                "--json",
                "--data-home",
                data_home
                    .to_str()
                    .unwrap_or_else(|| panic!("data path utf8")),
                "--sysfs-root",
                sysfs("intel-hybrid")
                    .to_str()
                    .unwrap_or_else(|| panic!("sysfs path utf8")),
            ])
            .output()
            .unwrap_or_else(|error| panic!("run dry-run: {error}"))
    };
    let first = run();
    let second = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(first.stdout, second.stdout);
    assert!(!marker.exists());
    let json: serde_json::Value = serde_json::from_slice(&first.stdout)
        .unwrap_or_else(|error| panic!("dry-run json: {error}"));
    assert_eq!(json["dry_run"], true);
    assert_eq!(
        json["plan"]["efficiency_cpus"],
        serde_json::json!([1, 3, 5, 7])
    );
    assert_eq!(
        json["plan"]["applications"][0]["arguments"],
        serde_json::json!([";", "$HOME"])
    );
    assert_eq!(json["report"]["initiated"], serde_json::json!([]));
}

#[test]
fn topology_fixture_still_has_expected_hybrid_class() {
    assert_eq!(
        topology("intel-hybrid").classification,
        TopologyClass::Hybrid
    );
}
