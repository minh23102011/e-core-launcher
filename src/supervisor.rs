//! Lightweight affinity supervision for process trees launched by this process.

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use nix::errno::Errno;
use nix::sched::{sched_getaffinity, sched_setaffinity, CpuSet};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::launcher::{InitiatedApplication, LaunchPlan, LaunchReport};

/// Production polling cadence. The supervisor performs no work between polls.
pub const DEFAULT_SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MINIMUM_SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_PROCESSES_PER_TREE: usize = 65_536;
const MAX_SUPERVISOR_WARNINGS: usize = 1_024;

/// Runtime-only settings for process-tree supervision.
#[derive(Clone, Debug)]
pub struct SupervisorOptions {
    /// Procfs root, normally `/proc`; injectable for diagnostics and tests.
    pub proc_root: PathBuf,
    /// Delay between bounded tree walks.
    pub poll_interval: Duration,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            poll_interval: DEFAULT_SUPERVISOR_POLL_INTERVAL,
        }
    }
}

/// One non-fatal condition encountered while a managed tree changed concurrently.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SupervisorWarning {
    /// Registered application associated with the tree.
    pub desktop_id: String,
    /// Process involved, when known.
    pub pid: Option<u32>,
    /// Conservative diagnostic; the process was not mutated on uncertainty.
    pub reason: String,
}

/// Deterministic summary produced when every enforce-enabled managed tree is gone.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SupervisionReport {
    /// Roots accepted for monitoring after confirmed target exec.
    pub tracked_roots: usize,
    /// Roots which ended or no longer matched their enrollment identity.
    pub completed_roots: usize,
    /// Number of polling passes which actually had a tracked root.
    pub polls: u64,
    /// Thread affinity sets changed because they differed from the plan.
    pub affinity_updates: u64,
    /// Non-fatal races and inaccessible metadata, in observation order.
    pub warnings: Vec<SupervisorWarning>,
    /// Graceful termination signal, when supervision stopped before every root ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination_signal: Option<String>,
}

/// Errors which prevent safe supervision from continuing.
#[derive(Debug, Error)]
pub enum SupervisorError {
    /// A cadence below this bound would permit accidental busy polling.
    #[error("supervisor poll interval must be at least {minimum_ms} ms")]
    PollIntervalTooShort { minimum_ms: u128 },
    /// The plan's affinity mask could not be represented.
    #[error("invalid supervisor affinity CPU {cpu}: {source}")]
    InvalidAffinityCpu { cpu: u32, source: Errno },
    /// A process identifier exceeded Linux's signed PID representation.
    #[error("process ID {pid} cannot be represented by the Linux process API")]
    InvalidPid { pid: u32 },
    /// A verified process could not be inspected conservatively.
    #[error("failed to inspect verified process {pid}: {source}")]
    InspectProcess {
        pid: u32,
        #[source]
        source: io::Error,
    },
    /// Affinity for a verified process/thread could not be read or changed.
    #[error("failed to enforce affinity for verified thread {pid}: {source}")]
    EnforceAffinity { pid: u32, source: Errno },
    /// Linux retained a mask other than the exact plan mask.
    #[error("verified thread {pid} retained affinity {actual:?}, expected {requested:?}")]
    AffinityMismatch {
        pid: u32,
        requested: Vec<u32>,
        actual: Vec<u32>,
    },
    /// A supervised plan must always carry at least one E-core CPU.
    #[error("supervisor affinity set is empty")]
    EmptyAffinity,
    /// Enforce-enabled launch reports must carry the identity captured at exec.
    #[error("managed target `{desktop_id}` PID {pid} has no captured Linux start time")]
    MissingLaunchIdentity { desktop_id: String, pid: u32 },
    /// Enrollment requires the helper's confirmed transition through target exec.
    #[error("managed target `{desktop_id}` PID {pid} has no confirmed exec transition")]
    UnconfirmedExec { desktop_id: String, pid: u32 },
    /// Procfs metadata is mandatory whenever a managed tree needs enforcement.
    #[error("procfs root {root} is unavailable for supervision: {reason}")]
    ProcfsUnavailable { root: PathBuf, reason: String },
    /// Signal setup/read failure prevented a reliable supervisor lifecycle.
    #[error("failed to {operation} supervisor termination signals: {source}")]
    SignalHandling {
        operation: &'static str,
        source: Errno,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ProcessIdentity {
    pid: u32,
    start_time_ticks: u64,
}

#[derive(Clone, Debug)]
struct ManagedTree {
    desktop_id: String,
    root: ProcessIdentity,
    cpus: Vec<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AffinityResult {
    AlreadyCorrect,
    Updated(u64),
    Vanished,
}

trait ProcessControl {
    fn identity(&self, pid: u32) -> io::Result<Option<(ProcessIdentity, u32)>>;
    fn children(&self, parent: ProcessIdentity) -> io::Result<Vec<ProcessIdentity>>;
    fn ensure_affinity(
        &self,
        process: ProcessIdentity,
        cpus: &[u32],
    ) -> Result<AffinityResult, SupervisorError>;
    fn reap_child(&self, pid: u32) -> bool;
}

#[derive(Clone, Debug)]
struct ProcfsProcessControl {
    root: PathBuf,
}

impl ProcfsProcessControl {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn process_path(&self, pid: u32) -> PathBuf {
        self.root.join(pid.to_string())
    }

    fn task_ids(&self, process: ProcessIdentity) -> io::Result<Vec<u32>> {
        if self.identity(process.pid)?.map(|value| value.0) != Some(process) {
            return Ok(Vec::new());
        }
        let mut tids = Vec::new();
        let task_root = self.process_path(process.pid).join("task");
        for entry in match fs::read_dir(task_root) {
            Ok(entries) => entries,
            Err(error) if is_vanished(&error) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        } {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if is_vanished(&error) => continue,
                Err(error) => return Err(error),
            };
            if let Some(tid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            {
                tids.push(tid);
                if tids.len() > MAX_PROCESSES_PER_TREE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("process has more than {MAX_PROCESSES_PER_TREE} threads"),
                    ));
                }
            }
        }
        tids.sort_unstable();
        tids.dedup();
        Ok(tids)
    }

