//! Fail-closed launch planning and acknowledged execution on detected E-cores.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, NulError, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use ioprio::{BePriorityLevel, Class as IoClass, Priority as IoPriority, RtPriorityLevel, Target};
use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use nix::sched::{sched_getaffinity, sched_setaffinity, CpuSet};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{close, execv, pipe, Pid};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::discovery::DiscoveryReport;
use crate::registry::{validate_registry, AppRegistry, IoPriorityClass, ValidationError};
use crate::topology::{CpuTopology, TopologyClass};

const DEFAULT_ACKNOWLEDGEMENT_TIMEOUT: Duration = Duration::from_secs(5);
const ACKNOWLEDGEMENT_POLL_INTERVAL: Duration = Duration::from_millis(5);
const FAILED_HELPER_REAP_TIMEOUT: Duration = Duration::from_millis(100);
const EXECUTION_REAP_INTERVAL: Duration = Duration::from_millis(100);
const MAX_ACKNOWLEDGEMENT_BYTES: usize = 16 * 1024;

/// A complete, validated request to launch explicitly managed applications.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaunchPlan {
    /// Applications in deterministic desktop-ID order.
    pub applications: Vec<PlannedApplication>,
    /// The exact sorted E-core CPU list detected for this plan.
    pub efficiency_cpus: Vec<u32>,
}

/// Current launch authority paired with validated registry policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlannedApplication {
    /// Stable explicit-registry and desktop-entry ID.
    pub desktop_id: String,
    /// Current desktop-entry display name.
    pub name: String,
    /// Current resolved executable, never registry snapshot metadata.
    pub executable: PathBuf,
    /// Current static arguments after safe Exec parsing.
    pub arguments: Vec<String>,
    /// Startup deadline in seconds relative to the start of this run.
    pub delay_seconds: u64,
    /// Absolute Linux nice value applied by the helper before exec.
    pub nice: i8,
    /// Linux I/O scheduling class applied by the helper before exec.
    pub io_class: IoPriorityClass,
    /// Per-class I/O priority, when required by the selected class.
    pub io_priority: Option<u8>,
    /// Whether supervisor mode must keep verified descendants on this affinity set.
    pub enforce_process_tree: bool,
}

/// One application which successfully crossed the target exec boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InitiatedApplication {
    /// Stable desktop ID.
    pub desktop_id: String,
    /// Helper PID, which became the target PID at successful exec.
    pub pid: u32,
    /// Linux process start time captured immediately after acknowledged exec.
    /// Supervision refuses enrollment when this identity cannot be confirmed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_start_time_ticks: Option<u64>,
    /// Explicit confirmation that helper spawn was not mistaken for target exec.
    pub exec_succeeded: bool,
}

/// Stage at which a real launch failed before a successful target exec.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchFailureStage {
    /// The acknowledgement pipe or helper process could not be created.
    HelperSpawn,
    /// The helper could not apply its E-core affinity.
    Affinity,
    /// The helper could not apply the requested nice value.
    Nice,
    /// The helper could not apply the requested I/O policy.
    IoPriority,
    /// The helper could not directly exec the target.
    Exec,
    /// The helper channel was malformed, closed early, or timed out.
    Acknowledgement,
}

impl std::fmt::Display for LaunchFailureStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::HelperSpawn => "helper_spawn",
            Self::Affinity => "affinity",
            Self::Nice => "nice",
            Self::IoPriority => "io_priority",
            Self::Exec => "exec",
            Self::Acknowledgement => "acknowledgement",
        };
        formatter.write_str(value)
    }
}

/// Structured information about one application which failed before exec.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaunchFailure {
    /// Stable desktop ID.
    pub desktop_id: String,
    /// Helper PID when spawning succeeded.
    pub pid: Option<u32>,
    /// Setup or transition stage which failed.
    pub stage: LaunchFailureStage,
    /// Specific operating-system or protocol diagnostic.
    pub reason: String,
}

impl std::fmt::Display for LaunchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "failed to launch `{}` during {}: {}",
            self.desktop_id, self.stage, self.reason
        )
    }
}

/// Immediate launch-transition results; this never indicates app completion.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaunchReport {
    /// Targets which successfully transitioned through exec, in launch order.
    pub initiated: Vec<InitiatedApplication>,
    /// The first pre-exec failure, if execution stopped partway through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<LaunchFailure>,
}

/// One deterministic launch deadline relative to the start of a run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScheduledLaunch {
    /// Stable desktop ID.
    pub desktop_id: String,
    /// Deadline relative to the common run start.
    pub delay_seconds: u64,
}

