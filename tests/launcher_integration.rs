use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ecore_launcher::{
    build_launch_plan, execute_plan_with_options, AppRegistry, CpuTopology, CpuTopologyDetector,
    DiscoveredApplication, DiscoveryReport, ExecutionOptions, IoPriorityClass, LaunchFailureStage,
    LaunchPlan, LauncherError, PlannedApplication, RegisteredApplication, TopologyClass,
};
use nix::sched::{sched_getaffinity, CpuSet};
use nix::unistd::Pid;

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

fn current_affinity() -> CpuSet {
    sched_getaffinity(Pid::this())
        .unwrap_or_else(|error| panic!("read current CPU affinity: {error}"))
}

fn first_available_cpu() -> u32 {
    let affinity = current_affinity();
    (0..CpuSet::count())
        .find(|cpu| affinity.is_set(*cpu).unwrap_or(false))
        .and_then(|cpu| u32::try_from(cpu).ok())
        .unwrap_or_else(|| panic!("at least one representable CPU must be available"))
}

fn first_unavailable_cpu() -> Option<u32> {
    let affinity = current_affinity();
    (0..CpuSet::count())
        .find(|cpu| !affinity.is_set(*cpu).unwrap_or(true))
        .and_then(|cpu| u32::try_from(cpu).ok())
}

fn synthetic_hybrid(root: &Path, efficiency_cpu: u32) -> PathBuf {
    let performance_cpu = if efficiency_cpu == 0 { 1 } else { 0 };
    let sysfs = root.join("synthetic-sysfs");
    let mut cpus = [performance_cpu, efficiency_cpu];
    cpus.sort_unstable();
    let cpu_list = format!("{},{}\n", cpus[0], cpus[1]);
    fs::create_dir_all(&sysfs).unwrap_or_else(|error| panic!("create synthetic sysfs: {error}"));
    fs::write(sysfs.join("online"), &cpu_list)
        .unwrap_or_else(|error| panic!("write synthetic online CPUs: {error}"));
    fs::write(sysfs.join("present"), &cpu_list)
        .unwrap_or_else(|error| panic!("write synthetic present CPUs: {error}"));
    for (cpu, core_type, core_id) in [
        (performance_cpu, "64\n", 0_u32),
        (efficiency_cpu, "32\n", 1_u32),
    ] {
        let topology = sysfs.join(format!("cpu{cpu}/topology"));
        fs::create_dir_all(&topology)
            .unwrap_or_else(|error| panic!("create CPU {cpu} topology: {error}"));
        fs::write(topology.join("core_type"), core_type)
            .unwrap_or_else(|error| panic!("write CPU {cpu} core type: {error}"));
        fs::write(topology.join("core_id"), format!("{core_id}\n"))
            .unwrap_or_else(|error| panic!("write CPU {cpu} core ID: {error}"));
        fs::write(topology.join("physical_package_id"), "0\n")
            .unwrap_or_else(|error| panic!("write CPU {cpu} package: {error}"));
        fs::write(topology.join("thread_siblings_list"), format!("{cpu}\n"))
            .unwrap_or_else(|error| panic!("write CPU {cpu} siblings: {error}"));
    }
    let detected = CpuTopologyDetector::new(&sysfs)
        .detect()
        .unwrap_or_else(|error| panic!("detect synthetic hybrid topology: {error}"));
    assert_eq!(detected.classification, TopologyClass::Hybrid);
    assert_eq!(detected.efficiency_cpus, [efficiency_cpu]);
    sysfs
}

fn current_nice_for_safe_lowering() -> i8 {
    let current = rustix::process::getpriority_process(None)
        .unwrap_or_else(|error| panic!("read current nice value: {error}"));
    i8::try_from((current + 1).min(19))
        .unwrap_or_else(|error| panic!("convert safe nice value: {error}"))
}

fn create_runtime_files(
    root: &TempDirectory,
    desktop_entries: &[(&str, String)],
    config_contents: &str,
) -> (PathBuf, PathBuf) {
    let data_home = root.path().join("data");
    let applications = data_home.join("applications");
    fs::create_dir_all(&applications)
        .unwrap_or_else(|error| panic!("create runtime applications: {error}"));
    for (desktop_id, exec) in desktop_entries {
        fs::write(
            applications.join(desktop_id),
            format!("[Desktop Entry]\nType=Application\nName={desktop_id}\nExec={exec}\n"),
        )
        .unwrap_or_else(|error| panic!("write {desktop_id}: {error}"));
    }
    let config = root.path().join("config.toml");
    fs::write(&config, config_contents)
        .unwrap_or_else(|error| panic!("write runtime config: {error}"));
    (config, data_home)
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "timed out waiting for {}", path.display());
}