    fn task_belongs_to(&self, pid: u32, tid: u32) -> io::Result<bool> {
        let status = match fs::read_to_string(
            self.process_path(pid)
                .join("task")
                .join(tid.to_string())
                .join("status"),
        ) {
            Ok(status) => status,
            Err(error) if is_vanished(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        Ok(status.lines().find_map(|line| {
            line.strip_prefix("Tgid:")
                .and_then(|value| value.trim().parse::<u32>().ok())
        }) == Some(pid))
    }
}

impl ProcessControl for ProcfsProcessControl {
    fn identity(&self, pid: u32) -> io::Result<Option<(ProcessIdentity, u32)>> {
        let stat = match fs::read_to_string(self.process_path(pid).join("stat")) {
            Ok(stat) => stat,
            Err(error) if is_vanished(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        parse_proc_stat(pid, &stat).map(Some)
    }

    fn children(&self, parent: ProcessIdentity) -> io::Result<Vec<ProcessIdentity>> {
        if self.identity(parent.pid)?.map(|value| value.0) != Some(parent) {
            return Ok(Vec::new());
        }
        let mut child_pids = BTreeSet::new();
        for tid in self.task_ids(parent)? {
            let path = self
                .process_path(parent.pid)
                .join("task")
                .join(tid.to_string())
                .join("children");
            let contents = match fs::read_to_string(path) {
                Ok(contents) => contents,
                Err(error) if is_vanished(&error) => continue,
                Err(error) => return Err(error),
            };
            for child_pid in contents
                .split_whitespace()
                .filter_map(|value| value.parse::<u32>().ok())
            {
                child_pids.insert(child_pid);
                if child_pids.len() > MAX_PROCESSES_PER_TREE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("process has more than {MAX_PROCESSES_PER_TREE} direct children"),
                    ));
                }
            }
        }
        if self.identity(parent.pid)?.map(|value| value.0) != Some(parent) {
            return Ok(Vec::new());
        }
        let mut children = Vec::new();
        for pid in child_pids {
            let Some((identity, ppid)) = self.identity(pid)? else {
                continue;
            };
            if ppid == parent.pid && identity.start_time_ticks >= parent.start_time_ticks {
                children.push(identity);
            }
        }
        Ok(children)
    }

    fn ensure_affinity(
        &self,
        process: ProcessIdentity,
        cpus: &[u32],
    ) -> Result<AffinityResult, SupervisorError> {
        let current =
            self.identity(process.pid)
                .map_err(|source| SupervisorError::InspectProcess {
                    pid: process.pid,
                    source,
                })?;
        if current.map(|value| value.0) != Some(process) {
            return Ok(AffinityResult::Vanished);
        }
        let expected = cpu_set(cpus)?;
        let tids = self
            .task_ids(process)
            .map_err(|source| SupervisorError::InspectProcess {
                pid: process.pid,
                source,
            })?;
        if tids.is_empty() {
            return Ok(AffinityResult::Vanished);
        }
        let mut updates = 0_u64;
        for tid in tids {
            let current =
                self.identity(process.pid)
                    .map_err(|source| SupervisorError::InspectProcess {
                        pid: process.pid,
                        source,
                    })?;
            let belongs = self
                .task_belongs_to(process.pid, tid)
                .map_err(|source| SupervisorError::InspectProcess { pid: tid, source })?;
            if current.map(|value| value.0) != Some(process) || !belongs {
                continue;
            }
            let pid = linux_pid(tid)?;
            let actual = match sched_getaffinity(pid) {
                Ok(actual) => actual,
                Err(Errno::ESRCH) => continue,
                Err(source) => return Err(SupervisorError::EnforceAffinity { pid: tid, source }),
            };
            if cpu_sets_equal(&actual, &expected) {
                continue;
            }
            match sched_setaffinity(pid, &expected) {
                Ok(()) => {
                    let retained = sched_getaffinity(pid)
                        .map_err(|source| SupervisorError::EnforceAffinity { pid: tid, source })?;
                    if !cpu_sets_equal(&retained, &expected) {
                        return Err(SupervisorError::AffinityMismatch {
                            pid: tid,
                            requested: cpu_set_values(&expected),
                            actual: cpu_set_values(&retained),
                        });
                    }
                    updates = updates.saturating_add(1);
                }
                Err(Errno::ESRCH) => continue,
                Err(source) => return Err(SupervisorError::EnforceAffinity { pid: tid, source }),
            }
        }
        if updates == 0 {
            Ok(AffinityResult::AlreadyCorrect)
        } else {
            Ok(AffinityResult::Updated(updates))
        }
    }

    fn reap_child(&self, pid: u32) -> bool {
        let Ok(pid) = i32::try_from(pid).map(Pid::from_raw) else {
            return true;
        };
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => false,
            Ok(_) | Err(Errno::ECHILD) => true,
            Err(_error) => false,
        }
    }
}