/// Runtime execution settings which do not alter launch policy.
#[derive(Clone, Debug)]
pub struct ExecutionOptions {
    /// Executable used for the hidden helper process.
    pub helper_executable: PathBuf,
    /// Maximum time to await setup failure or successful exec transition.
    pub acknowledgement_timeout: Duration,
}

impl ExecutionOptions {
    /// Resolve production defaults using the current executable.
    pub fn for_current_executable() -> Result<Self, LauncherError> {
        Ok(Self {
            helper_executable: std::env::current_exe()
                .map_err(|source| LauncherError::CurrentExecutable { source })?,
            acknowledgement_timeout: DEFAULT_ACKNOWLEDGEMENT_TIMEOUT,
        })
    }
}

/// Errors returned while making or executing a launch plan.
#[derive(Debug, Error)]
pub enum LauncherError {
    /// The supplied registry did not contain valid runtime policy.
    #[error("registry policy is invalid: {error}")]
    InvalidRegistryPolicy { error: ValidationError },
    /// An explicitly requested ID has not been registered.
    #[error("desktop application `{desktop_id}` is not registered")]
    UnknownRegisteredApplication { desktop_id: String },
    /// An explicitly requested ID is not enabled for launching.
    #[error("desktop application `{desktop_id}` is disabled")]
    DisabledApplication { desktop_id: String },
    /// A selected registered desktop ID no longer resolves through discovery.
    #[error("desktop application `{desktop_id}` is not currently discoverable")]
    UnavailableApplication { desktop_id: String },
    /// Terminal applications are deliberately outside the launcher contract.
    #[error("desktop application `{desktop_id}` requires Terminal=true and cannot be launched")]
    TerminalApplication { desktop_id: String },
    /// Conservative topology detection did not confirm a hybrid CPU.
    #[error("reliable E-cores are unavailable: detected topology is {classification}")]
    TopologyNotHybrid { classification: TopologyClass },
    /// A hybrid result without logical E-core IDs is not launchable.
    #[error("reliable E-cores are unavailable: hybrid topology has no efficiency CPUs")]
    EmptyEfficiencyCpus,
    /// The system CPU-set API rejected an E-core CPU ID.
    #[error("CPU {cpu} cannot be represented in the Linux affinity set: {source}")]
    InvalidAffinityCpu { cpu: u32, source: nix::errno::Errno },
    /// A manually constructed plan contained an invalid launch delay.
    #[error("launch delay for `{desktop_id}` cannot be represented safely")]
    InvalidLaunchDelay { desktop_id: String },
    /// The current launcher executable could not be found for the helper.
    #[error("failed to resolve the current launcher executable: {source}")]
    CurrentExecutable {
        #[source]
        source: io::Error,
    },
    /// A planned target contains a NUL byte and cannot be passed to exec.
    #[error("launch {field} contains an interior NUL byte: {source}")]
    InvalidExecArgument {
        field: &'static str,
        #[source]
        source: NulError,
    },
    /// Registry I/O policy was not internally valid.
    #[error("invalid {class} I/O policy with priority {priority:?}")]
    InvalidIoPolicy {
        class: IoPriorityClass,
        priority: Option<u8>,
    },
    /// Applying the affinity set in the helper failed.
    #[error("failed to apply E-core affinity: {source}")]
    ApplyAffinity { source: nix::errno::Errno },
    /// Linux accepted the syscall but retained a different allowed mask.
    #[error("requested E-core affinity {requested:?}, but kernel reported {actual:?}")]
    AffinityMismatch {
        requested: Vec<u32>,
        actual: Vec<u32>,
    },
    /// Applying or verifying the exact nice value failed.
    #[error("failed to apply nice value {nice}: {source}")]
    ApplyNice {
        nice: i8,
        #[source]
        source: io::Error,
    },
    /// The kernel did not retain the exact requested nice value.
    #[error("requested nice value {requested}, but kernel reported {actual}")]
    NiceMismatch { requested: i8, actual: i32 },
    /// Applying or verifying the exact I/O priority failed.
    #[error("failed to apply {class} I/O priority {priority:?}: {source}")]
    ApplyIoPriority {
        class: IoPriorityClass,
        priority: Option<u8>,
        #[source]
        source: io::Error,
    },
    /// The kernel did not retain the exact requested I/O mask.
    #[error("requested I/O priority mask {requested}, but kernel reported {actual}")]
    IoPriorityMismatch { requested: u16, actual: u16 },
    /// Directly replacing the helper with the target failed.
    #[error("failed to exec target {executable}: {source}")]
    Exec {
        executable: PathBuf,
        source: nix::errno::Errno,
    },
    /// The helper could not establish or write its acknowledgement channel.
    #[error("helper acknowledgement channel failed: {source}")]
    HelperAcknowledgement {
        #[source]
        source: io::Error,
    },
    /// One real launch failed; the report preserves earlier exec successes.
    #[error("{failure}")]
    RuntimeLaunchFailed {
        failure: Box<LaunchFailure>,
        report: Box<LaunchReport>,
    },
}

