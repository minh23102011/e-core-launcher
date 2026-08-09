use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use ecore_launcher::{
    execute_plan, supervise_process_trees, LaunchPlan, LaunchReport, SupervisionReport,
    SupervisorOptions,
};
use serde::Serialize;

use super::run::{build_plan, LaunchSelectionArgs};

/// Launch enabled applications and supervise only opted-in process trees.
#[derive(Debug, Args)]
pub struct SuperviseArgs {
    /// Emit one deterministic JSON report after supervised trees have ended.
    #[arg(long)]
    json: bool,

    /// Alternate procfs root for diagnostics and synthetic tests.
    #[arg(long, value_name = "PATH", default_value = "/proc")]
    proc_root: PathBuf,

    /// Process-tree polling cadence; values below 10ms are rejected.
    #[arg(long, value_name = "MILLISECONDS", default_value_t = 1_000)]
    poll_interval_ms: u64,

    #[command(flatten)]
    launch: LaunchSelectionArgs,
}

#[derive(Debug, Serialize)]
struct SuperviseOutput {
    plan: LaunchPlan,
    launch: LaunchReport,
    supervision: SupervisionReport,
}

pub fn run(arguments: &SuperviseArgs, config: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let plan = build_plan(&arguments.launch, config)?;
    let launch = match execute_plan(&plan) {
        Ok(report) => report,
        Err(error) => {
            let report = error.launch_report().cloned().unwrap_or_default();
            if !arguments.json {
                print_launches(&report);
            }
            let output = SuperviseOutput {
                plan,
                launch: report,
                supervision: SupervisionReport::default(),
            };
            emit(&output, arguments.json)?;
            return Err(Box::new(error));
        }
    };
    if !arguments.json {
        print_launches(&launch);
    }
    let supervision = supervise_process_trees(
        &plan,
        &launch,
        &SupervisorOptions {
            proc_root: arguments.proc_root.clone(),
            poll_interval: Duration::from_millis(arguments.poll_interval_ms),
        },
    )?;
    let output = SuperviseOutput {
        plan,
        launch,
        supervision,
    };
    emit(&output, arguments.json)?;
    Ok(())
}

fn print_launches(report: &LaunchReport) {
    if report.initiated.is_empty() {
        println!("No enabled registered applications to supervise.");
        return;
    }
    for initiated in &report.initiated {
        println!(
            "Exec succeeded for {} (PID {}).",
            initiated.desktop_id, initiated.pid
        );
    }
}

fn emit(output: &SuperviseOutput, json: bool) -> Result<(), Box<dyn Error>> {
    if json {
        println!("{}", serde_json::to_string_pretty(output)?);
    } else {
        if let Some(signal) = &output.supervision.termination_signal {
            println!(
                "Supervision stopped cleanly after {signal}; managed applications were not signalled."
            );
        } else if output.supervision.tracked_roots == 0 {
            println!("No enforce-enabled live process tree required monitoring.");
        } else {
            println!(
                "Supervision ended after {} poll(s); {} affinity update(s), {} root(s) completed.",
                output.supervision.polls,
                output.supervision.affinity_updates,
                output.supervision.completed_roots
            );
        }
        if let Some(failure) = &output.launch.failure {
            println!("Failed before exec: {failure}");
        }
        for warning in &output.supervision.warnings {
            eprintln!(
                "supervisor warning for {}: {}",
                warning.desktop_id, warning.reason
            );
        }
    }
    Ok(())
}