struct Supervisor<C> {
    control: C,
    trees: Vec<ManagedTree>,
    child_pids: BTreeSet<u32>,
    report: SupervisionReport,
    warning_keys: BTreeSet<(String, Option<u32>, String)>,
    warnings_truncated: bool,
}

struct TerminationSignals {
    descriptor: SignalFd,
    previous_mask: SigSet,
}

impl TerminationSignals {
    fn new() -> Result<Self, SupervisorError> {
        let previous_mask =
            SigSet::thread_get_mask().map_err(|source| SupervisorError::SignalHandling {
                operation: "read the current mask for",
                source,
            })?;
        let mut mask = SigSet::empty();
        mask.add(Signal::SIGINT);
        mask.add(Signal::SIGTERM);
        mask.thread_block()
            .map_err(|source| SupervisorError::SignalHandling {
                operation: "block",
                source,
            })?;
        let descriptor =
            match SignalFd::with_flags(&mask, SfdFlags::SFD_CLOEXEC | SfdFlags::SFD_NONBLOCK) {
                Ok(descriptor) => descriptor,
                Err(source) => {
                    let _restore_result = previous_mask.thread_set_mask();
                    return Err(SupervisorError::SignalHandling {
                        operation: "open a descriptor for",
                        source,
                    });
                }
            };
        Ok(Self {
            descriptor,
            previous_mask,
        })
    }

    fn received(&self) -> Result<Option<&'static str>, SupervisorError> {
        self.descriptor
            .read_signal()
            .map(|signal| {
                signal.and_then(|signal| match signal.ssi_signo as i32 {
                    value if value == Signal::SIGINT as i32 => Some("SIGINT"),
                    value if value == Signal::SIGTERM as i32 => Some("SIGTERM"),
                    _other => None,
                })
            })
            .map_err(|source| SupervisorError::SignalHandling {
                operation: "read",
                source,
            })
    }
}

impl Drop for TerminationSignals {
    fn drop(&mut self) {
        let _restore_result = self.previous_mask.thread_set_mask();
    }
}