impl LauncherError {
    /// Exec-success records retained before a later runtime failure.
    #[must_use]
    pub fn initiated(&self) -> &[InitiatedApplication] {
        self.launch_report()
            .map_or(&[], |report| report.initiated.as_slice())
    }

    /// Partial report retained by a runtime launch failure.
    #[must_use]
    pub fn launch_report(&self) -> Option<&LaunchReport> {
        match self {
            Self::RuntimeLaunchFailed { report, .. } => Some(report.as_ref()),
            _error => None,
        }
    }

    fn helper_stage(&self) -> LaunchFailureStage {
        match self {
            Self::ApplyAffinity { .. }
            | Self::AffinityMismatch { .. }
            | Self::InvalidAffinityCpu { .. } => LaunchFailureStage::Affinity,
            Self::ApplyNice { .. } | Self::NiceMismatch { .. } => LaunchFailureStage::Nice,
            Self::InvalidIoPolicy { .. }
            | Self::ApplyIoPriority { .. }
            | Self::IoPriorityMismatch { .. } => LaunchFailureStage::IoPriority,
            Self::Exec { .. } | Self::InvalidExecArgument { .. } => LaunchFailureStage::Exec,
            _error => LaunchFailureStage::Acknowledgement,
        }
    }
}

/// Build a full plan before any child process is started.
pub fn build_launch_plan(
    registry: &AppRegistry,
    discovery: &DiscoveryReport,
    topology: Option<&CpuTopology>,
    requested_ids: &[String],
) -> Result<LaunchPlan, LauncherError> {
    validate_registry(registry).map_err(|error| LauncherError::InvalidRegistryPolicy { error })?;
    let selected_ids = select_ids(registry, requested_ids)?;
    if selected_ids.is_empty() {
        return Ok(LaunchPlan {
            applications: Vec::new(),
            efficiency_cpus: Vec::new(),
        });
    }

    let registered: BTreeMap<&str, _> = registry
        .apps
        .iter()
        .map(|application| (application.desktop_id.as_str(), application))
        .collect();
    let current: BTreeMap<&str, _> = discovery
        .applications
        .iter()
        .map(|application| (application.desktop_id.as_str(), application))
        .collect();
    let mut applications = Vec::with_capacity(selected_ids.len());
    for desktop_id in selected_ids {
        let policy = registered.get(desktop_id.as_str()).ok_or_else(|| {
            LauncherError::UnknownRegisteredApplication {
                desktop_id: desktop_id.clone(),
            }
        })?;
        let application = current.get(desktop_id.as_str()).ok_or_else(|| {
            LauncherError::UnavailableApplication {
                desktop_id: desktop_id.clone(),
            }
        })?;
        if application.terminal {
            return Err(LauncherError::TerminalApplication { desktop_id });
        }
        validate_exec_inputs(&application.executable, &application.arguments)?;
        map_io_priority(policy.io_class, policy.io_priority)?;
        applications.push(PlannedApplication {
            desktop_id,
            name: application.name.clone(),
            executable: application.executable.clone(),
            arguments: application.arguments.clone(),
            delay_seconds: policy.delay_seconds,
            nice: policy.nice,
            io_class: policy.io_class,
            io_priority: policy.io_priority,
            enforce_process_tree: policy.enforce_process_tree,
        });
    }

    let topology = topology.ok_or(LauncherError::TopologyNotHybrid {
        classification: TopologyClass::Unknown,
    })?;
    if topology.classification != TopologyClass::Hybrid {
        return Err(LauncherError::TopologyNotHybrid {
            classification: topology.classification,
        });
    }
    if topology.efficiency_cpus.is_empty() {
        return Err(LauncherError::EmptyEfficiencyCpus);
    }
    validate_cpu_set(&topology.efficiency_cpus)?;
    Ok(LaunchPlan {
        applications,
        efficiency_cpus: topology.efficiency_cpus.clone(),
    })
}

fn select_ids(
    registry: &AppRegistry,
    requested_ids: &[String],
) -> Result<Vec<String>, LauncherError> {
    let registered: BTreeMap<&str, _> = registry
        .apps
        .iter()
        .map(|application| (application.desktop_id.as_str(), application))
        .collect();
    let ids: BTreeSet<String> = if requested_ids.is_empty() {
        registry
            .apps
            .iter()
            .filter(|application| application.enabled)
            .map(|application| application.desktop_id.clone())
            .collect()
    } else {
        requested_ids.iter().cloned().collect()
    };
    for desktop_id in &ids {
        let application = registered.get(desktop_id.as_str()).ok_or_else(|| {
            LauncherError::UnknownRegisteredApplication {
                desktop_id: desktop_id.clone(),
            }
        })?;
        if !application.enabled {
            return Err(LauncherError::DisabledApplication {
                desktop_id: desktop_id.clone(),
            });
        }
    }
    Ok(ids.into_iter().collect())
}

