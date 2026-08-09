use std::error::Error;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::Args;
use ecore_launcher::{
    build_launch_plan, exec_with_affinity, execute_plan, resolve_config_path, CpuTopologyDetector,
    LaunchPlan, LaunchReport, RegistryStore,
};
use serde::Serialize;

use super::discover::DiscoveryArgs;

/// Launch explicitly registered applications after all validation succeeds.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Print the complete validated plan without spawning any process.
    #[arg(long)]
    dry_run: bool,

    /// Emit stable, machine-readable plan and initiation data.
    #[arg(long)]
    json: bool,

    /// Registered desktop IDs to launch; omit to select every enabled entry.
    #[arg(value_name = "DESKTOP_ID")]
    desktop_ids: Vec<String>,

    /// Ignore OnlyShowIn and NotShowIn desktop-environment filters.
    #[arg(long)]
    ignore_desktop_filter: bool,

    /// Replacement XDG user data root; its applications child is scanned.
    #[arg(long, value_name = "PATH")]
    data_home: Option<PathBuf>,

    /// Replacement XDG system data root; repeat to set precedence order.
    #[arg(long, value_name = "PATH", action = clap::ArgAction::Append)]
    data_dir: Vec<PathBuf>,

    /// Alternate /sys/devices/system/cpu root for fixtures and diagnostics.
    #[arg(long, value_name = "PATH", default_value = "/sys/devices/system/cpu")]
    sysfs_root: PathBuf,
}

impl RunArgs {
    fn discovery(&self) -> DiscoveryArgs {
        DiscoveryArgs {
            all: true,
            ignore_desktop_filter: self.ignore_desktop_filter,
            data_home: self.data_home.clone(),
            data_dir: self.data_dir.clone(),
        }
    }
}

/// Hidden helper arguments. The `--` separator preserves target argument boundaries.
#[derive(Debug, Args)]
pub struct HelperArgs {
    /// E-core logical CPU ID; supplied once for each CPU by the parent launcher.
    #[arg(long = "cpu", required = true)]
    cpus: Vec<u32>,

    /// Direct target executable.
    #[arg(value_name = "EXECUTABLE")]
    executable: PathBuf,

    /// Direct target arguments, including values beginning with a hyphen.
    #[arg(
        value_name = "ARGUMENT",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    arguments: Vec<OsString>,
}

#[derive(Debug, Serialize)]
struct RunOutput {
    dry_run: bool,
    plan: LaunchPlan,
    report: LaunchReport,
}

pub fn run(arguments: &RunArgs, config: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let store = RegistryStore::new(resolve_config_path(config)?);
    let registry = store.load()?;
    let requested = &arguments.desktop_ids;
    let selected_any = if requested.is_empty() {
        registry.apps.iter().any(|application| application.enabled)
    } else {
        true
    };
    let discovery = if selected_any {
        Some(
            ecore_launcher::DesktopApplicationScanner::from_options(
                arguments.discovery().options(),
            )
            .discover()?,
        )
    } else {
        None
    };
    let topology = if selected_any {
        Some(CpuTopologyDetector::new(&arguments.sysfs_root).detect()?)
    } else {
        None
    };
    let empty_discovery = ecore_launcher::DiscoveryReport::default();
    let discovery = match discovery.as_ref() {
        Some(report) => report,
        None => &empty_discovery,
    };
    let plan = build_launch_plan(&registry, discovery, topology.as_ref(), requested)?;
    let report = if arguments.dry_run {
        LaunchReport::default()
    } else {
        match execute_plan(&plan) {
            Ok(report) => report,
            Err(error) => {
                let output = RunOutput {
                    dry_run: false,
                    plan,
                    report: LaunchReport {
                        initiated: error.initiated().to_vec(),
                    },
                };
                emit(&output, arguments.json)?;
                return Err(Box::new(error));
            }
        }
    };
    let output = RunOutput {
        dry_run: arguments.dry_run,
        plan,
        report,
    };
    emit(&output, arguments.json)?;
    Ok(())
}

pub fn run_helper(arguments: &HelperArgs) -> Result<(), Box<dyn Error>> {
    exec_with_affinity(&arguments.cpus, &arguments.executable, &arguments.arguments)?;
    Ok(())
}

fn print_human(output: &RunOutput) {
    if output.plan.applications.is_empty() {
        println!("No enabled registered applications to launch.");
        return;
    }
    if output.dry_run {
        println!("Validated launch plan (dry run):");
        for application in &output.plan.applications {
            println!(
                "  {}: {}",
                application.desktop_id,
                application.executable.display()
            );
        }
        println!("E-core CPUs: {:?}", output.plan.efficiency_cpus);
        return;
    }
    for initiated in &output.report.initiated {
        println!(
            "Initiated {} (PID {}).",
            initiated.desktop_id, initiated.pid
        );
    }
}

fn emit(output: &RunOutput, json: bool) -> Result<(), Box<dyn Error>> {
    if json {
        println!("{}", serde_json::to_string_pretty(output)?);
    } else {
        print_human(output);
    }
    Ok(())
}