impl<C: ProcessControl> Supervisor<C> {
    fn poll_once(&mut self) -> Result<(), SupervisorError> {
        if self.trees.is_empty() {
            return Ok(());
        }
        self.report.polls = self.report.polls.saturating_add(1);
        self.child_pids.retain(|pid| !self.control.reap_child(*pid));
        let mut retained = Vec::with_capacity(self.trees.len());
        for tree in std::mem::take(&mut self.trees) {
            let root = match self.control.identity(tree.root.pid) {
                Ok(Some((identity, _ppid))) if identity == tree.root => identity,
                Ok(_missing_or_reused) => {
                    self.report.completed_roots = self.report.completed_roots.saturating_add(1);
                    continue;
                }
                Err(error) => {
                    self.warn(SupervisorWarning {
                        desktop_id: tree.desktop_id.clone(),
                        pid: Some(tree.root.pid),
                        reason: format!("could not verify managed root: {error}"),
                    });
                    retained.push(tree);
                    continue;
                }
            };
            let mut queue = VecDeque::from([root]);
            let mut visited = BTreeSet::new();
            let mut known = BTreeSet::from([root]);
            while let Some(process) = queue.pop_front() {
                if !visited.insert(process) {
                    continue;
                }
                match self.control.ensure_affinity(process, &tree.cpus) {
                    Ok(AffinityResult::Updated(count)) => {
                        self.report.affinity_updates =
                            self.report.affinity_updates.saturating_add(count);
                    }
                    Ok(AffinityResult::AlreadyCorrect | AffinityResult::Vanished) => {}
                    Err(error) => return Err(error),
                }
                match self.control.children(process) {
                    Ok(children) => {
                        for child in children {
                            if known.contains(&child) {
                                continue;
                            }
                            if known.len() >= MAX_PROCESSES_PER_TREE {
                                self.warn(SupervisorWarning {
                                    desktop_id: tree.desktop_id.clone(),
                                    pid: Some(tree.root.pid),
                                    reason: format!(
                                        "managed tree exceeded the safety bound of {MAX_PROCESSES_PER_TREE} processes"
                                    ),
                                });
                                break;
                            }
                            known.insert(child);
                            queue.push_back(child);
                        }
                    }
                    Err(error) if is_vanished(&error) => {}
                    Err(error) => self.warn(SupervisorWarning {
                        desktop_id: tree.desktop_id.clone(),
                        pid: Some(process.pid),
                        reason: format!("could not inspect verified descendants: {error}"),
                    }),
                }
            }
            retained.push(tree);
        }
        self.trees = retained;
        Ok(())
    }

    fn warn(&mut self, warning: SupervisorWarning) {
        if self.warnings_truncated {
            return;
        }
        let key = (
            warning.desktop_id.clone(),
            warning.pid,
            warning.reason.clone(),
        );
        if !self.warning_keys.insert(key) {
            return;
        }
        if self.report.warnings.len() < MAX_SUPERVISOR_WARNINGS {
            self.report.warnings.push(warning);
        } else if !self.warnings_truncated {
            self.warnings_truncated = true;
            self.report.warnings.push(SupervisorWarning {
                desktop_id: "supervisor".to_owned(),
                pid: None,
                reason: format!(
                    "additional warnings omitted after the {MAX_SUPERVISOR_WARNINGS}-warning safety bound"
                ),
            });
        }
    }
}