/// Return deterministic execution deadlines, ordered by delay and desktop ID.
#[must_use]
pub fn launch_schedule(plan: &LaunchPlan) -> Vec<ScheduledLaunch> {
    let mut schedule: Vec<_> = plan
        .applications
        .iter()
        .map(|application| ScheduledLaunch {
            desktop_id: application.desktop_id.clone(),
            delay_seconds: application.delay_seconds,
        })
        .collect();
    schedule.sort_by(|left, right| {
        (left.delay_seconds, &left.desktop_id).cmp(&(right.delay_seconds, &right.desktop_id))
    });
    schedule
}

/// Execute a plan with the current binary as the acknowledged helper.
pub fn execute_plan(plan: &LaunchPlan) -> Result<LaunchReport, LauncherError> {
    let options = ExecutionOptions::for_current_executable()?;
    execute_plan_with_options(plan, &options)
}

/// Execute a plan with explicit orchestration options.
pub fn execute_plan_with_options(
    plan: &LaunchPlan,
    options: &ExecutionOptions,
) -> Result<LaunchReport, LauncherError> {
    if plan.applications.is_empty() {
        return Ok(LaunchReport::default());
    }
    let by_id: BTreeMap<&str, _> = plan
        .applications
        .iter()
        .map(|application| (application.desktop_id.as_str(), application))
        .collect();
    let started_at = Instant::now();
    let schedule = launch_schedule(plan)
        .into_iter()
        .map(|scheduled| {
            let deadline = started_at
                .checked_add(Duration::from_secs(scheduled.delay_seconds))
                .ok_or_else(|| LauncherError::InvalidLaunchDelay {
                    desktop_id: scheduled.desktop_id.clone(),
                })?;
            Ok((scheduled, deadline))
        })
        .collect::<Result<Vec<_>, LauncherError>>()?;
    let mut report = LaunchReport::default();
    let mut launched_child_pids = BTreeSet::new();
    for (scheduled, deadline) in schedule {
        sleep_until(deadline, &mut launched_child_pids);
        let Some(application) = by_id.get(scheduled.desktop_id.as_str()) else {
            continue;
        };
        match spawn_and_acknowledge(
            &options.helper_executable,
            application,
            &plan.efficiency_cpus,
            options.acknowledgement_timeout,
        ) {
            Ok(initiated) => {
                launched_child_pids.insert(initiated.pid);
                report.initiated.push(initiated);
            }
            Err(failure) => {
                report.failure = Some(failure.clone());
                reap_finished_children(&mut launched_child_pids);
                return Err(LauncherError::RuntimeLaunchFailed {
                    failure: Box::new(failure),
                    report: Box::new(report),
                });
            }
        }
    }
    reap_finished_children(&mut launched_child_pids);
    Ok(report)
}

fn sleep_until(deadline: Instant, launched_child_pids: &mut BTreeSet<u32>) {
    loop {
        reap_finished_children(launched_child_pids);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        thread::sleep(remaining.min(EXECUTION_REAP_INTERVAL));
    }
}

fn reap_finished_children(launched_child_pids: &mut BTreeSet<u32>) {
    launched_child_pids.retain(|pid| {
        let Ok(raw_pid) = i32::try_from(*pid) else {
            return false;
        };
        match waitpid(Pid::from_raw(raw_pid), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => true,
            Ok(_) | Err(nix::errno::Errno::ECHILD) => false,
            Err(_error) => true,
        }
    });
}

