use std::error::Error;
use std::path::PathBuf;

use clap::Args;
use ecore_launcher::{topology::format_cpu_list, CpuTopology, CpuTopologyDetector};

#[derive(Debug, Args)]
pub struct TopologyArgs {
    /// Emit stable, machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Alternate /sys/devices/system/cpu root for diagnostics and fixtures.
    #[arg(long, value_name = "PATH", default_value = "/sys/devices/system/cpu")]
    sysfs_root: PathBuf,
}

pub fn run(arguments: &TopologyArgs) -> Result<(), Box<dyn Error>> {
    let topology = CpuTopologyDetector::new(&arguments.sysfs_root).detect()?;
    if arguments.json {
        println!("{}", serde_json::to_string_pretty(&topology)?);
    } else {
        print_human_report(&topology);
    }
    Ok(())
}

fn print_human_report(topology: &CpuTopology) {
    println!("CPU topology\n");
    println!("Classification: {}", topology.classification);
    println!("Confidence:     {:.2}", topology.confidence);
    println!(
        "Online CPUs:    {}",
        display_cpu_list(&topology.online_cpus)
    );
    println!(
        "P-core CPUs:    {}",
        display_cpu_list(&topology.performance_cpus)
    );
    println!(
        "E-core CPUs:    {}",
        display_cpu_list(&topology.efficiency_cpus)
    );
    println!("\nPhysical cores:");
    for core in &topology.physical_cores {
        let package = core
            .package_id
            .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
        let core_id = core
            .core_id
            .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
        let label = if core.logical_cpus.len() == 1 {
            "CPU"
        } else {
            "CPUs"
        };
        let logical = core
            .logical_cpus
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "  package {package}, core {core_id}: {label} {logical:<10} {}",
            core.core_class
        );
    }
    println!("\nEvidence:");
    for item in &topology.evidence {
        println!("  - {}", item.interpretation);
    }
}

fn display_cpu_list(cpus: &[u32]) -> String {
    if cpus.is_empty() {
        "none".to_owned()
    } else {
        format_cpu_list(cpus)
    }
}