fn json_from_mixed_stdout(stdout: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(stdout);
    let start = text
        .find('{')
        .unwrap_or_else(|| panic!("JSON object missing from stdout: {text}"));
    serde_json::from_str(&text[start..])
        .unwrap_or_else(|error| panic!("parse launcher JSON from {text:?}: {error}"))
}

#[test]
fn runtime_policy_probe_target() {
    let Some(output) = std::env::var_os("ECORE_LAUNCHER_PROBE_OUTPUT") else {
        return;
    };
    let affinity = current_affinity();
    let cpus: Vec<_> = (0..CpuSet::count())
        .filter(|cpu| affinity.is_set(*cpu).unwrap_or(false))
        .collect();
    let nice = rustix::process::getpriority_process(None)
        .unwrap_or_else(|error| panic!("probe nice value: {error}"));
    let io_priority = ioprio::get_priority(ioprio::Target::Process(ioprio::Pid::from_raw(0)))
        .unwrap_or_else(|error| panic!("probe I/O priority: {error}"));
    let io_class = match io_priority.class() {
        Some(ioprio::Class::Realtime(_level)) => "realtime",
        Some(ioprio::Class::BestEffort(_level)) => "best-effort",
        Some(ioprio::Class::Idle) => "idle",
        None => "none",
    };
    fs::write(
        PathBuf::from(output),
        serde_json::to_vec(&serde_json::json!({
            "cpus": cpus,
            "nice": nice,
            "io_class": io_class,
            "io_mask": io_priority.inner()
        }))
        .unwrap_or_else(|error| panic!("serialize probe state: {error}")),
    )
    .unwrap_or_else(|error| panic!("write probe state: {error}"));
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
fn launch_plan_combines_fresh_exec_authority_with_registry_policy() {
    let mut registry = registry(&[("policy.desktop", true)]);
    let stored = registry
        .apps
        .first_mut()
        .unwrap_or_else(|| panic!("registered policy app"));
    stored.name = "stale registry name".to_owned();
    stored.desktop_file = Some(PathBuf::from("/stale/not-authority.desktop"));
    stored.delay_seconds = 9;
    stored.nice = 11;
    stored.io_class = IoPriorityClass::Realtime;
    stored.io_priority = Some(6);
    stored.enforce_process_tree = true;

    let mut current = application("policy.desktop");
    current.name = "Fresh Name".to_owned();
    current.executable = PathBuf::from("/fresh/executable");
    current.arguments = vec![";".to_owned(), "$HOME".to_owned()];
    let plan = build_launch_plan(
        &registry,
        &report(vec![current]),
        Some(&topology("intel-hybrid")),
        &[],
    )
    .unwrap_or_else(|error| panic!("build policy plan: {error}"));
    let planned = &plan.applications[0];
    assert_eq!(planned.name, "Fresh Name");
    assert_eq!(planned.executable, PathBuf::from("/fresh/executable"));
    assert_eq!(planned.arguments, [";", "$HOME"]);
    assert_eq!(planned.delay_seconds, 9);
    assert_eq!(planned.nice, 11);
    assert_eq!(planned.io_class, IoPriorityClass::Realtime);
    assert_eq!(planned.io_priority, Some(6));
    assert!(planned.enforce_process_tree);
}

#[test]
fn helper_spawn_failure_is_not_reported_as_exec_success() {
    let plan = LaunchPlan {
        applications: vec![PlannedApplication {
            desktop_id: "missing-helper.desktop".to_owned(),
            name: "Missing helper".to_owned(),
            executable: PathBuf::from("/bin/true"),
            arguments: Vec::new(),
            delay_seconds: 0,
            nice: 0,
            io_class: IoPriorityClass::None,
            io_priority: None,
            enforce_process_tree: false,
        }],
        efficiency_cpus: vec![0],
    };
    let options = ExecutionOptions {
        helper_executable: PathBuf::from("/definitely/missing/ecore-launcher-helper"),
        acknowledgement_timeout: Duration::from_millis(50),
    };
    let error = execute_plan_with_options(&plan, &options)
        .expect_err("missing helper executable must fail");
    let report = error
        .launch_report()
        .unwrap_or_else(|| panic!("runtime failure report"));
    assert!(report.initiated.is_empty());
    assert_eq!(
        report
            .failure
            .as_ref()
            .unwrap_or_else(|| panic!("spawn failure"))
            .stage,
        LaunchFailureStage::HelperSpawn
    );
}

#[test]
fn helper_applies_affinity_nice_and_idle_io_before_acknowledged_exec() {
    let root = TempDirectory::new("runtime-policy");
    let efficiency_cpu = first_available_cpu();
    let sysfs = synthetic_hybrid(root.path(), efficiency_cpu);
    let desired_nice = current_nice_for_safe_lowering();
    let test_target = std::env::current_exe()
        .unwrap_or_else(|error| panic!("resolve integration test executable: {error}"));
    let exec = format!(
        "\"{}\" --exact runtime_policy_probe_target",
        test_target.display()
    );
    let config_contents = format!(
        "schema_version = 1\n[[apps]]\ndesktop_id = \"probe.desktop\"\nname = \"stored\"\nenabled = true\ndelay_seconds = 0\nnice = {desired_nice}\nio_class = \"idle\"\nenforce_process_tree = false\n"
    );
    let (config, data_home) =
        create_runtime_files(&root, &[("probe.desktop", exec)], &config_contents);
    let probe = root.path().join("probe.json");
    let output = Command::new(env!("CARGO_BIN_EXE_ecore-launcher"))
        .env("ECORE_LAUNCHER_PROBE_OUTPUT", &probe)
        .args([
            "--config",
            config
                .to_str()
                .unwrap_or_else(|| panic!("runtime config path UTF-8")),
            "run",
            "--json",
            "--data-home",
            data_home
                .to_str()
                .unwrap_or_else(|| panic!("runtime data path UTF-8")),
            "--sysfs-root",
            sysfs
                .to_str()
                .unwrap_or_else(|| panic!("runtime sysfs path UTF-8")),
        ])
        .output()
        .unwrap_or_else(|error| panic!("run acknowledged policy target: {error}"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_for_file(&probe, Duration::from_secs(2));
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(&probe).unwrap_or_else(|error| panic!("read probe state: {error}")),
    )
    .unwrap_or_else(|error| panic!("parse probe state: {error}"));
    assert_eq!(state["cpus"], serde_json::json!([efficiency_cpu]));
    assert_eq!(state["nice"], desired_nice);
    assert_eq!(state["io_class"], "idle");
    assert_eq!(
        state["io_mask"],
        ioprio::Priority::new(ioprio::Class::Idle).inner()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"initiated\""));
    assert!(stdout.contains("probe.desktop"));
}

#[test]
fn helper_setup_failure_reaches_parent_and_target_never_execs() {
    let Some(unavailable_cpu) = first_unavailable_cpu() else {
        return;
    };
    let root = TempDirectory::new("setup-failure");
    let sysfs = synthetic_hybrid(root.path(), unavailable_cpu);
    let test_target = std::env::current_exe()
        .unwrap_or_else(|error| panic!("resolve integration test executable: {error}"));
    let exec = format!(
        "\"{}\" --exact runtime_policy_probe_target",
        test_target.display()
    );
    let desired_nice = current_nice_for_safe_lowering();
    let config_contents = format!(
        "schema_version = 1\n[[apps]]\ndesktop_id = \"failure.desktop\"\nname = \"stored\"\nenabled = true\ndelay_seconds = 0\nnice = {desired_nice}\nio_class = \"none\"\nenforce_process_tree = false\n"
    );
    let (config, data_home) =
        create_runtime_files(&root, &[("failure.desktop", exec)], &config_contents);
    let marker = root.path().join("must-not-exec.json");
    let output = Command::new(env!("CARGO_BIN_EXE_ecore-launcher"))
        .env("ECORE_LAUNCHER_PROBE_OUTPUT", &marker)
        .args([
            "--config",
            config
                .to_str()
                .unwrap_or_else(|| panic!("failure config path UTF-8")),
            "run",
            "--json",
            "--data-home",
            data_home
                .to_str()
                .unwrap_or_else(|| panic!("failure data path UTF-8")),
            "--sysfs-root",
            sysfs
                .to_str()
                .unwrap_or_else(|| panic!("failure sysfs path UTF-8")),
        ])
        .output()
        .unwrap_or_else(|error| panic!("run setup failure target: {error}"));
    assert!(!output.status.success());
    assert!(!marker.exists());
    let json = json_from_mixed_stdout(&output.stdout);
    assert_eq!(json["report"]["initiated"], serde_json::json!([]));
    assert_eq!(json["report"]["failure"]["desktop_id"], "failure.desktop");
    assert_eq!(json["report"]["failure"]["stage"], "affinity");
}

#[test]
fn exec_failure_reports_earlier_success_and_delay_is_relative_to_run_start() {
    let root = TempDirectory::new("partial-exec-failure");
    let efficiency_cpu = first_available_cpu();
    let sysfs = synthetic_hybrid(root.path(), efficiency_cpu);
    let test_target = std::env::current_exe()
        .unwrap_or_else(|error| panic!("resolve integration test executable: {error}"));
    let first_exec = format!(
        "\"{}\" --exact runtime_policy_probe_target",
        test_target.display()
    );
    let disappearing_target = root.path().join("disappearing-target");
    symlink("/bin/true", &disappearing_target)
        .unwrap_or_else(|error| panic!("create disappearing target: {error}"));
    let desired_nice = current_nice_for_safe_lowering();
    let config_contents = format!(
        "schema_version = 1\n[[apps]]\ndesktop_id = \"a.desktop\"\nname = \"first\"\nenabled = true\ndelay_seconds = 0\nnice = {desired_nice}\nio_class = \"none\"\nenforce_process_tree = false\n[[apps]]\ndesktop_id = \"z.desktop\"\nname = \"second\"\nenabled = true\ndelay_seconds = 2\nnice = {desired_nice}\nio_class = \"none\"\nenforce_process_tree = false\n"
    );
    let (config, data_home) = create_runtime_files(
        &root,
        &[
            ("a.desktop", first_exec),
            (
                "z.desktop",
                disappearing_target.to_string_lossy().into_owned(),
            ),
        ],
        &config_contents,
    );
    let first_marker = root.path().join("first.json");
    let started = Instant::now();
    let child = Command::new(env!("CARGO_BIN_EXE_ecore-launcher"))
        .env("ECORE_LAUNCHER_PROBE_OUTPUT", &first_marker)
        .args([
            "--config",
            config
                .to_str()
                .unwrap_or_else(|| panic!("partial config path UTF-8")),
            "run",
            "--json",
            "--data-home",
            data_home
                .to_str()
                .unwrap_or_else(|| panic!("partial data path UTF-8")),
            "--sysfs-root",
            sysfs
                .to_str()
                .unwrap_or_else(|| panic!("partial sysfs path UTF-8")),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn partial launch: {error}"));
    wait_for_file(&first_marker, Duration::from_secs(1));
    fs::remove_file(&disappearing_target)
        .unwrap_or_else(|error| panic!("remove disappearing target: {error}"));
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("wait for partial launch: {error}"));
    assert!(!output.status.success());
    assert!(started.elapsed() >= Duration::from_millis(1_800));
    let json = json_from_mixed_stdout(&output.stdout);
    assert_eq!(json["report"]["initiated"][0]["desktop_id"], "a.desktop");
    assert_eq!(json["report"]["initiated"][0]["exec_succeeded"], true);
    assert_eq!(json["report"]["failure"]["desktop_id"], "z.desktop");
    assert_eq!(json["report"]["failure"]["stage"], "exec");
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
        "schema_version = 1\n[[apps]]\ndesktop_id = \"dry.desktop\"\nname = \"stored\"\nenabled = true\ndelay_seconds = 3600\nnice = 12\nio_class = \"realtime\"\nio_priority = 3\nenforce_process_tree = true\n",
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
    assert_eq!(json["plan"]["applications"][0]["delay_seconds"], 3600);
    assert_eq!(json["plan"]["applications"][0]["nice"], 12);
    assert_eq!(json["plan"]["applications"][0]["io_class"], "realtime");
    assert_eq!(json["plan"]["applications"][0]["io_priority"], 3);
    assert_eq!(
        json["plan"]["applications"][0]["enforce_process_tree"],
        true
    );
    assert_eq!(json["report"]["initiated"], serde_json::json!([]));
    assert!(json["report"].get("failure").is_none());
}

#[test]
fn topology_fixture_still_has_expected_hybrid_class() {
    assert_eq!(
        topology("intel-hybrid").classification,
        TopologyClass::Hybrid
    );
}