fn spawn_and_acknowledge(
    helper: &Path,
    application: &PlannedApplication,
    cpus: &[u32],
    timeout: Duration,
) -> Result<InitiatedApplication, LaunchFailure> {
    let (read_fd, write_fd) = pipe().map_err(|source| LaunchFailure {
        desktop_id: application.desktop_id.clone(),
        pid: None,
        stage: LaunchFailureStage::HelperSpawn,
        reason: format!("failed to create acknowledgement pipe: {source}"),
    })?;
    configure_acknowledgement_reader(&read_fd).map_err(|source| LaunchFailure {
        desktop_id: application.desktop_id.clone(),
        pid: None,
        stage: LaunchFailureStage::HelperSpawn,
        reason: format!("failed to configure acknowledgement pipe: {source}"),
    })?;

    let mut command = Command::new(helper);
    command
        .arg("__exec")
        .arg("--ack-fd")
        .arg(write_fd.as_raw_fd().to_string())
        .arg("--nice")
        .arg(application.nice.to_string())
        .arg("--io-class")
        .arg(application.io_class.to_string());
    if let Some(priority) = application.io_priority {
        command.arg("--io-priority").arg(priority.to_string());
    }
    for cpu in cpus {
        command.arg("--cpu").arg(cpu.to_string());
    }
    command
        .arg("--")
        .arg(&application.executable)
        .args(&application.arguments);

    let mut child = command.spawn().map_err(|source| LaunchFailure {
        desktop_id: application.desktop_id.clone(),
        pid: None,
        stage: LaunchFailureStage::HelperSpawn,
        reason: source.to_string(),
    })?;
    let pid = child.id();
    drop(write_fd);
    match wait_for_acknowledgement(File::from(read_fd), timeout) {
        Ok(()) => Ok(InitiatedApplication {
            desktop_id: application.desktop_id.clone(),
            pid,
            process_start_time_ticks: crate::supervisor::process_start_time_at(
                Path::new("/proc"),
                pid,
            )
            .ok()
            .flatten(),
            exec_succeeded: true,
        }),
        Err(error) => {
            terminate_failed_helper(&mut child);
            Err(LaunchFailure {
                desktop_id: application.desktop_id.clone(),
                pid: Some(pid),
                stage: error.stage,
                reason: error.reason,
            })
        }
    }
}

fn configure_acknowledgement_reader(read_fd: &impl AsFd) -> Result<(), nix::errno::Errno> {
    let raw_fd = read_fd.as_fd().as_raw_fd();
    fcntl(raw_fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    let current = fcntl(raw_fd, FcntlArg::F_GETFL)?;
    let flags = OFlag::from_bits_truncate(current);
    fcntl(raw_fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok(())
}

fn terminate_failed_helper(child: &mut Child) {
    let _kill_result = child.kill();
    let now = Instant::now();
    let deadline = match now.checked_add(FAILED_HELPER_REAP_TIMEOUT) {
        Some(deadline) => deadline,
        None => now,
    };
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if Instant::now() >= deadline => return,
            Ok(None) => thread::sleep(ACKNOWLEDGEMENT_POLL_INTERVAL),
        }
    }
}

#[derive(Debug)]
struct AcknowledgementFailure {
    stage: LaunchFailureStage,
    reason: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum AcknowledgementMessage {
    Ready,
    Failure {
        stage: LaunchFailureStage,
        reason: String,
    },
}

fn wait_for_acknowledgement(
    mut reader: File,
    timeout: Duration,
) -> Result<(), AcknowledgementFailure> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| AcknowledgementFailure {
            stage: LaunchFailureStage::Acknowledgement,
            reason: "helper acknowledgement timeout cannot be represented safely".to_owned(),
        })?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return interpret_acknowledgement(&bytes),
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.len() > MAX_ACKNOWLEDGEMENT_BYTES {
                    return Err(AcknowledgementFailure {
                        stage: LaunchFailureStage::Acknowledgement,
                        reason: "helper acknowledgement exceeded the protocol size limit"
                            .to_owned(),
                    });
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(AcknowledgementFailure {
                        stage: LaunchFailureStage::Acknowledgement,
                        reason: format!(
                            "helper did not acknowledge exec within {} ms",
                            timeout.as_millis()
                        ),
                    });
                }
                thread::sleep(remaining.min(ACKNOWLEDGEMENT_POLL_INTERVAL));
            }
            Err(error) => {
                return Err(AcknowledgementFailure {
                    stage: LaunchFailureStage::Acknowledgement,
                    reason: format!("failed to read helper acknowledgement: {error}"),
                });
            }
        }
    }
}

fn interpret_acknowledgement(bytes: &[u8]) -> Result<(), AcknowledgementFailure> {
    let input = std::str::from_utf8(bytes).map_err(|error| AcknowledgementFailure {
        stage: LaunchFailureStage::Acknowledgement,
        reason: format!("helper acknowledgement was not UTF-8: {error}"),
    })?;
    let mut ready = false;
    for line in input.lines().filter(|line| !line.is_empty()) {
        let message: AcknowledgementMessage =
            serde_json::from_str(line).map_err(|error| AcknowledgementFailure {
                stage: LaunchFailureStage::Acknowledgement,
                reason: format!("invalid helper acknowledgement: {error}"),
            })?;
        match message {
            AcknowledgementMessage::Ready if !ready => ready = true,
            AcknowledgementMessage::Ready => {
                return Err(AcknowledgementFailure {
                    stage: LaunchFailureStage::Acknowledgement,
                    reason: "helper sent more than one ready message".to_owned(),
                });
            }
            AcknowledgementMessage::Failure { stage, reason } => {
                return Err(AcknowledgementFailure { stage, reason });
            }
        }
    }
    if ready {
        Ok(())
    } else {
        Err(AcknowledgementFailure {
            stage: LaunchFailureStage::Acknowledgement,
            reason: "helper closed acknowledgement channel before setup completed".to_owned(),
        })
    }
}

