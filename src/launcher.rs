//! Fail-closed launch planning and direct execution on detected E-cores.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, NulError, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use nix::sched::{sched_setaffinity, CpuSet};
use nix::unistd::{execv, Pid};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::discovery::DiscoveryReport;
use crate::registry::AppRegistry;
use crate::topology::{CpuTopology, TopologyClass};

/// A complete, validated request to launch explicitly managed applications.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaunchPlan {
    /// Applications in deterministic desktop-ID order.
    pub applications: Vec<PlannedApplication>,
    /// The exact sorted E-core CPU list detected for this plan.
    pub efficiency_cpus: Vec<u32>,
}

/// Current launch authority freshly resolved from a desktop entry.
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
}

/// One application whose helper process was successfully initiated.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InitiatedApplication {
    /// Stable desktop ID.
    pub desktop_id: String,
    /// Helper PID, which becomes the target PID after direct exec.
    pub pid: u32,
}

/// The immediate initiation result; it does not indicate application completion.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaunchReport {
    /// Successfully spawned helper/target processes in plan order.
    pub initiated: Vec<InitiatedApplication>,
}

/// Errors returned while making or executing a launch plan.
#[derive(Debug, Error)]
pub enum LauncherError {
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
    /// Applying the affinity set in the helper failed.
    #[error("failed to apply E-core affinity: {source}")]
    ApplyAffinity { source: nix::errno::Errno },
    /// Directly replacing the helper with the target failed.
    #[error("failed to exec target {executable}: {source}")]
    Exec {
        executable: PathBuf,
        source: nix::errno::Errno,
    },
    /// A helper could not be spawned after the listed earlier applications started.
    #[error("failed to initiate `{desktop_id}` after {} application(s): {source}", initiated.len())]
    SpawnFailed {
        desktop_id: String,
        initiated: Vec<InitiatedApplication>,
        #[source]
        source: io::Error,
    },
}

impl LauncherError {
    /// Applications initiated before an execution-time spawn failure.
    #[must_use]
    pub fn initiated(&self) -> &[InitiatedApplication] {
        match self {
            Self::SpawnFailed { initiated, .. } => initiated,
            _error => &[],
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
    let selected_ids = select_ids(registry, requested_ids)?;
    if selected_ids.is_empty() {
        return Ok(LaunchPlan {
            applications: Vec::new(),
            efficiency_cpus: Vec::new(),
        });
    }

    let current: BTreeMap<&str, _> = discovery
        .applications
        .iter()
        .map(|application| (application.desktop_id.as_str(), application))
        .collect();
    let mut applications = Vec::with_capacity(selected_ids.len());
    for desktop_id in selected_ids {
        let application = current.get(desktop_id.as_str()).ok_or_else(|| {
            LauncherError::UnavailableApplication {
                desktop_id: desktop_id.clone(),
            }
        })?;
        if application.terminal {
            return Err(LauncherError::TerminalApplication { desktop_id });
        }
        validate_exec_inputs(&application.executable, &application.arguments)?;
        applications.push(PlannedApplication {
            desktop_id,
            name: application.name.clone(),
            executable: application.executable.clone(),
            arguments: application.arguments.clone(),
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

/// Spawn one helper per application without waiting for GUI process completion.
pub fn execute_plan(plan: &LaunchPlan) -> Result<LaunchReport, LauncherError> {
    if plan.applications.is_empty() {
        return Ok(LaunchReport::default());
    }
    let launcher =
        std::env::current_exe().map_err(|source| LauncherError::CurrentExecutable { source })?;
    let mut report = LaunchReport::default();
    for application in &plan.applications {
        let mut command = Command::new(&launcher);
        command.arg("__exec");
        for cpu in &plan.efficiency_cpus {
            command.arg("--cpu").arg(cpu.to_string());
        }
        command
            .arg("--")
            .arg(&application.executable)
            .args(&application.arguments);
        let child = command
            .spawn()
            .map_err(|source| LauncherError::SpawnFailed {
                desktop_id: application.desktop_id.clone(),
                initiated: report.initiated.clone(),
                source,
            })?;
        report.initiated.push(InitiatedApplication {
            desktop_id: application.desktop_id.clone(),
            pid: child.id(),
        });
    }
    Ok(report)
}

/// Apply affinity to this helper and directly exec the target without a shell.
///
/// On success this function never returns because the process image is replaced.
pub fn exec_with_affinity(
    cpus: &[u32],
    executable: &Path,
    arguments: &[OsString],
) -> Result<(), LauncherError> {
    let cpu_set = build_cpu_set(cpus)?;
    sched_setaffinity(Pid::this(), &cpu_set)
        .map_err(|source| LauncherError::ApplyAffinity { source })?;
    let executable = cstring_from_os(executable.as_os_str(), "executable")?;
    let mut exec_arguments = Vec::with_capacity(arguments.len() + 1);
    exec_arguments.push(executable.clone());
    for argument in arguments {
        exec_arguments.push(cstring_from_os(argument, "argument")?);
    }
    match execv(&executable, &exec_arguments) {
        Err(source) => Err(LauncherError::Exec {
            executable: executable_path(&executable),
            source,
        }),
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

#[cfg(test)]
mod tests {
    use super::build_cpu_set;

    #[test]
    fn affinity_set_rejects_empty_and_out_of_range_cpu_ids() {
        assert!(build_cpu_set(&[]).is_err());
        assert!(build_cpu_set(&[u32::MAX]).is_err());
    }
}
