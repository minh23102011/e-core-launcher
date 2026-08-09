use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};

use clap::Args;
use ecore_launcher::{
    diagnose, resolve_config_path, DoctorOptions, DoctorReport, DoctorStatus, IntegrationPaths,
    SessionEnvironment,
};

use super::discover::discovery_options;

/// Read-only end-to-end diagnostics for core launch readiness.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Emit deterministic structured JSON.
    #[arg(long)]
    json: bool,

    /// Alternate CPU sysfs root for fixtures and diagnostics.
    #[arg(long, value_name = "PATH", default_value = "/sys/devices/system/cpu")]
    sysfs_root: PathBuf,

    /// Alternate procfs root for fixtures and diagnostics.
    #[arg(long, value_name = "PATH", default_value = "/proc")]
    proc_root: PathBuf,

    /// Override XDG_CONFIG_HOME without creating it.
    #[arg(long, value_name = "PATH")]
    config_home: Option<PathBuf>,

    /// Override an XDG system config root inspected read-only for autostart.
    #[arg(long, value_name = "PATH", action = clap::ArgAction::Append)]
    config_dir: Vec<PathBuf>,

    /// Override XDG_STATE_HOME for read-only startup ownership diagnostics.
    #[arg(long, value_name = "PATH")]
    state_home: Option<PathBuf>,

    /// Direct systemctl executable used only for read-only user status.
    #[arg(long, value_name = "PATH", default_value = "systemctl")]
    systemctl: PathBuf,

    /// Ignore OnlyShowIn and NotShowIn desktop-environment filters.
    #[arg(long)]
    ignore_desktop_filter: bool,

    /// Replacement XDG user data root; its applications child is scanned.
    #[arg(long, value_name = "PATH")]
    data_home: Option<PathBuf>,

    /// Replacement XDG system data root; repeat to set precedence order.
    #[arg(long, value_name = "PATH", action = clap::ArgAction::Append)]
    data_dir: Vec<PathBuf>,
}

pub fn run(arguments: &DoctorArgs, config: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let discovery = discovery_options(
        true,
        arguments.ignore_desktop_filter,
        arguments.data_home.as_ref(),
        &arguments.data_dir,
    );
    let registry_path = make_absolute(resolve_config_path(config)?)?;
    let report = diagnose(&DoctorOptions {
        registry_path,
        discovery,
        sysfs_root: arguments.sysfs_root.clone(),
        proc_root: arguments.proc_root.clone(),
        integration_paths: IntegrationPaths::from_environment(
            arguments.config_home.as_deref(),
            &arguments.config_dir,
            arguments.state_home.as_deref(),
        )?,
        launcher_executable: std::env::current_exe()?,
        systemctl_executable: arguments.systemctl.clone(),
        session: SessionEnvironment::from_environment(),
    });
    if arguments.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report);
    }
    if report.has_errors() {
        Err(Box::new(io::Error::other(
            "doctor found one or more launch-blocking conditions",
        )))
    } else {
        Ok(())
    }
}

fn make_absolute(path: PathBuf) -> Result<PathBuf, io::Error> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn print_human(report: &DoctorReport) {
    println!("ecore-launcher doctor: {:?}\n", report.status);
    for check in &report.checks {
        let status = match check.status {
            DoctorStatus::Ok => "OK",
            DoctorStatus::Warning => "WARNING",
            DoctorStatus::Error => "ERROR",
        };
        println!("[{status}] {}: {}", check.id, check.summary);
        for (key, value) in &check.details {
            if !value.is_empty() {
                println!("  {key}: {value}");
            }
        }
    }
}