/// Apply all runtime policy to this helper and directly exec the target.
///
/// The acknowledgement descriptor is inherited from the parent. Its safe
/// duplicate is close-on-exec, so EOF after `Ready` proves an exec transition.
pub fn run_exec_helper(
    cpus: &[u32],
    nice: i8,
    io_class: IoPriorityClass,
    io_priority: Option<u8>,
    executable: &Path,
    arguments: &[OsString],
    acknowledgement_fd: RawFd,
) -> Result<(), LauncherError> {
    let mut acknowledgement = open_acknowledgement(acknowledgement_fd)?;
    let target = match prepare_exec_target(executable, arguments) {
        Ok(target) => target,
        Err(error) => {
            let _write_result = write_acknowledgement(
                &mut acknowledgement,
                &AcknowledgementMessage::Failure {
                    stage: LaunchFailureStage::Exec,
                    reason: error.to_string(),
                },
            );
            return Err(error);
        }
    };
    let setup = apply_affinity(cpus)
        .and_then(|()| apply_nice(nice))
        .and_then(|()| apply_io_policy(io_class, io_priority));
    if let Err(error) = setup {
        let _write_result = write_acknowledgement(
            &mut acknowledgement,
            &AcknowledgementMessage::Failure {
                stage: error.helper_stage(),
                reason: error.to_string(),
            },
        );
        return Err(error);
    }
    write_acknowledgement(&mut acknowledgement, &AcknowledgementMessage::Ready)?;
    match exec_prepared_target(&target) {
        Err(error) => {
            let _write_result = write_acknowledgement(
                &mut acknowledgement,
                &AcknowledgementMessage::Failure {
                    stage: LaunchFailureStage::Exec,
                    reason: error.to_string(),
                },
            );
            Err(error)
        }
        Ok(never) => match never {},
    }
}

fn open_acknowledgement(raw_fd: RawFd) -> Result<File, LauncherError> {
    if raw_fd < 3 {
        return Err(LauncherError::HelperAcknowledgement {
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "acknowledgement descriptor must be at least 3",
            ),
        });
    }
    let path = PathBuf::from(format!("/proc/self/fd/{raw_fd}"));
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|source| LauncherError::HelperAcknowledgement { source })?;
    fcntl(file.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(|source| {
        LauncherError::HelperAcknowledgement {
            source: io_error_from_errno(source),
        }
    })?;
    close(raw_fd).map_err(|source| LauncherError::HelperAcknowledgement {
        source: io_error_from_errno(source),
    })?;
    Ok(file)
}

fn write_acknowledgement(
    writer: &mut File,
    message: &AcknowledgementMessage,
) -> Result<(), LauncherError> {
    let mut bytes =
        serde_json::to_vec(message).map_err(|source| LauncherError::HelperAcknowledgement {
            source: io::Error::new(io::ErrorKind::InvalidData, source),
        })?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .and_then(|()| writer.flush())
        .map_err(|source| LauncherError::HelperAcknowledgement { source })
}

fn apply_affinity(cpus: &[u32]) -> Result<(), LauncherError> {
    let cpu_set = build_cpu_set(cpus)?;
    sched_setaffinity(Pid::this(), &cpu_set)
        .map_err(|source| LauncherError::ApplyAffinity { source })?;
    let actual =
        sched_getaffinity(Pid::this()).map_err(|source| LauncherError::ApplyAffinity { source })?;
    if cpu_sets_equal(&cpu_set, &actual) {
        Ok(())
    } else {
        Err(LauncherError::AffinityMismatch {
            requested: cpu_set_values(&cpu_set),
            actual: cpu_set_values(&actual),
        })
    }
}

fn apply_nice(nice: i8) -> Result<(), LauncherError> {
    rustix::process::setpriority_process(None, i32::from(nice)).map_err(|source| {
        LauncherError::ApplyNice {
            nice,
            source: io::Error::from_raw_os_error(source.raw_os_error()),
        }
    })?;
    let actual =
        rustix::process::getpriority_process(None).map_err(|source| LauncherError::ApplyNice {
            nice,
            source: io::Error::from_raw_os_error(source.raw_os_error()),
        })?;
    if actual == i32::from(nice) {
        Ok(())
    } else {
        Err(LauncherError::NiceMismatch {
            requested: nice,
            actual,
        })
    }
}