/// Monitor enforce-enabled applications from one launch report until their
/// verified roots disappear. Stopping this function never signals a target.
pub fn supervise_process_trees(
    plan: &LaunchPlan,
    launch_report: &LaunchReport,
    options: &SupervisorOptions,
) -> Result<SupervisionReport, SupervisorError> {
    if options.poll_interval < MINIMUM_SUPERVISOR_POLL_INTERVAL {
        return Err(SupervisorError::PollIntervalTooShort {
            minimum_ms: MINIMUM_SUPERVISOR_POLL_INTERVAL.as_millis(),
        });
    }
    let enforce_ids: BTreeSet<&str> = plan
        .applications
        .iter()
        .filter(|application| application.enforce_process_tree)
        .map(|application| application.desktop_id.as_str())
        .collect();
    let enforce_initiated: Vec<_> = launch_report
        .initiated
        .iter()
        .filter(|initiated| enforce_ids.contains(initiated.desktop_id.as_str()))
        .collect();
    if !enforce_initiated.is_empty() {
        if let Some(initiated) = enforce_initiated
            .iter()
            .find(|initiated| !initiated.exec_succeeded)
        {
            return Err(SupervisorError::UnconfirmedExec {
                desktop_id: initiated.desktop_id.clone(),
                pid: initiated.pid,
            });
        }
        validate_procfs_root(&options.proc_root)?;
        if let Some(initiated) = enforce_initiated
            .iter()
            .find(|initiated| initiated.process_start_time_ticks.is_none())
        {
            return Err(SupervisorError::MissingLaunchIdentity {
                desktop_id: initiated.desktop_id.clone(),
                pid: initiated.pid,
            });
        }
    }
    let control = ProcfsProcessControl::new(options.proc_root.clone());
    let mut report = SupervisionReport::default();
    let mut trees = Vec::new();
    for application in &plan.applications {
        if !application.enforce_process_tree {
            continue;
        }
        let Some(initiated) = launch_report
            .initiated
            .iter()
            .find(|initiated| initiated.desktop_id == application.desktop_id)
        else {
            continue;
        };
        enroll_root(
            &control,
            initiated,
            &plan.efficiency_cpus,
            &mut trees,
            &mut report,
        );
    }
    report.tracked_roots = trees.len();
    let warnings_truncated = report.warnings.len() > MAX_SUPERVISOR_WARNINGS;
    if warnings_truncated {
        report.warnings.truncate(MAX_SUPERVISOR_WARNINGS);
        report.warnings.push(SupervisorWarning {
            desktop_id: "supervisor".to_owned(),
            pid: None,
            reason: format!(
                "additional warnings omitted after the {MAX_SUPERVISOR_WARNINGS}-warning safety bound"
            ),
        });
    }
    let warning_keys = report
        .warnings
        .iter()
        .map(|warning| {
            (
                warning.desktop_id.clone(),
                warning.pid,
                warning.reason.clone(),
            )
        })
        .collect();
    let mut supervisor = Supervisor {
        control,
        trees,
        child_pids: launch_report
            .initiated
            .iter()
            .map(|initiated| initiated.pid)
            .collect(),
        report,
        warning_keys,
        warnings_truncated,
    };
    let termination_signals = if supervisor.trees.is_empty() {
        None
    } else {
        Some(TerminationSignals::new()?)
    };
    while !supervisor.trees.is_empty() {
        if let Some(signal) = termination_signals
            .as_ref()
            .map(TerminationSignals::received)
            .transpose()?
            .flatten()
        {
            supervisor.report.termination_signal = Some(signal.to_owned());
            break;
        }
        supervisor.poll_once()?;
        if !supervisor.trees.is_empty() {
            thread::sleep(options.poll_interval);
        }
    }
    Ok(supervisor.report)
}

fn enroll_root<C: ProcessControl>(
    control: &C,
    initiated: &InitiatedApplication,
    cpus: &[u32],
    trees: &mut Vec<ManagedTree>,
    report: &mut SupervisionReport,
) {
    let Some(expected_start_time) = initiated.process_start_time_ticks else {
        push_enrollment_warning(
            report,
            SupervisorWarning {
                desktop_id: initiated.desktop_id.clone(),
                pid: Some(initiated.pid),
                reason: "target process identity was unavailable after exec; refusing supervision"
                    .to_owned(),
            },
        );
        return;
    };
    match control.identity(initiated.pid) {
        Ok(Some((root, _ppid))) if root.start_time_ticks == expected_start_time => {
            trees.push(ManagedTree {
                desktop_id: initiated.desktop_id.clone(),
                root,
                cpus: cpus.to_vec(),
            });
        }
        Ok(Some((_root, _ppid))) => push_enrollment_warning(
            report,
            SupervisorWarning {
                desktop_id: initiated.desktop_id.clone(),
                pid: Some(initiated.pid),
                reason:
                    "target PID start time changed before enrollment; refusing possible PID reuse"
                        .to_owned(),
            },
        ),
        Ok(None) => push_enrollment_warning(
            report,
            SupervisorWarning {
                desktop_id: initiated.desktop_id.clone(),
                pid: Some(initiated.pid),
                reason: "target exited before process-tree supervision could enroll it".to_owned(),
            },
        ),
        Err(error) => push_enrollment_warning(
            report,
            SupervisorWarning {
                desktop_id: initiated.desktop_id.clone(),
                pid: Some(initiated.pid),
                reason: format!("could not enroll managed root: {error}"),
            },
        ),
    }
}

fn push_enrollment_warning(report: &mut SupervisionReport, warning: SupervisorWarning) {
    if report.warnings.len() < MAX_SUPERVISOR_WARNINGS {
        report.warnings.push(warning);
    } else if report.warnings.len() == MAX_SUPERVISOR_WARNINGS {
        report.warnings.push(SupervisorWarning {
            desktop_id: "supervisor".to_owned(),
            pid: None,
            reason: format!(
                "additional warnings omitted after the {MAX_SUPERVISOR_WARNINGS}-warning safety bound"
            ),
        });
    }
}

