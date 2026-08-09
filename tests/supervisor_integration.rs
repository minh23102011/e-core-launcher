use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ecore_launcher::{
    supervise_process_trees, InitiatedApplication, IoPriorityClass, LaunchPlan, LaunchReport,
    PlannedApplication, SupervisorOptions,
};
use nix::sched::{sched_getaffinity, CpuSet};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(test: &str) -> Self {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ecore-launcher-supervisor-{}-{test}-{sequence}",
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
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn first_allowed_cpu() -> u32 {
    let affinity = sched_getaffinity(Pid::this())
        .unwrap_or_else(|error| panic!("read current affinity: {error}"));
    (0..CpuSet::count())
        .find(|cpu| affinity.is_set(*cpu).unwrap_or(false))
        .and_then(|cpu| u32::try_from(cpu).ok())
        .unwrap_or_else(|| panic!("test process needs one allowed CPU"))
}

fn current_affinity() -> Vec<u32> {
    let affinity = sched_getaffinity(Pid::this())
        .unwrap_or_else(|error| panic!("read current affinity: {error}"));
    (0..CpuSet::count())
        .filter(|cpu| affinity.is_set(*cpu).unwrap_or(false))
        .filter_map(|cpu| u32::try_from(cpu).ok())
        .collect()
}

fn process_start_time(pid: u32) -> u64 {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
        .unwrap_or_else(|error| panic!("read process stat: {error}"));
    stat.rsplit_once(") ")
        .and_then(|value| value.1.split_whitespace().nth(19))
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("parse process start time"))
}

fn wait_for(path: &Path, predicate: impl Fn(&str) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(contents) = fs::read_to_string(path) {
            if predicate(&contents) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn supervision_probe_target() {
    let Some(role) = std::env::var_os("ECORE_SUPERVISOR_PROBE_ROLE") else {
        return;
    };
    if role == "root" {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "supervision_probe_target", "--nocapture"])
            .env("ECORE_SUPERVISOR_PROBE_ROLE", "child")
            .status()
            .unwrap_or_else(|error| panic!("spawn probe descendant: {error}"));
        assert!(status.success());
        return;
    }
    thread::sleep(Duration::from_millis(150));
    let output = PathBuf::from(
        std::env::var_os("ECORE_SUPERVISOR_PROBE_OUTPUT")
            .unwrap_or_else(|| panic!("probe output path")),
    );
    fs::write(output, serde_json::to_vec(&current_affinity()).unwrap())
        .unwrap_or_else(|error| panic!("write probe affinity: {error}"));
    thread::sleep(Duration::from_millis(100));
}

#[test]
fn verified_real_descendant_gets_exact_mask_and_unrelated_child_is_untouched() {
    let root = TempDirectory::new("real-tree");
    let output = root.path().join("affinity.json");
    let mut managed = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "supervision_probe_target", "--nocapture"])
        .env("ECORE_SUPERVISOR_PROBE_ROLE", "root")
        .env("ECORE_SUPERVISOR_PROBE_OUTPUT", &output)
        .stdout(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn managed root: {error}"));
    let mut unrelated = Command::new("/bin/sleep")
        .arg("1")
        .spawn()
        .unwrap_or_else(|error| panic!("spawn unrelated child: {error}"));
    let unrelated_before = sched_getaffinity(Pid::from_raw(unrelated.id() as i32)).unwrap();
    let cpu = first_allowed_cpu();
    let plan = LaunchPlan {
        applications: vec![PlannedApplication {
            desktop_id: "managed.desktop".to_owned(),
            name: "Managed".to_owned(),
            executable: std::env::current_exe().unwrap(),
            arguments: Vec::new(),
            delay_seconds: 0,
            nice: 0,
            io_class: IoPriorityClass::None,
            io_priority: None,
            enforce_process_tree: true,
        }],
        efficiency_cpus: vec![cpu],
    };
    let report = LaunchReport {
        initiated: vec![InitiatedApplication {
            desktop_id: "managed.desktop".to_owned(),
            pid: managed.id(),
            process_start_time_ticks: Some(process_start_time(managed.id())),
            exec_succeeded: true,
        }],
        failure: None,
    };
    let supervision = supervise_process_trees(
        &plan,
        &report,
        &SupervisorOptions {
            proc_root: PathBuf::from("/proc"),
            poll_interval: Duration::from_millis(10),
        },
    )
    .unwrap_or_else(|error| panic!("supervise managed tree: {error}"));
    let observed: Vec<u32> = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(observed, [cpu]);
    assert_eq!(supervision.tracked_roots, 1);
    assert_eq!(supervision.completed_roots, 1);
    let unrelated_after = sched_getaffinity(Pid::from_raw(unrelated.id() as i32)).unwrap();
    assert_eq!(unrelated_before, unrelated_after);
    let _ignored = unrelated.kill();
    let _ignored = unrelated.wait();
    let _ignored = managed.try_wait();
}