fn apply_io_policy(class: IoPriorityClass, priority: Option<u8>) -> Result<(), LauncherError> {
    let Some(requested) = map_io_priority(class, priority)? else {
        return Ok(());
    };
    let target = Target::Process(ioprio::Pid::from_raw(0));
    ioprio::set_priority(target, requested).map_err(|source| LauncherError::ApplyIoPriority {
        class,
        priority,
        source: io::Error::other(source),
    })?;
    let actual = ioprio::get_priority(target).map_err(|source| LauncherError::ApplyIoPriority {
        class,
        priority,
        source: io::Error::other(source),
    })?;
    if actual == requested {
        Ok(())
    } else {
        Err(LauncherError::IoPriorityMismatch {
            requested: requested.inner(),
            actual: actual.inner(),
        })
    }
}

fn map_io_priority(
    class: IoPriorityClass,
    priority: Option<u8>,
) -> Result<Option<IoPriority>, LauncherError> {
    let mapped = match (class, priority) {
        (IoPriorityClass::None, None) => return Ok(None),
        (IoPriorityClass::Idle, None) => IoPriority::new(IoClass::Idle),
        (IoPriorityClass::BestEffort, Some(level)) => {
            let level = BePriorityLevel::from_level(level)
                .ok_or(LauncherError::InvalidIoPolicy { class, priority })?;
            IoPriority::new(IoClass::BestEffort(level))
        }
        (IoPriorityClass::Realtime, Some(level)) => {
            let level = RtPriorityLevel::from_level(level)
                .ok_or(LauncherError::InvalidIoPolicy { class, priority })?;
            IoPriority::new(IoClass::Realtime(level))
        }
        _policy => return Err(LauncherError::InvalidIoPolicy { class, priority }),
    };
    Ok(Some(mapped))
}

struct PreparedExecTarget {
    executable: CString,
    arguments: Vec<CString>,
}

fn prepare_exec_target(
    executable: &Path,
    arguments: &[OsString],
) -> Result<PreparedExecTarget, LauncherError> {
    let executable = cstring_from_os(executable.as_os_str(), "executable")?;
    let mut exec_arguments = Vec::with_capacity(arguments.len() + 1);
    exec_arguments.push(executable.clone());
    for argument in arguments {
        exec_arguments.push(cstring_from_os(argument, "argument")?);
    }
    Ok(PreparedExecTarget {
        executable,
        arguments: exec_arguments,
    })
}

fn exec_prepared_target(
    target: &PreparedExecTarget,
) -> Result<std::convert::Infallible, LauncherError> {
    execv(&target.executable, &target.arguments).map_err(|source| LauncherError::Exec {
        executable: executable_path(&target.executable),
        source,
    })
}

/// Apply only E-core affinity and directly exec a target.
///
/// This preserves the affinity-only library entry point; runtime launches use
/// [`run_exec_helper`] so all effective policy and acknowledgement is applied.
pub fn exec_with_affinity(
    cpus: &[u32],
    executable: &Path,
    arguments: &[OsString],
) -> Result<(), LauncherError> {
    let target = prepare_exec_target(executable, arguments)?;
    apply_affinity(cpus)?;
    match exec_prepared_target(&target) {
        Err(error) => Err(error),
        Ok(never) => match never {},
    }
}

fn executable_path(executable: &CString) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(executable.as_bytes()))
}

fn validate_exec_inputs(executable: &Path, arguments: &[String]) -> Result<(), LauncherError> {
    cstring_from_os(executable.as_os_str(), "executable")?;
    for argument in arguments {
        CString::new(argument.as_bytes()).map_err(|source| LauncherError::InvalidExecArgument {
            field: "argument",
            source,
        })?;
    }
    Ok(())
}

fn cstring_from_os(value: &OsStr, field: &'static str) -> Result<CString, LauncherError> {
    CString::new(value.as_bytes())
        .map_err(|source| LauncherError::InvalidExecArgument { field, source })
}

fn validate_cpu_set(cpus: &[u32]) -> Result<(), LauncherError> {
    build_cpu_set(cpus).map(|_set| ())
}

fn build_cpu_set(cpus: &[u32]) -> Result<CpuSet, LauncherError> {
    if cpus.is_empty() {
        return Err(LauncherError::EmptyEfficiencyCpus);
    }
    let mut cpu_set = CpuSet::new();
    for cpu in cpus {
        cpu_set
            .set(*cpu as usize)
            .map_err(|source| LauncherError::InvalidAffinityCpu { cpu: *cpu, source })?;
    }
    Ok(cpu_set)
}