fn validate_procfs_root(root: &Path) -> Result<(), SupervisorError> {
    match fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_metadata) => {
            return Err(SupervisorError::ProcfsUnavailable {
                root: root.to_owned(),
                reason: "path is not a directory".to_owned(),
            })
        }
        Err(error) => {
            return Err(SupervisorError::ProcfsUnavailable {
                root: root.to_owned(),
                reason: error.to_string(),
            })
        }
    }
    fs::read_to_string(root.join("self/stat")).map_err(|error| {
        SupervisorError::ProcfsUnavailable {
            root: root.to_owned(),
            reason: format!("self process metadata cannot be read: {error}"),
        }
    })?;
    fs::read_to_string(
        root.join("self/task")
            .join(std::process::id().to_string())
            .join("children"),
    )
    .map(|_children| ())
    .map_err(|error| SupervisorError::ProcfsUnavailable {
        root: root.to_owned(),
        reason: format!("self task child metadata cannot be read: {error}"),
    })
}

pub(crate) fn process_start_time_at(root: &Path, pid: u32) -> io::Result<Option<u64>> {
    ProcfsProcessControl::new(root.to_owned())
        .identity(pid)
        .map(|identity| identity.map(|value| value.0.start_time_ticks))
}

fn parse_proc_stat(pid: u32, stat: &str) -> io::Result<(ProcessIdentity, u32)> {
    let suffix = stat.rsplit_once(") ").map(|value| value.1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "process stat has no command terminator",
        )
    })?;
    let fields: Vec<&str> = suffix.split_whitespace().collect();
    let ppid = fields
        .get(1)
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "process stat has no valid PPID")
        })?;
    let start_time_ticks = fields
        .get(19)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "process stat has no valid start time",
            )
        })?;
    Ok((
        ProcessIdentity {
            pid,
            start_time_ticks,
        },
        ppid,
    ))
}