#[test]
fn supervisor_lifecycle_target() {
    let Some(output) = std::env::var_os("ECORE_SUPERVISOR_LIFECYCLE_OUTPUT") else {
        return;
    };
    let output = PathBuf::from(output);
    fs::write(&output, format!("{}\n", std::process::id())).unwrap();
    thread::sleep(Duration::from_millis(800));
    fs::write(&output, format!("{}\ndone\n", std::process::id())).unwrap();
}

fn synthetic_hybrid(root: &Path, efficiency: u32) -> PathBuf {
    let performance = if efficiency == 0 { 1 } else { 0 };
    let sysfs = root.join("sysfs");
    let mut cpus = [efficiency, performance];
    cpus.sort_unstable();
    fs::create_dir_all(&sysfs).unwrap();
    fs::write(sysfs.join("online"), format!("{},{}\n", cpus[0], cpus[1])).unwrap();
    fs::write(sysfs.join("present"), format!("{},{}\n", cpus[0], cpus[1])).unwrap();
    for (cpu, core_type) in [(efficiency, 32), (performance, 64)] {
        let topology = sysfs.join(format!("cpu{cpu}/topology"));
        fs::create_dir_all(&topology).unwrap();
        fs::write(topology.join("core_type"), format!("{core_type}\n")).unwrap();
        fs::write(topology.join("core_id"), format!("{cpu}\n")).unwrap();
        fs::write(topology.join("physical_package_id"), "0\n").unwrap();
        fs::write(topology.join("thread_siblings_list"), format!("{cpu}\n")).unwrap();
    }
    sysfs
}

#[test]
fn terminating_cli_supervisor_does_not_kill_managed_target() {
    let root = TempDirectory::new("termination");
    let data_home = root.path().join("data");
    let applications = data_home.join("applications");
    fs::create_dir_all(&applications).unwrap();
    let test_binary = std::env::current_exe().unwrap();
    fs::write(
        applications.join("lifecycle.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=Lifecycle\nExec=\"{}\" --exact supervisor_lifecycle_target --nocapture\n",
            test_binary.display()
        ),
    )
    .unwrap();
    let config = root.path().join("config.toml");
    let current_nice = rustix::process::getpriority_process(None)
        .unwrap_or_else(|error| panic!("read current nice: {error}"));
    let target_nice = (current_nice + 1).min(19);
    fs::write(
        &config,
        format!(
            "schema_version = 1\n[[apps]]\ndesktop_id = \"lifecycle.desktop\"\nname = \"Lifecycle\"\nenabled = true\ndelay_seconds = 0\nnice = {target_nice}\nio_class = \"none\"\nenforce_process_tree = true\n"
        ),
    )
    .unwrap();
    let sysfs = synthetic_hybrid(root.path(), first_allowed_cpu());
    let output = root.path().join("lifecycle.txt");
    let mut supervisor = Command::new(env!("CARGO_BIN_EXE_ecore-launcher"))
        .arg("--config")
        .arg(&config)
        .arg("supervise")
        .arg("--data-home")
        .arg(&data_home)
        .arg("--sysfs-root")
        .arg(&sysfs)
        .arg("--poll-interval-ms")
        .arg("10")
        .env("ECORE_SUPERVISOR_LIFECYCLE_OUTPUT", &output)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn CLI supervisor: {error}"));
    wait_for(&output, |contents| contents.lines().next().is_some());
    let target_pid: i32 = fs::read_to_string(&output)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM)
        .unwrap_or_else(|error| panic!("terminate supervisor: {error}"));
    let status = supervisor.wait().unwrap();
    assert!(status.success());
    assert!(Path::new("/proc").join(target_pid.to_string()).exists());
    wait_for(&output, |contents| contents.contains("done"));
}