fn cpu_sets_equal(left: &CpuSet, right: &CpuSet) -> bool {
    (0..CpuSet::count()).all(|cpu| left.is_set(cpu).ok() == right.is_set(cpu).ok())
}

fn cpu_set_values(set: &CpuSet) -> Vec<u32> {
    (0..CpuSet::count())
        .filter(|cpu| set.is_set(*cpu).unwrap_or(false))
        .filter_map(|cpu| u32::try_from(cpu).ok())
        .collect()
}

fn io_error_from_errno(errno: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(errno as i32)
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use super::{
        apply_io_policy, build_cpu_set, configure_acknowledgement_reader, launch_schedule,
        map_io_priority, wait_for_acknowledgement, LaunchPlan, PlannedApplication,
    };
    use crate::registry::IoPriorityClass;

    #[test]
    fn affinity_set_rejects_empty_and_out_of_range_cpu_ids() {
        assert!(build_cpu_set(&[]).is_err());
        assert!(build_cpu_set(&[u32::MAX]).is_err());
    }

    #[test]
    fn schedule_uses_non_cumulative_deadlines_and_deterministic_ties() {
        let plan = LaunchPlan {
            applications: vec![
                planned("c.desktop", 5),
                planned("b.desktop", 2),
                planned("a.desktop", 0),
                planned("aa.desktop", 2),
            ],
            efficiency_cpus: vec![1],
        };
        let schedule = launch_schedule(&plan);
        assert_eq!(
            schedule
                .iter()
                .map(|entry| (entry.desktop_id.as_str(), entry.delay_seconds))
                .collect::<Vec<_>>(),
            [
                ("a.desktop", 0),
                ("aa.desktop", 2),
                ("b.desktop", 2),
                ("c.desktop", 5)
            ]
        );
    }

    #[test]
    fn io_classes_map_exactly_and_none_does_not_change_current_policy() {
        assert!(map_io_priority(IoPriorityClass::None, None)
            .unwrap_or_else(|error| panic!("map none: {error}"))
            .is_none());
        assert_eq!(
            map_io_priority(IoPriorityClass::BestEffort, Some(3))
                .unwrap_or_else(|error| panic!("map best effort: {error}"))
                .and_then(ioprio::Priority::class),
            Some(ioprio::Class::BestEffort(
                ioprio::BePriorityLevel::from_level(3)
                    .unwrap_or_else(|| panic!("valid best-effort level"))
            ))
        );
        assert_eq!(
            map_io_priority(IoPriorityClass::Realtime, Some(6))
                .unwrap_or_else(|error| panic!("map realtime: {error}"))
                .and_then(ioprio::Priority::class),
            Some(ioprio::Class::Realtime(
                ioprio::RtPriorityLevel::from_level(6)
                    .unwrap_or_else(|| panic!("valid realtime level"))
            ))
        );
        assert_eq!(
            map_io_priority(IoPriorityClass::Idle, None)
                .unwrap_or_else(|error| panic!("map idle: {error}"))
                .and_then(ioprio::Priority::class),
            Some(ioprio::Class::Idle)
        );
        let target = ioprio::Target::Process(ioprio::Pid::from_raw(0));
        let before = ioprio::get_priority(target)
            .unwrap_or_else(|error| panic!("get I/O priority before none: {error}"));
        apply_io_policy(IoPriorityClass::None, None)
            .unwrap_or_else(|error| panic!("apply none: {error}"));
        let after = ioprio::get_priority(target)
            .unwrap_or_else(|error| panic!("get I/O priority after none: {error}"));
        assert_eq!(before, after);
    }

    #[test]
    fn acknowledgement_wait_is_bounded_when_writer_never_responds() {
        let (read_fd, write_fd) = nix::unistd::pipe()
            .unwrap_or_else(|error| panic!("create acknowledgement pipe: {error}"));
        configure_acknowledgement_reader(&read_fd)
            .unwrap_or_else(|error| panic!("configure acknowledgement pipe: {error}"));
        assert!(write_fd.as_raw_fd() >= 0);
        let started = std::time::Instant::now();
        let error = wait_for_acknowledgement(
            std::fs::File::from(read_fd),
            std::time::Duration::from_millis(20),
        )
        .expect_err("silent helper must time out");
        assert!(error.reason.contains("did not acknowledge exec"));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        drop(write_fd);
    }

    fn planned(desktop_id: &str, delay_seconds: u64) -> PlannedApplication {
        PlannedApplication {
            desktop_id: desktop_id.to_owned(),
            name: desktop_id.to_owned(),
            executable: "/bin/true".into(),
            arguments: Vec::new(),
            delay_seconds,
            nice: 5,
            io_class: IoPriorityClass::None,
            io_priority: None,
            enforce_process_tree: false,
        }
    }
}