fn cpu_set(cpus: &[u32]) -> Result<CpuSet, SupervisorError> {
    if cpus.is_empty() {
        return Err(SupervisorError::EmptyAffinity);
    }
    let mut set = CpuSet::new();
    for cpu in cpus {
        set.set(*cpu as usize)
            .map_err(|source| SupervisorError::InvalidAffinityCpu { cpu: *cpu, source })?;
    }
    Ok(set)
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

fn linux_pid(pid: u32) -> Result<Pid, SupervisorError> {
    i32::try_from(pid)
        .map(Pid::from_raw)
        .map_err(|_error| SupervisorError::InvalidPid { pid })
}

fn is_vanished(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    use super::{
        AffinityResult, ManagedTree, ProcessControl, ProcessIdentity, SupervisionReport,
        Supervisor, SupervisorError,
    };

    #[derive(Clone, Debug)]
    struct FakeProcess {
        start: u64,
        ppid: u32,
        children: Vec<u32>,
        correct: bool,
    }

    #[derive(Default)]
    struct FakeControl {
        processes: RefCell<BTreeMap<u32, FakeProcess>>,
        mutations: RefCell<Vec<(u32, Vec<u32>)>>,
        reaped: RefCell<Vec<u32>>,
        affinity_failures: RefCell<BTreeSet<u32>>,
    }

    impl FakeControl {
        fn add(&self, pid: u32, start: u64, ppid: u32, children: Vec<u32>, correct: bool) {
            self.processes.borrow_mut().insert(
                pid,
                FakeProcess {
                    start,
                    ppid,
                    children,
                    correct,
                },
            );
        }
    }

    impl ProcessControl for FakeControl {
        fn identity(&self, pid: u32) -> std::io::Result<Option<(ProcessIdentity, u32)>> {
            Ok(self.processes.borrow().get(&pid).map(|process| {
                (
                    ProcessIdentity {
                        pid,
                        start_time_ticks: process.start,
                    },
                    process.ppid,
                )
            }))
        }

        fn children(&self, parent: ProcessIdentity) -> std::io::Result<Vec<ProcessIdentity>> {
            let processes = self.processes.borrow();
            let Some(process) = processes.get(&parent.pid) else {
                return Ok(Vec::new());
            };
            if process.start != parent.start_time_ticks {
                return Ok(Vec::new());
            }
            Ok(process
                .children
                .iter()
                .filter_map(|pid| {
                    processes
                        .get(pid)
                        .filter(|child| child.ppid == parent.pid)
                        .map(|child| ProcessIdentity {
                            pid: *pid,
                            start_time_ticks: child.start,
                        })
                })
                .collect())
        }

        fn ensure_affinity(
            &self,
            process: ProcessIdentity,
            cpus: &[u32],
        ) -> Result<AffinityResult, SupervisorError> {
            if self.affinity_failures.borrow().contains(&process.pid) {
                return Err(SupervisorError::InvalidPid { pid: process.pid });
            }
            let mut processes = self.processes.borrow_mut();
            let Some(current) = processes.get_mut(&process.pid) else {
                return Ok(AffinityResult::Vanished);
            };
            if current.start != process.start_time_ticks {
                return Ok(AffinityResult::Vanished);
            }
            if current.correct {
                Ok(AffinityResult::AlreadyCorrect)
            } else {
                current.correct = true;
                self.mutations
                    .borrow_mut()
                    .push((process.pid, cpus.to_vec()));
                Ok(AffinityResult::Updated(1))
            }
        }

        fn reap_child(&self, pid: u32) -> bool {
            self.reaped.borrow_mut().push(pid);
            false
        }
    }

    fn supervisor(control: FakeControl, enforce: bool) -> Supervisor<FakeControl> {
        let trees = if enforce {
            vec![ManagedTree {
                desktop_id: "managed.desktop".to_owned(),
                root: ProcessIdentity {
                    pid: 10,
                    start_time_ticks: 100,
                },
                cpus: vec![2, 3],
            }]
        } else {
            Vec::new()
        };
        Supervisor {
            control,
            trees,
            child_pids: BTreeSet::new(),
            report: SupervisionReport::default(),
            warning_keys: BTreeSet::new(),
            warnings_truncated: false,
        }
    }

    #[test]
    fn disabled_enforcement_performs_no_process_work() {
        let control = FakeControl::default();
        control.add(10, 100, 1, vec![11], false);
        control.add(11, 101, 10, Vec::new(), false);
        let mut supervisor = supervisor(control, false);
        supervisor.poll_once().unwrap();
        assert_eq!(supervisor.report.polls, 0);
        assert!(supervisor.control.mutations.borrow().is_empty());
    }

    #[test]
    fn verified_descendants_are_changed_but_unrelated_processes_are_untouched() {
        let control = FakeControl::default();
        control.add(10, 100, 1, vec![11], true);
        control.add(11, 101, 10, vec![12], false);
        control.add(12, 102, 11, Vec::new(), false);
        control.add(99, 103, 1, Vec::new(), false);
        let mut supervisor = supervisor(control, true);
        supervisor.poll_once().unwrap();
        assert_eq!(
            *supervisor.control.mutations.borrow(),
            [(11, vec![2, 3]), (12, vec![2, 3])]
        );
        assert_eq!(supervisor.report.affinity_updates, 2);
    }

    #[test]
    fn correct_affinity_is_not_rewritten_and_disappearance_is_benign() {
        let control = FakeControl::default();
        control.add(10, 100, 1, vec![11], true);
        control.add(11, 101, 10, Vec::new(), true);
        let mut supervisor = supervisor(control, true);
        supervisor.poll_once().unwrap();
        assert!(supervisor.control.mutations.borrow().is_empty());
        supervisor.control.processes.borrow_mut().remove(&11);
        supervisor.poll_once().unwrap();
        assert!(supervisor.report.warnings.is_empty());
    }

    #[test]
    fn root_pid_reuse_or_completion_stops_tracking() {
        let control = FakeControl::default();
        control.add(10, 200, 1, Vec::new(), false);
        let mut supervisor = supervisor(control, true);
        supervisor.poll_once().unwrap();
        assert!(supervisor.trees.is_empty());
        assert_eq!(supervisor.report.completed_roots, 1);
        assert!(supervisor.control.mutations.borrow().is_empty());
    }

    #[test]
    fn distinct_runtime_warnings_remain_bounded() {
        let mut supervisor = supervisor(FakeControl::default(), false);
        for pid in 0..(super::MAX_SUPERVISOR_WARNINGS as u32 + 100) {
            supervisor.warn(super::SupervisorWarning {
                desktop_id: "managed.desktop".to_owned(),
                pid: Some(pid),
                reason: "synthetic warning".to_owned(),
            });
        }
        assert_eq!(
            supervisor.report.warnings.len(),
            super::MAX_SUPERVISOR_WARNINGS + 1
        );
        assert_eq!(
            supervisor.warning_keys.len(),
            super::MAX_SUPERVISOR_WARNINGS + 1
        );
        assert!(supervisor.warnings_truncated);
    }

    #[test]
    fn no_enforced_application_returns_without_procfs_or_polling() {
        use crate::launcher::{LaunchPlan, LaunchReport, PlannedApplication};
        use crate::registry::IoPriorityClass;
        use std::path::PathBuf;
        use std::time::Duration;

        let plan = LaunchPlan {
            applications: vec![PlannedApplication {
                desktop_id: "one-shot.desktop".to_owned(),
                name: "One Shot".to_owned(),
                executable: PathBuf::from("/bin/true"),
                arguments: Vec::new(),
                delay_seconds: 0,
                nice: 0,
                io_class: IoPriorityClass::None,
                io_priority: None,
                enforce_process_tree: false,
            }],
            efficiency_cpus: vec![1],
        };
        let report = super::supervise_process_trees(
            &plan,
            &LaunchReport::default(),
            &super::SupervisorOptions {
                proc_root: PathBuf::from("/definitely/missing/proc"),
                poll_interval: Duration::from_millis(10),
            },
        )
        .unwrap_or_else(|error| panic!("empty supervision: {error}"));
        assert_eq!(report, super::SupervisionReport::default());
    }

    #[test]
    fn busy_loop_intervals_are_rejected_before_monitoring() {
        use crate::launcher::{LaunchPlan, LaunchReport};
        use std::path::PathBuf;
        use std::time::Duration;

        let error = super::supervise_process_trees(
            &LaunchPlan {
                applications: Vec::new(),
                efficiency_cpus: Vec::new(),
            },
            &LaunchReport::default(),
            &super::SupervisorOptions {
                proc_root: PathBuf::from("/proc"),
                poll_interval: Duration::ZERO,
            },
        )
        .expect_err("zero-duration polling must be rejected");
        assert!(matches!(
            error,
            super::SupervisorError::PollIntervalTooShort { .. }
        ));
    }

    #[test]
    fn enrollment_rejects_missing_or_changed_launch_identity() {
        use crate::launcher::{InitiatedApplication, LaunchPlan, LaunchReport, PlannedApplication};
        use crate::registry::IoPriorityClass;
        use std::path::PathBuf;
        use std::time::Duration;

        let plan = LaunchPlan {
            applications: vec![PlannedApplication {
                desktop_id: "identity.desktop".to_owned(),
                name: "Identity".to_owned(),
                executable: PathBuf::from("/bin/true"),
                arguments: Vec::new(),
                delay_seconds: 0,
                nice: 0,
                io_class: IoPriorityClass::None,
                io_priority: None,
                enforce_process_tree: true,
            }],
            efficiency_cpus: vec![0],
        };
        let options = super::SupervisorOptions {
            proc_root: PathBuf::from("/proc"),
            poll_interval: Duration::from_millis(10),
        };
        let launch_report = |start_time| LaunchReport {
            initiated: vec![InitiatedApplication {
                desktop_id: "identity.desktop".to_owned(),
                pid: std::process::id(),
                process_start_time_ticks: start_time,
                exec_succeeded: true,
            }],
            failure: None,
        };
        assert!(matches!(
            super::supervise_process_trees(&plan, &launch_report(None), &options),
            Err(super::SupervisorError::MissingLaunchIdentity { .. })
        ));
        let mut unconfirmed = launch_report(Some(0));
        unconfirmed.initiated[0].exec_succeeded = false;
        assert!(matches!(
            super::supervise_process_trees(&plan, &unconfirmed, &options),
            Err(super::SupervisorError::UnconfirmedExec { .. })
        ));
        let report = super::supervise_process_trees(&plan, &launch_report(Some(0)), &options)
            .unwrap_or_else(|error| panic!("identity reuse rejection: {error}"));
        assert_eq!(report.tracked_roots, 0);
        assert_eq!(report.polls, 0);
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn polling_reaps_only_explicit_launch_children_and_propagates_enforcement_errors() {
        let control = FakeControl::default();
        control.add(10, 100, 1, Vec::new(), false);
        control.affinity_failures.borrow_mut().insert(10);
        let mut supervisor = supervisor(control, true);
        supervisor.child_pids.extend([10, 11]);
        let error = supervisor.poll_once().unwrap_err();
        assert_eq!(*supervisor.control.reaped.borrow(), [10, 11]);
        assert!(matches!(error, SupervisorError::InvalidPid { pid: 10 }));
    }
}
