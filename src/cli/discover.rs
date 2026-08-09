use std::error::Error;
use std::path::PathBuf;

use clap::Args;
use ecore_launcher::{
    DesktopApplicationScanner, DiscoveredApplication, DiscoveryOptions, DiscoveryReport,
};

#[derive(Clone, Debug, Args)]
pub(crate) struct DiscoveryArgs {
    /// Include NoDisplay=true applications; Hidden=true overrides stay suppressed.
    #[arg(long)]
    pub(crate) all: bool,

    /// Ignore OnlyShowIn and NotShowIn desktop-environment filters.
    #[arg(long)]
    pub(crate) ignore_desktop_filter: bool,

    /// Replacement XDG user data root; its applications child is scanned.
    #[arg(long, value_name = "PATH")]
    pub(crate) data_home: Option<PathBuf>,

    /// Replacement XDG system data root; repeat to set precedence order.
    #[arg(long, value_name = "PATH", action = clap::ArgAction::Append)]
    pub(crate) data_dir: Vec<PathBuf>,
}

impl DiscoveryArgs {
    pub(crate) fn options(&self) -> DiscoveryOptions {
        discovery_options(
            self.all,
            self.ignore_desktop_filter,
            self.data_home.as_ref(),
            &self.data_dir,
        )
    }
}

pub(crate) fn discovery_options(
    include_no_display: bool,
    ignore_desktop_filter: bool,
    data_home: Option<&PathBuf>,
    data_dirs: &[PathBuf],
) -> DiscoveryOptions {
    let mut options = DiscoveryOptions::from_environment();
    let explicit_paths = data_home.is_some() || !data_dirs.is_empty();
    if explicit_paths {
        options.data_home = data_home.cloned();
        options.data_dirs = data_dirs.to_vec();
        options.require_existing_roots = true;
    }
    options.include_no_display = include_no_display;
    options.ignore_desktop_filter = ignore_desktop_filter;
    options
}

#[derive(Debug, Args)]
pub struct DiscoverArgs {
    /// Emit stable, machine-readable JSON including warnings.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    discovery: DiscoveryArgs,
}

pub fn run(arguments: &DiscoverArgs) -> Result<(), Box<dyn Error>> {
    let report =
        DesktopApplicationScanner::from_options(arguments.discovery.options()).discover()?;
    if arguments.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human_report(&report);
    }
    Ok(())
}

fn print_human_report(report: &DiscoveryReport) {
    println!("Installed applications\n");
    if report.applications.is_empty() {
        println!("No usable desktop applications were found.");
    } else {
        let id_width = report
            .applications
            .iter()
            .map(|application| application.desktop_id.len())
            .max()
            .unwrap_or(2)
            .max(2);
        let name_width = report
            .applications
            .iter()
            .map(|application| application.name.len())
            .max()
            .unwrap_or(11)
            .max(11);
        println!(
            "{:<id_width$}  {:<name_width$}  Executable",
            "ID", "Application"
        );
        for application in &report.applications {
            print_application(application, id_width, name_width);
        }
    }

    println!(
        "\nFound {} application{}.",
        report.applications.len(),
        plural(report.applications.len())
    );
    let skipped = report
        .warnings
        .iter()
        .filter(|warning| warning.skipped)
        .count();
    println!(
        "Skipped {skipped} invalid, unavailable, filtered, overridden, or duplicate entr{}.",
        if skipped == 1 { "y" } else { "ies" }
    );
}

fn print_application(application: &DiscoveredApplication, id_width: usize, name_width: usize) {
    println!(
        "{:<id_width$}  {:<name_width$}  {}",
        application.desktop_id,
        application.name,
        application.executable.display()
    );
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::plural;

    #[test]
    fn pluralizes_application_count() {
        assert_eq!(plural(1), "");
        assert_eq!(plural(0), "s");
        assert_eq!(plural(2), "s");
    }
}
